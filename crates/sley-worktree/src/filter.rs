//! Content filtering on the blob<->worktree boundary: CRLF/encoding/ident/process filters, clean/smudge, and `ls-files --eol` info.
//!
//! Split out of `lib.rs` in the wave-47 mechanical refactor: a pure code move
//! (no function body changed); all items are re-exported from `lib.rs`.
use super::*;
use crate::attributes::*;
use crate::ignore::*;
use crate::index::*;
use crate::index_io::*;
use crate::types_admin::*;

// ---------------------------------------------------------------------------
// Content filtering on the blob <-> worktree boundary
//
// Git runs two kinds of conversion when content crosses between the worktree
// and the object database:
//
//   * the line-ending / `core.autocrlf` conversion (driven by the `text`,
//     `eol` attributes and the `core.autocrlf` / `core.eol` config), and
//   * the long-running `filter.<name>.clean` / `.smudge` driver filters
//     (selected by the `filter=<name>` attribute and configured commands).
//
// "clean" runs on the way *into* the object store (worktree -> blob), e.g. on
// `git add` / `git hash-object -w`. "smudge" runs on the way *out* (blob ->
// worktree), e.g. on checkout / restore. The driver filter, when present,
// wraps the EOL conversion: on clean git first runs the configured `clean`
// command and then applies CRLF->LF normalization; on smudge git first applies
// LF->CRLF and then runs the `smudge` command.
// ---------------------------------------------------------------------------

/// The line-ending conversion that applies to a path, derived from its
/// attributes and the repository config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EolConversion {
    /// No conversion: binary content, or text with `core.autocrlf=false` and no
    /// `eol`/`text=auto` request to add carriage returns.
    None,
    /// Normalize to LF on clean; no carriage returns on smudge (`eol=lf`, or
    /// `core.autocrlf=input`).
    Lf,
    /// Normalize to LF on clean; emit CRLF on smudge (`eol=crlf`, or
    /// `core.autocrlf=true`).
    Crlf,
}

/// How git should decide whether a path is text for the purpose of EOL
/// conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TextDecision {
    /// `-text` / `binary`: never convert.
    Binary,
    /// `text` is set explicitly: always treat as text.
    Text,
    /// `text=auto` (or implied by `core.autocrlf`): treat as text unless the
    /// content looks binary.
    Auto,
    /// No opinion from attributes or config: leave content untouched.
    Unspecified,
}

/// The fully resolved set of conversions that apply to a single path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ContentFilterPlan {
    pub(crate) text: TextDecision,
    /// The conversion to apply when `text` resolves to "this is text".
    pub(crate) eol: EolConversion,
    /// Whether `$Id$` keyword collapse/expansion applies to this path.
    pub(crate) ident: bool,
    /// `filter.<name>` driver, if assigned via attributes and configured.
    pub(crate) driver: Option<FilterDriver>,
    /// `working-tree-encoding` attribute: the worktree charset to decode from
    /// on checkin / encode to on checkout.
    pub(crate) encoding: WtEncoding,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FilterDriver {
    name: Vec<u8>,
    process: Option<String>,
    clean: Option<String>,
    smudge: Option<String>,
    required: bool,
}

/// The resolved `working-tree-encoding` attribute (convert.c
/// `git_path_check_encoding`): an unset / empty / `UTF-8` value means no
/// conversion; `working-tree-encoding` (true) / `working-tree-encoding=false`
/// are rejected; any other value names a charset the worktree file is stored in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WtEncoding {
    /// No reencoding (unset, empty, or the default UTF-8).
    None,
    /// `working-tree-encoding` set as a boolean — `true`/`false` are invalid.
    Invalid,
    /// A named encoding (the original attribute value, preserved for messages).
    Named(Vec<u8>),
}

impl WtEncoding {
    fn from_attr(state: Option<&AttributeState>) -> WtEncoding {
        match state {
            // Unset (`-working-tree-encoding`) or no attribute: nothing to do.
            None | Some(AttributeState::Unset) => WtEncoding::None,
            // `working-tree-encoding` with no value is the boolean true.
            Some(AttributeState::Set) => WtEncoding::Invalid,
            Some(AttributeState::Value(value)) => {
                // An empty value (`working-tree-encoding=`) or UTF-8 (the
                // in-repo default) needs no conversion.
                if value.is_empty() || encoding_name_is_utf8(value) {
                    WtEncoding::None
                } else {
                    WtEncoding::Named(value.clone())
                }
            }
        }
    }
}

/// Whether `name` denotes UTF-8 (`same_encoding(name, "UTF-8")`): the leading
/// `UTF` prefix and an optional `-` are skipped, then the remainder compared
/// case-insensitively against `8`.
pub(crate) fn encoding_name_is_utf8(name: &[u8]) -> bool {
    utf_suffix(name).is_some_and(|suffix| suffix == "8")
}

/// Strip a leading case-insensitive `UTF` and optional `-`, returning the
/// uppercased remainder (e.g. `utf-16le` → `16LE`, `UTF-16LE-BOM` →
/// `16LE-BOM`). `None` when `name` is not a UTF family encoding or not UTF-8.
pub(crate) fn utf_suffix(name: &[u8]) -> Option<String> {
    let upper: String = std::str::from_utf8(name).ok()?.to_ascii_uppercase();
    let rest = upper.strip_prefix("UTF")?;
    Some(rest.strip_prefix('-').unwrap_or(rest).to_string())
}

#[derive(Clone, Copy)]
pub(crate) enum BomProblem {
    Prohibited,
    Required,
}

/// The byte order mark validation from convert.c `validate_encoding`: an
/// explicit-endianness UTF encoding must not start with a BOM, while a
/// byte-order-agnostic one (`UTF-16` / `UTF-32`) must.
pub(crate) fn utf_bom_problem(suffix: &str, data: &[u8]) -> Option<BomProblem> {
    let has16 = data.starts_with(&[0xFF, 0xFE]) || data.starts_with(&[0xFE, 0xFF]);
    let has32 = data.starts_with(&[0xFF, 0xFE, 0, 0]) || data.starts_with(&[0, 0, 0xFE, 0xFF]);
    match suffix {
        "16LE" | "16BE" => has16.then_some(BomProblem::Prohibited),
        "32LE" | "32BE" => has32.then_some(BomProblem::Prohibited),
        "16" => (!has16).then_some(BomProblem::Required),
        "32" => (!has32).then_some(BomProblem::Required),
        _ => None,
    }
}

/// The byte order used by the platform iconv for byte-order-agnostic
/// `UTF-16` / `UTF-32` when no BOM says otherwise. Git delegates these names to
/// iconv; glibc follows host endian, while macOS emits big-endian with a BOM.
#[cfg(target_os = "macos")]
pub(crate) const ICONV_UTF_DEFAULT_LE: bool = false;
#[cfg(not(target_os = "macos"))]
pub(crate) const ICONV_UTF_DEFAULT_LE: bool = cfg!(target_endian = "little");

/// Decode worktree-encoded bytes to UTF-8 (encode_to_git's reencode), or `None`
/// when the encoding is unsupported or the bytes are not valid in it.
pub(crate) fn decode_to_utf8(suffix: &str, data: &[u8]) -> Option<Vec<u8>> {
    match suffix {
        "16LE" => decode_utf16(data, true),
        "16BE" => decode_utf16(data, false),
        "16" | "16LE-BOM" | "16BE-BOM" => {
            let (le, body) = strip_utf16_bom(data);
            decode_utf16(body, le)
        }
        "32LE" => decode_utf32(data, true),
        "32BE" => decode_utf32(data, false),
        "32" | "32LE-BOM" | "32BE-BOM" => {
            let (le, body) = strip_utf32_bom(data);
            decode_utf32(body, le)
        }
        _ => None,
    }
}

/// Encode UTF-8 bytes to the worktree encoding (encode_to_worktree's reencode),
/// or `None` when the encoding is unsupported or the input is not valid UTF-8.
pub(crate) fn encode_from_utf8(suffix: &str, utf8: &[u8]) -> Option<Vec<u8>> {
    match suffix {
        "16LE" => encode_utf16(utf8, true, false),
        "16BE" => encode_utf16(utf8, false, false),
        "16LE-BOM" => encode_utf16(utf8, true, true),
        "16BE-BOM" => encode_utf16(utf8, false, true),
        "16" => encode_utf16(utf8, ICONV_UTF_DEFAULT_LE, true),
        "32LE" => encode_utf32(utf8, true, false),
        "32BE" => encode_utf32(utf8, false, false),
        "32LE-BOM" => encode_utf32(utf8, true, true),
        "32BE-BOM" => encode_utf32(utf8, false, true),
        "32" => encode_utf32(utf8, ICONV_UTF_DEFAULT_LE, true),
        _ => None,
    }
}

pub(crate) fn decode_named_encoding_to_utf8(name: &[u8], data: &[u8]) -> Option<Vec<u8>> {
    if let Some(suffix) = utf_suffix(name) {
        return decode_to_utf8(&suffix, data);
    }
    let encoding = encoding_rs::Encoding::for_label(name)?;
    let (decoded, _, had_errors) = encoding.decode(data);
    (!had_errors).then(|| decoded.into_owned().into_bytes())
}

pub(crate) fn encode_utf8_to_named_encoding(name: &[u8], utf8: &[u8]) -> Option<Vec<u8>> {
    if let Some(suffix) = utf_suffix(name) {
        return encode_from_utf8(&suffix, utf8);
    }
    let text = std::str::from_utf8(utf8).ok()?;
    let encoding = encoding_rs::Encoding::for_label(name)?;
    let (encoded, _, had_errors) = encoding.encode(text);
    (!had_errors).then(|| encoded.into_owned())
}

pub(crate) fn should_check_roundtrip_encoding(config: &GitConfig, name: &[u8]) -> bool {
    match config.get("core", None, "checkRoundtripEncoding") {
        Some(value) => value
            .as_bytes()
            .split(|byte| *byte == b',' || byte.is_ascii_whitespace())
            .filter(|part| !part.is_empty())
            .any(|part| same_encoding_name(part, name)),
        None => same_encoding_name(b"SHIFT-JIS", name),
    }
}

pub(crate) fn same_encoding_name(left: &[u8], right: &[u8]) -> bool {
    match (utf_suffix(left), utf_suffix(right)) {
        (Some(left), Some(right)) => left == right,
        _ => left.eq_ignore_ascii_case(right),
    }
}

pub(crate) fn trace_roundtrip_encoding_check(name: &[u8]) {
    if std::env::var_os("GIT_TRACE_WORKING_TREE_ENCODING").is_none()
        && std::env::var_os("GIT_TRACE").is_none()
    {
        return;
    }
    eprintln!(
        "Checking roundtrip encoding for {}",
        String::from_utf8_lossy(name)
    );
}

pub(crate) fn strip_utf16_bom(data: &[u8]) -> (bool, &[u8]) {
    if data.starts_with(&[0xFF, 0xFE]) {
        (true, &data[2..])
    } else if data.starts_with(&[0xFE, 0xFF]) {
        (false, &data[2..])
    } else {
        (ICONV_UTF_DEFAULT_LE, data)
    }
}

pub(crate) fn strip_utf32_bom(data: &[u8]) -> (bool, &[u8]) {
    if data.starts_with(&[0xFF, 0xFE, 0, 0]) {
        (true, &data[4..])
    } else if data.starts_with(&[0, 0, 0xFE, 0xFF]) {
        (false, &data[4..])
    } else {
        (ICONV_UTF_DEFAULT_LE, data)
    }
}

pub(crate) fn decode_utf16(data: &[u8], le: bool) -> Option<Vec<u8>> {
    if !data.len().is_multiple_of(2) {
        return None;
    }
    let units = data.chunks_exact(2).map(|chunk| {
        let pair = [chunk[0], chunk[1]];
        if le {
            u16::from_le_bytes(pair)
        } else {
            u16::from_be_bytes(pair)
        }
    });
    let mut out = String::new();
    for unit in char::decode_utf16(units) {
        out.push(unit.ok()?);
    }
    Some(out.into_bytes())
}

pub(crate) fn decode_utf32(data: &[u8], le: bool) -> Option<Vec<u8>> {
    if !data.len().is_multiple_of(4) {
        return None;
    }
    let mut out = String::new();
    for chunk in data.chunks_exact(4) {
        let quad = [chunk[0], chunk[1], chunk[2], chunk[3]];
        let cp = if le {
            u32::from_le_bytes(quad)
        } else {
            u32::from_be_bytes(quad)
        };
        out.push(char::from_u32(cp)?);
    }
    Some(out.into_bytes())
}

pub(crate) fn encode_utf16(utf8: &[u8], le: bool, bom: bool) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(utf8).ok()?;
    let mut out = Vec::with_capacity(utf8.len() * 2 + 2);
    if bom {
        out.extend_from_slice(if le { &[0xFF, 0xFE] } else { &[0xFE, 0xFF] });
    }
    for unit in text.encode_utf16() {
        out.extend_from_slice(&if le {
            unit.to_le_bytes()
        } else {
            unit.to_be_bytes()
        });
    }
    Some(out)
}

pub(crate) fn encode_utf32(utf8: &[u8], le: bool, bom: bool) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(utf8).ok()?;
    let mut out = Vec::with_capacity(utf8.len() * 4 + 4);
    if bom {
        out.extend_from_slice(if le {
            &[0xFF, 0xFE, 0, 0]
        } else {
            &[0, 0, 0xFE, 0xFF]
        });
    }
    for ch in text.chars() {
        let cp = ch as u32;
        out.extend_from_slice(&if le {
            cp.to_le_bytes()
        } else {
            cp.to_be_bytes()
        });
    }
    Some(out)
}

/// Reject a `working-tree-encoding` boolean (`true`/`false`) before any
/// conversion runs — `git_path_check_encoding` dies on it regardless of
/// direction.
pub(crate) fn check_wt_encoding_valid(encoding: &WtEncoding) -> Result<()> {
    if matches!(encoding, WtEncoding::Invalid) {
        eprintln!("fatal: true/false are no valid working-tree-encodings");
        return Err(GitError::Exit(128));
    }
    Ok(())
}

pub(crate) fn clean_encoding_needs_stat_match_validation(
    config: &GitConfig,
    attributes: &[AttributeCheck],
) -> bool {
    let plan = ContentFilterPlan::resolve(config, attributes);
    matches!(plan.encoding, WtEncoding::Invalid | WtEncoding::Named(_))
}

pub(crate) fn validate_clean_encoding_for_stat_match(
    config: &GitConfig,
    attributes: &[AttributeCheck],
    path: &[u8],
    content: &[u8],
) -> Result<()> {
    let plan = ContentFilterPlan::resolve(config, attributes);
    check_wt_encoding_valid(&plan.encoding)?;
    let _ = encode_to_git(config, &plan.encoding, path, Cow::Borrowed(content), false)?;
    Ok(())
}

/// encode_to_git: decode worktree-encoded `data` to UTF-8 for storage in the
/// object database. Runs the BOM validation first (fatal when writing an
/// object). Returns the borrowed input unchanged when there is no encoding.
pub(crate) fn encode_to_git<'a>(
    config: &GitConfig,
    encoding: &WtEncoding,
    path: &[u8],
    data: Cow<'a, [u8]>,
    write_object: bool,
) -> Result<Cow<'a, [u8]>> {
    let name = match encoding {
        WtEncoding::None => return Ok(data),
        WtEncoding::Invalid => return check_wt_encoding_valid(encoding).map(|()| data),
        WtEncoding::Named(name) => name,
    };
    if data.is_empty() {
        return Ok(data);
    }
    let display = String::from_utf8_lossy(path);
    let enc = String::from_utf8_lossy(name);
    if let Some(suffix) = utf_suffix(name)
        && let Some(problem) = utf_bom_problem(&suffix, &data)
    {
        let number = &suffix[..2.min(suffix.len())];
        match problem {
            BomProblem::Prohibited => {
                eprintln!(
                    "hint: The file '{display}' contains a byte order mark (BOM). \
Please use UTF-{number} as working-tree-encoding."
                );
                report_encode_failure(
                    write_object,
                    &format!("BOM is prohibited in '{display}' if encoded as {enc}"),
                )?;
                return Ok(data);
            }
            BomProblem::Required => {
                eprintln!(
                    "hint: The file '{display}' is missing a byte order mark (BOM). \
Please use UTF-{number}BE or UTF-{number}LE (depending on the byte order) as \
working-tree-encoding."
                );
                report_encode_failure(
                    write_object,
                    &format!("BOM is required in '{display}' if encoded as {enc}"),
                )?;
                return Ok(data);
            }
        }
    }
    match decode_named_encoding_to_utf8(name, &data) {
        Some(utf8) => {
            if should_check_roundtrip_encoding(config, name) {
                trace_roundtrip_encoding_check(name);
                if encode_utf8_to_named_encoding(name, &utf8).as_deref() != Some(data.as_ref()) {
                    report_encode_failure(
                        write_object,
                        &format!("encoding round trip failed for '{display}' from {enc} to UTF-8"),
                    )?;
                    return Ok(data);
                }
            }
            Ok(Cow::Owned(utf8))
        }
        None => {
            report_encode_failure(
                write_object,
                &format!("failed to encode '{display}' from {enc} to UTF-8"),
            )?;
            Ok(data)
        }
    }
}

/// encode_to_worktree: reencode UTF-8 `data` to the worktree encoding on
/// checkout. A failure is reported (never fatal) and the content left as-is,
/// matching convert.c `encode_to_worktree`.
pub(crate) fn encode_to_worktree<'a>(
    encoding: &WtEncoding,
    path: &[u8],
    data: Cow<'a, [u8]>,
) -> Result<Cow<'a, [u8]>> {
    let name = match encoding {
        WtEncoding::None => return Ok(data),
        WtEncoding::Invalid => return check_wt_encoding_valid(encoding).map(|()| data),
        WtEncoding::Named(name) => name,
    };
    if data.is_empty() {
        return Ok(data);
    }
    match encode_utf8_to_named_encoding(name, &data) {
        Some(encoded) => Ok(Cow::Owned(encoded)),
        None => {
            let display = String::from_utf8_lossy(path);
            let enc = String::from_utf8_lossy(name);
            eprintln!("error: failed to encode '{display}' from UTF-8 to {enc}");
            Ok(data)
        }
    }
}

/// Emit a clean-side encoding failure: fatal (`die`) when writing an object,
/// otherwise an `error:` diagnostic that lets the caller keep the content as-is.
pub(crate) fn report_encode_failure(write_object: bool, message: &str) -> Result<()> {
    if write_object {
        eprintln!("fatal: {message}");
        Err(GitError::Exit(128))
    } else {
        eprintln!("error: {message}");
        Ok(())
    }
}

/// Decode one crlf-family attribute (`text` or its legacy alias `crlf`) into a
/// text decision, plus whether the value form forced an EOL direction.
///
/// Mirrors git's `git_path_check_crlf` (convert.c): a *set* attribute is text,
/// an *unset* one is binary, `=auto` is auto, `=input` forces LF while still
/// counting as text, and any other value is "undefined" — i.e. no opinion, so
/// the caller falls through to the next source (the `crlf` alias, then config).
pub(crate) fn decode_crlf_family_attribute(
    state: Option<&AttributeState>,
) -> (TextDecision, EolConversion) {
    match state {
        Some(AttributeState::Set) => (TextDecision::Text, EolConversion::None),
        Some(AttributeState::Unset) => (TextDecision::Binary, EolConversion::None),
        Some(AttributeState::Value(value)) if value == b"auto" => {
            (TextDecision::Auto, EolConversion::None)
        }
        // `crlf=input` / `text=input`: text content normalized to LF (no CR on
        // smudge), exactly like `core.autocrlf=input`.
        Some(AttributeState::Value(value)) if value == b"input" => {
            (TextDecision::Text, EolConversion::Lf)
        }
        // `=<other>` is CRLF_UNDEFINED in git for the `crlf` alias: no opinion.
        _ => (TextDecision::Unspecified, EolConversion::None),
    }
}

impl ContentFilterPlan {
    /// Build the plan for `path` from the parsed attributes and repo config.
    fn resolve(config: &GitConfig, checks: &[AttributeCheck]) -> Self {
        let text_attr = checks.iter().find(|check| check.attribute == b"text");
        let crlf_attr = checks.iter().find(|check| check.attribute == b"crlf");
        let ident_attr = checks.iter().find(|check| check.attribute == b"ident");
        let eol_attr = checks.iter().find(|check| check.attribute == b"eol");
        let filter_attr = checks.iter().find(|check| check.attribute == b"filter");
        let encoding_attr = checks
            .iter()
            .find(|check| check.attribute == b"working-tree-encoding");
        let encoding = WtEncoding::from_attr(encoding_attr.and_then(|check| check.state.as_ref()));

        // Resolve the eol attribute first; `eol=crlf|lf` also forces text.
        let eol_value = eol_attr.and_then(|check| match &check.state {
            Some(AttributeState::Value(value)) => Some(value.clone()),
            _ => None,
        });

        // The `text` attribute decides first; only when it is unspecified does
        // git consult the legacy `crlf` alias (convert.c `convert_attrs`).
        let mut forced_eol = EolConversion::None;
        let mut text = match text_attr.map(|check| &check.state) {
            Some(Some(AttributeState::Set)) => TextDecision::Text,
            Some(Some(AttributeState::Unset)) => TextDecision::Binary,
            Some(Some(AttributeState::Value(value))) if value == b"auto" => TextDecision::Auto,
            Some(Some(AttributeState::Value(value))) if value == b"input" => {
                forced_eol = EolConversion::Lf;
                TextDecision::Text
            }
            // `text=<other>` is treated by git as a set text attribute.
            Some(Some(AttributeState::Value(_))) => TextDecision::Text,
            // `!text` (unspecified) or no text attribute: fall through to `crlf`.
            _ => {
                let (decision, eol) =
                    decode_crlf_family_attribute(crlf_attr.and_then(|check| check.state.as_ref()));
                forced_eol = eol;
                decision
            }
        };

        // A concrete `eol` attribute implies the path is text even when `text`
        // was left unspecified (git: `eol` without `text` is treated as
        // `text=auto`-ish; upstream forces conversion). We honour eol only when
        // text is not explicitly binary.
        let eol = match (&text, eol_value.as_deref()) {
            (TextDecision::Binary, _) => EolConversion::None,
            (_, Some(b"crlf")) => {
                if text == TextDecision::Unspecified {
                    text = TextDecision::Text;
                }
                EolConversion::Crlf
            }
            (_, Some(b"lf")) => {
                if text == TextDecision::Unspecified {
                    text = TextDecision::Text;
                }
                EolConversion::Lf
            }
            // No explicit `eol` attribute, but `text=input`/`crlf=input` already
            // forced the LF direction (git's CRLF_TEXT_INPUT). Honour it over the
            // config-derived default.
            _ if forced_eol == EolConversion::Lf => EolConversion::Lf,
            // No eol attribute: derive direction from config.
            _ => eol_from_config(config),
        };

        // When the path is text but neither `eol` nor `core.autocrlf`/`core.eol`
        // asked for carriage returns, we still normalize to LF on clean. That is
        // modelled by `EolConversion::Lf` (clean strips CR, smudge adds none).
        let eol = match (&text, eol) {
            (TextDecision::Text | TextDecision::Auto, EolConversion::None) => EolConversion::Lf,
            (_, eol) => eol,
        };

        // If config does not enable autocrlf and there is no eol/text opinion,
        // there is genuinely nothing to do.
        let text = match (text, eol_attr.is_some()) {
            (TextDecision::Unspecified, _) => {
                // Without any text/eol attribute, only `core.autocrlf` can make a
                // path eligible, and then it behaves like `text=auto`.
                if autocrlf_enabled(config) {
                    TextDecision::Auto
                } else {
                    TextDecision::Unspecified
                }
            }
            (text, _) => text,
        };

        let driver = resolve_filter_driver(config, filter_attr);
        let ident = matches!(
            ident_attr.and_then(|check| check.state.as_ref()),
            Some(AttributeState::Set)
        );

        ContentFilterPlan {
            text,
            eol,
            ident,
            driver,
            encoding,
        }
    }

    /// Whether EOL conversion should run for the given content.
    fn convert_eol(&self, content: &[u8]) -> bool {
        match self.text {
            TextDecision::Binary | TextDecision::Unspecified => false,
            TextDecision::Text => self.eol != EolConversion::None,
            // `text=auto`: only when the blob does not look binary.
            TextDecision::Auto => self.eol != EolConversion::None && !looks_binary(content),
        }
    }

    /// The smudge-side LF->CRLF safety check, mirroring convert.c
    /// `will_convert_lf_to_crlf`. Returns false (no conversion) when:
    ///   * there is no naked LF to convert, or
    ///   * the action is `text=auto`-derived (the "new safer autocrlf") AND the
    ///     content already contains a lone CR or a CRLF pair, or looks binary.
    ///
    /// An explicit `text`/`eol=crlf` (non-auto) path always converts naked LFs.
    pub(crate) fn will_convert_lf_to_crlf(&self, content: &[u8]) -> bool {
        self.will_convert_lf_to_crlf_stats(&gather_convert_stats(content))
    }

    /// Stats-based variant of [`will_convert_lf_to_crlf`], mirroring convert.c
    /// `will_convert_lf_to_crlf(struct text_stat *, ...)`. Used by the safecrlf
    /// round-trip simulation, which mutates a copy of the stats rather than
    /// re-scanning the buffer.
    fn will_convert_lf_to_crlf_stats(&self, stats: &ConvertStats) -> bool {
        // `output_eol(crlf_action) != EOL_CRLF` short-circuits in git.
        if self.eol != EolConversion::Crlf {
            return false;
        }
        // No naked LF? Nothing to convert.
        if stats.lonelf == 0 {
            return false;
        }
        if self.text == TextDecision::Auto {
            // Any CR or CRLF already present: leave it untouched (irreversible).
            if stats.lonecr > 0 || stats.crlf > 0 {
                return false;
            }
            if convert_is_binary(stats) {
                return false;
            }
        }
        true
    }

    /// Whether this path is a candidate for the `core.safecrlf` round-trip check
    /// at all: git only warns for non-`CRLF_BINARY` actions. `Binary` and
    /// `Unspecified` (with autocrlf off) correspond to git's `CRLF_BINARY`.
    fn safecrlf_applies(&self) -> bool {
        matches!(self.text, TextDecision::Text | TextDecision::Auto)
    }

    /// Emit git's `core.safecrlf` round-trip warning for `path`, mirroring the
    /// stderr side-effect of convert.c `crlf_to_git` (the `CONV_EOL_RNDTRP_*`
    /// branch). `old_stats` are the stats of the *pre-conversion* worktree
    /// content (already gathered by the caller so the buffer is scanned once);
    /// `index_has_crlf` is whether the path's current index blob already has a
    /// CRLF (git's `has_crlf_in_index`, used only for the auto-crlf decision).
    ///
    /// This never inspects or alters the bytes written to the object store; it is
    /// purely the additive warning git prints alongside `git add`/`commit`.
    /// Returns `Err` only under `core.safecrlf=true` when the round-trip is
    /// irreversible (git `die`s).
    fn check_safe_crlf_stats(
        &self,
        old_stats: &ConvertStats,
        index_has_crlf: bool,
        flags: ConvFlags,
        path: &[u8],
    ) -> Result<()> {
        if flags == ConvFlags::Off || !self.safecrlf_applies() {
            return Ok(());
        }

        // Replicate `crlf_to_git`'s `convert_crlf_into_lf` decision (the clean
        // direction). It starts as "there is a CRLF to collapse"; auto paths
        // suppress conversion for binary content or content whose index blob
        // already carries a CRLF (the "new safer autocrlf").
        let mut convert_crlf_into_lf = old_stats.crlf > 0;
        if self.text == TextDecision::Auto {
            if convert_is_binary(old_stats) {
                // git returns 0 here: no conversion *and* no warning.
                return Ok(());
            }
            if index_has_crlf {
                convert_crlf_into_lf = false;
            }
        }

        // Simulate the round-trip on a copy of the stats.
        let mut new_stats = old_stats.clone();
        // Simulate "git add" (clean: CRLF -> LF).
        if convert_crlf_into_lf {
            new_stats.lonelf += new_stats.crlf;
            new_stats.crlf = 0;
        }
        // Simulate "git checkout" (smudge: LF -> CRLF).
        if self.will_convert_lf_to_crlf_stats(&new_stats) {
            new_stats.crlf += new_stats.lonelf;
            new_stats.lonelf = 0;
        }
        check_safe_crlf(old_stats, &new_stats, flags, path)
    }
}

/// Derive the smudge-direction line ending from `core.autocrlf` / `core.eol`.
pub(crate) fn eol_from_config(config: &GitConfig) -> EolConversion {
    if let Some(value) = config.get("core", None, "autocrlf") {
        match value.to_ascii_lowercase().as_str() {
            "input" => return EolConversion::Lf,
            "true" | "yes" | "on" | "1" => return EolConversion::Crlf,
            _ => {}
        }
    }
    if config.get_bool("core", None, "autocrlf") == Some(true) {
        return EolConversion::Crlf;
    }
    match config
        .get("core", None, "eol")
        .map(|v| v.to_ascii_lowercase())
    {
        Some(ref v) if v == "crlf" => EolConversion::Crlf,
        Some(ref v) if v == "lf" => EolConversion::Lf,
        _ => EolConversion::None,
    }
}

/// Whether `core.autocrlf` is set to anything that enables conversion
/// (`true` or `input`).
pub(crate) fn autocrlf_enabled(config: &GitConfig) -> bool {
    if let Some(value) = config.get("core", None, "autocrlf")
        && value.eq_ignore_ascii_case("input")
    {
        return true;
    }
    config.get_bool("core", None, "autocrlf") == Some(true)
}

/// Resolve the `filter=<name>` attribute against `filter.<name>.*` config.
pub(crate) fn resolve_filter_driver(
    config: &GitConfig,
    filter_attr: Option<&AttributeCheck>,
) -> Option<FilterDriver> {
    let name = match filter_attr.map(|check| &check.state) {
        Some(Some(AttributeState::Value(value))) => value.clone(),
        // `filter` set/unset without a value selects no driver.
        _ => return None,
    };
    let subsection = String::from_utf8_lossy(&name).into_owned();
    let process = filter_config_value(config, &subsection, "process").filter(|cmd| !cmd.is_empty());
    let clean = filter_config_value(config, &subsection, "clean").filter(|cmd| !cmd.is_empty());
    let smudge = filter_config_value(config, &subsection, "smudge").filter(|cmd| !cmd.is_empty());
    let required = filter_config_bool(config, &subsection, "required").unwrap_or(false);
    // A filter with neither command and not required is a no-op.
    if process.is_none() && clean.is_none() && smudge.is_none() && !required {
        return None;
    }
    Some(FilterDriver {
        name,
        process,
        clean,
        smudge,
        required,
    })
}

pub(crate) fn filter_config_value(
    config: &GitConfig,
    subsection: &str,
    key: &str,
) -> Option<String> {
    config
        .get("filter", Some(subsection), key)
        .map(str::to_owned)
        .or_else(|| global_filter_config_value(subsection, key))
}

pub(crate) fn filter_config_bool(config: &GitConfig, subsection: &str, key: &str) -> Option<bool> {
    config
        .get_bool("filter", Some(subsection), key)
        .or_else(|| {
            global_filter_config_value(subsection, key)
                .as_deref()
                .and_then(sley_config::parse_config_bool)
        })
}

pub(crate) fn global_filter_config_value(subsection: &str, key: &str) -> Option<String> {
    for (path, _) in sley_config::default_config_layer_paths().into_iter().rev() {
        let Ok(config) = GitConfig::read(path) else {
            continue;
        };
        if let Some(value) = config.get("filter", Some(subsection), key) {
            return Some(value.to_owned());
        }
    }
    None
}

/// Heuristic mirroring git's `buffer_is_binary`: content is treated as binary
/// when a NUL byte appears within the first 8000 bytes.
pub(crate) fn looks_binary(content: &[u8]) -> bool {
    const FIRST_FEW_BYTES: usize = 8000;
    let window = &content[..content.len().min(FIRST_FEW_BYTES)];
    window.contains(&0)
}

/// Strip carriage returns that immediately precede a line feed (CRLF -> LF).
/// A lone CR (old-Mac line ending) is left untouched, matching git, which only
/// collapses CRLF pairs.
pub(crate) fn convert_crlf_to_lf_cow(content: Cow<'_, [u8]>) -> Cow<'_, [u8]> {
    if !content.windows(2).any(|window| window == b"\r\n") {
        return content;
    }
    let mut out = Vec::with_capacity(content.len());
    let mut index = 0;
    while index < content.len() {
        let byte = content[index];
        if byte == b'\r' && content.get(index + 1) == Some(&b'\n') {
            // Drop the CR; the LF is emitted on the next iteration.
            index += 1;
            continue;
        }
        out.push(byte);
        index += 1;
    }
    Cow::Owned(out)
}

/// Convert lone LF bytes to CRLF (LF -> CRLF). An LF already preceded by a CR
/// is left as-is so content is not double-converted, matching git.
pub(crate) fn convert_lf_to_crlf(content: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(content.len() + content.len() / 16);
    let mut prev = 0u8;
    for &byte in content {
        if byte == b'\n' && prev != b'\r' {
            out.push(b'\r');
        }
        out.push(byte);
        prev = byte;
    }
    out
}

/// Collapse git `$Id: ... $` keywords to `$Id$` on the clean path.
pub(crate) fn ident_to_git_cow(content: Cow<'_, [u8]>) -> Cow<'_, [u8]> {
    let input = content.as_ref();
    if !has_git_ident(input) {
        return content;
    }
    let mut out = Vec::with_capacity(input.len());
    let mut pos = 0;
    while let Some(relative) = input[pos..].iter().position(|byte| *byte == b'$') {
        let dollar = pos + relative;
        out.extend_from_slice(&input[pos..=dollar]);
        pos = dollar + 1;
        if input.len().saturating_sub(pos) > 3 && input[pos..].starts_with(b"Id:") {
            let search = &input[pos + 3..];
            let Some(end_relative) = search.iter().position(|byte| *byte == b'$') else {
                break;
            };
            let end = pos + 3 + end_relative;
            if input[pos + 3..end].contains(&b'\n') {
                continue;
            }
            out.extend_from_slice(b"Id$");
            pos = end + 1;
        }
    }
    out.extend_from_slice(&input[pos..]);
    Cow::Owned(out)
}

/// Expand `$Id$` and git-style `$Id: <hex> $` keywords using the blob id of the
/// unexpanded content, matching convert.c's ident_to_worktree.
pub(crate) fn ident_to_worktree_cow(
    format: ObjectFormat,
    content: Cow<'_, [u8]>,
) -> Result<Cow<'_, [u8]>> {
    let input = content.as_ref();
    if !has_git_ident(input) {
        return Ok(content);
    }
    let oid = EncodedObject::new(ObjectType::Blob, input.to_vec()).object_id(format)?;
    let replacement = format!("Id: {} $", oid.to_hex());
    let mut out = Vec::with_capacity(input.len() + replacement.len());
    let mut pos = 0;
    while let Some(relative) = input[pos..].iter().position(|byte| *byte == b'$') {
        let dollar = pos + relative;
        out.extend_from_slice(&input[pos..=dollar]);
        pos = dollar + 1;
        if input.len().saturating_sub(pos) < 3 || !input[pos..].starts_with(b"Id") {
            continue;
        }
        match input.get(pos + 2) {
            Some(b'$') => {
                pos += 3;
            }
            Some(b':') => {
                let search = &input[pos + 3..];
                let Some(end_relative) = search.iter().position(|byte| *byte == b'$') else {
                    break;
                };
                let end = pos + 3 + end_relative;
                if input[pos + 3..end].contains(&b'\n') || is_foreign_ident(&input[pos + 3..end]) {
                    continue;
                }
                pos = end + 1;
            }
            _ => continue,
        }
        out.extend_from_slice(replacement.as_bytes());
    }
    out.extend_from_slice(&input[pos..]);
    Ok(Cow::Owned(out))
}

pub(crate) fn has_git_ident(content: &[u8]) -> bool {
    let mut pos = 0;
    while let Some(relative) = content[pos..].iter().position(|byte| *byte == b'$') {
        let start = pos + relative + 1;
        if content.len().saturating_sub(start) < 3 {
            break;
        }
        if !content[start..].starts_with(b"Id") {
            pos = start;
            continue;
        }
        match content.get(start + 2) {
            Some(b'$') => return true,
            Some(b':') => {
                let search = &content[start + 3..];
                let Some(end_relative) = search.iter().position(|byte| *byte == b'$') else {
                    break;
                };
                let end = start + 3 + end_relative;
                if !content[start + 3..end].contains(&b'\n') {
                    return true;
                }
                pos = end + 1;
            }
            _ => pos = start,
        }
    }
    false
}

pub(crate) fn is_foreign_ident(expansion: &[u8]) -> bool {
    if expansion.len() <= 1 {
        return false;
    }
    expansion[1..expansion.len().saturating_sub(1)].contains(&b' ')
}

/// Run a configured `clean`/`smudge` command as a subprocess, feeding `content`
/// on stdin and returning its stdout. Errors carry enough context for the
/// caller to decide whether the failure is fatal (required filter) or should be
/// silently ignored (optional filter passthrough).
pub(crate) fn run_filter_command(command: &str, path: &[u8], content: &[u8]) -> Result<Vec<u8>> {
    // Git expands `%f` in the filter command to the path of the file being
    // filtered (quoted). We perform the same substitution.
    let display_path = String::from_utf8_lossy(path);
    let expanded = command.replace("%f", &shell_quote(&display_path));
    // Run through the platform shell so pipelines / arguments in the configured
    // command behave the same way git's `run_command`-with-shell does.
    let (shell, flag) = if cfg!(windows) {
        ("cmd", "/C")
    } else {
        ("/bin/sh", "-c")
    };
    let mut child = Command::new(shell)
        .arg(flag)
        .arg(&expanded)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| GitError::Command(format!("failed to spawn filter `{command}`: {err}")))?;
    // Write the content to the child's stdin on a separate thread so we never
    // deadlock against a filter that streams output before consuming all input.
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| GitError::Command(format!("filter `{command}` stdin unavailable")))?;
    let payload = content.to_vec();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&payload);
        // Dropping `stdin` here closes the pipe so the child sees EOF.
    });
    let output = child
        .wait_with_output()
        .map_err(|err| GitError::Command(format!("filter `{command}` failed: {err}")))?;
    // Join the writer; its own errors (e.g. broken pipe) are non-fatal because
    // the child's exit status is the authoritative signal.
    let _ = writer.join();
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(GitError::Command(format!(
            "filter `{command}` exited with {}: {}",
            output.status,
            stderr.trim()
        )));
    }
    Ok(output.stdout)
}

pub(crate) const PROCESS_CAP_CLEAN: u8 = 1;
pub(crate) const PROCESS_CAP_SMUDGE: u8 = 1 << 1;
pub(crate) const PROCESS_CAP_DELAY: u8 = 1 << 2;
pub(crate) const PKT_DATA_MAX: usize = 65_516;

pub(crate) static PROCESS_FILTERS: OnceLock<Mutex<HashMap<String, ProcessFilter>>> =
    OnceLock::new();
pub(crate) static PROCESS_FILTER_ABORTED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
/// Extra protocol metadata sent on smudge requests when the caller knows the
/// tree-ish/ref context of the materialization.
pub type ProcessFilterMetadata = Vec<(String, String)>;
pub(crate) static PROCESS_FILTER_METADATA: OnceLock<Mutex<Option<ProcessFilterMetadata>>> =
    OnceLock::new();
pub(crate) static PROCESS_FILTER_CWD: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();

/// Restores the previous process-filter metadata when dropped.
pub struct ProcessFilterMetadataGuard {
    previous: Option<ProcessFilterMetadata>,
}

impl Drop for ProcessFilterMetadataGuard {
    fn drop(&mut self) {
        if let Ok(mut guard) = PROCESS_FILTER_METADATA
            .get_or_init(|| Mutex::new(None))
            .lock()
        {
            *guard = self.previous.take();
        }
    }
}

pub fn set_process_filter_metadata(
    metadata: Option<ProcessFilterMetadata>,
) -> ProcessFilterMetadataGuard {
    let mutex = PROCESS_FILTER_METADATA.get_or_init(|| Mutex::new(None));
    let previous = mutex
        .lock()
        .map(|mut guard| std::mem::replace(&mut *guard, metadata))
        .unwrap_or(None);
    ProcessFilterMetadataGuard { previous }
}

/// Restores the previous process-filter working directory when dropped.
pub struct ProcessFilterCwdGuard {
    previous: Option<PathBuf>,
}

impl Drop for ProcessFilterCwdGuard {
    fn drop(&mut self) {
        if let Ok(mut guard) = PROCESS_FILTER_CWD.get_or_init(|| Mutex::new(None)).lock() {
            *guard = self.previous.take();
        }
    }
}

pub fn set_process_filter_cwd(cwd: Option<PathBuf>) -> ProcessFilterCwdGuard {
    let mutex = PROCESS_FILTER_CWD.get_or_init(|| Mutex::new(None));
    let previous = mutex
        .lock()
        .map(|mut guard| std::mem::replace(&mut *guard, cwd))
        .unwrap_or(None);
    ProcessFilterCwdGuard { previous }
}

pub(crate) fn current_process_filter_metadata() -> Option<ProcessFilterMetadata> {
    PROCESS_FILTER_METADATA
        .get_or_init(|| Mutex::new(None))
        .lock()
        .ok()
        .and_then(|guard| guard.clone())
}

pub(crate) fn current_process_filter_cwd() -> Option<PathBuf> {
    PROCESS_FILTER_CWD
        .get_or_init(|| Mutex::new(None))
        .lock()
        .ok()
        .and_then(|guard| guard.clone())
}

pub(crate) struct ProcessFilter {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: ChildStdout,
    capabilities: u8,
    closed: bool,
}

pub(crate) enum ProcessFilterOutcome {
    Filtered(Vec<u8>),
    Unsupported,
    Status(String),
}

pub(crate) enum DriverFilterResult<'a> {
    Content(Cow<'a, [u8]>),
    Delayed { process: String },
}

pub(crate) enum SmudgeFilterResult<'a> {
    Content(Cow<'a, [u8]>),
    Delayed { process: String },
}

pub(crate) struct ProcessFilterFailure {
    pub(crate) message: String,
    pub(crate) protocol: bool,
}

impl ProcessFilterFailure {
    fn protocol(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            protocol: true,
        }
    }
}

pub(crate) fn run_process_filter(
    command: &str,
    direction: &str,
    path: &[u8],
    content: &[u8],
    blob: Option<ObjectId>,
    can_delay: bool,
) -> std::result::Result<ProcessFilterOutcome, ProcessFilterFailure> {
    if PROCESS_FILTER_ABORTED
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .map(|aborted| aborted.contains(command))
        .unwrap_or(false)
    {
        return Ok(ProcessFilterOutcome::Status("abort".to_string()));
    }
    let filters = PROCESS_FILTERS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut filters = filters
        .lock()
        .map_err(|_| ProcessFilterFailure::protocol("process filter cache poisoned"))?;
    if !filters.contains_key(command) {
        let filter = ProcessFilter::start(command)?;
        filters.insert(command.to_string(), filter);
    }
    let Some(filter) = filters.get_mut(command) else {
        return Err(ProcessFilterFailure::protocol(
            "process filter missing after insert",
        ));
    };
    let result = filter.apply(direction, path, content, blob, can_delay);
    if matches!(result, Ok(ProcessFilterOutcome::Status(ref status)) if status == "abort") {
        if let Some(mut filter) = filters.remove(command) {
            filter.finish_gracefully();
        }
        if let Ok(mut aborted) = PROCESS_FILTER_ABORTED
            .get_or_init(|| Mutex::new(HashSet::new()))
            .lock()
        {
            aborted.insert(command.to_string());
        }
    }
    if result.as_ref().is_err_and(|err| err.protocol) {
        filters.remove(command);
    }
    result
}

pub(crate) fn list_available_process_filter_blobs(
    command: &str,
) -> std::result::Result<Vec<Vec<u8>>, ProcessFilterFailure> {
    let filters = PROCESS_FILTERS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut filters = filters
        .lock()
        .map_err(|_| ProcessFilterFailure::protocol("process filter cache poisoned"))?;
    let Some(filter) = filters.get_mut(command) else {
        return Err(ProcessFilterFailure::protocol(format!(
            "external filter '{command}' is not available anymore although not all paths have been filtered"
        )));
    };
    let result = filter.list_available_blobs();
    if result.as_ref().is_err_and(|err| err.protocol) {
        filters.remove(command);
    }
    result
}

impl ProcessFilter {
    fn start(command: &str) -> std::result::Result<Self, ProcessFilterFailure> {
        let (shell, flag) = if cfg!(windows) {
            ("cmd", "/C")
        } else {
            ("/bin/sh", "-c")
        };
        let mut process = Command::new(shell);
        process
            .arg(flag)
            .arg(command)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        if let Some(cwd) = current_process_filter_cwd() {
            process.current_dir(cwd);
        }
        let mut child = process.spawn().map_err(|err| {
            ProcessFilterFailure::protocol(format!(
                "cannot fork to run subprocess '{command}': {err}"
            ))
        })?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| ProcessFilterFailure::protocol("process filter stdin unavailable"))?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| ProcessFilterFailure::protocol("process filter stdout unavailable"))?;

        write_pkt_text(&mut stdin, "git-filter-client\n")?;
        write_pkt_text(&mut stdin, "version=2\n")?;
        write_flush(&mut stdin)?;

        let line = read_pkt_text(&mut stdout)?.ok_or_else(|| {
            ProcessFilterFailure::protocol(
                "Unexpected line '<flush packet>', expected git-filter-server",
            )
        })?;
        if line != "git-filter-server" {
            return Err(ProcessFilterFailure::protocol(format!(
                "Unexpected line '{line}', expected git-filter-server"
            )));
        }
        let line = read_pkt_text(&mut stdout)?.ok_or_else(|| {
            ProcessFilterFailure::protocol("Unexpected line '<flush packet>', expected version")
        })?;
        if line != "version=2" {
            return Err(ProcessFilterFailure::protocol(format!(
                "Unexpected line '{line}', expected version"
            )));
        }
        if let Some(line) = read_pkt_text(&mut stdout)? {
            return Err(ProcessFilterFailure::protocol(format!(
                "Unexpected line '{line}', expected flush"
            )));
        }

        write_pkt_text(&mut stdin, "capability=clean\n")?;
        write_pkt_text(&mut stdin, "capability=smudge\n")?;
        write_pkt_text(&mut stdin, "capability=delay\n")?;
        write_flush(&mut stdin)?;

        let mut capabilities = 0;
        while let Some(line) = read_pkt_text(&mut stdout)? {
            match line.as_str() {
                "capability=clean" => capabilities |= PROCESS_CAP_CLEAN,
                "capability=smudge" => capabilities |= PROCESS_CAP_SMUDGE,
                "capability=delay" => capabilities |= PROCESS_CAP_DELAY,
                _ => {}
            }
        }

        Ok(Self {
            child,
            stdin: Some(stdin),
            stdout,
            capabilities,
            closed: false,
        })
    }

    fn apply(
        &mut self,
        direction: &str,
        path: &[u8],
        content: &[u8],
        blob: Option<ObjectId>,
        can_delay: bool,
    ) -> std::result::Result<ProcessFilterOutcome, ProcessFilterFailure> {
        let wanted = match direction {
            "clean" => PROCESS_CAP_CLEAN,
            "smudge" => PROCESS_CAP_SMUDGE,
            _ => 0,
        };
        if self.capabilities & wanted == 0 {
            return Ok(ProcessFilterOutcome::Unsupported);
        }

        let can_delay =
            can_delay && direction == "smudge" && self.capabilities & PROCESS_CAP_DELAY != 0;

        {
            let stdin = self
                .stdin
                .as_mut()
                .ok_or_else(|| ProcessFilterFailure::protocol("process filter stdin closed"))?;
            write_pkt_text(stdin, &format!("command={direction}\n"))?;
            write_pkt_text(
                stdin,
                &format!("pathname={}\n", String::from_utf8_lossy(path)),
            )?;
            if direction == "smudge"
                && let Some(blob) = blob
            {
                if let Some(metadata) = current_process_filter_metadata() {
                    for (key, value) in metadata {
                        write_pkt_text(stdin, &format!("{key}={value}\n"))?;
                    }
                }
                write_pkt_text(stdin, &format!("blob={}\n", blob.to_hex()))?;
            }
            if can_delay {
                write_pkt_text(stdin, "can-delay=1\n")?;
            }
            write_flush(stdin)?;
            write_pkt_content(stdin, content)?;
            write_flush(stdin)?;
        }

        let mut status = read_process_status(&mut self.stdout)?.unwrap_or_default();
        match status.as_str() {
            "success" => {}
            "error" | "abort" | "delayed" => return Ok(ProcessFilterOutcome::Status(status)),
            other => {
                return Err(ProcessFilterFailure::protocol(format!(
                    "external filter returned unsupported status '{other}'"
                )));
            }
        }

        let output = read_pkt_content(&mut self.stdout)?;
        if let Some(next) = read_process_status(&mut self.stdout)? {
            status = next;
        }
        match status.as_str() {
            "" | "success" => Ok(ProcessFilterOutcome::Filtered(output)),
            "error" | "abort" | "delayed" => Ok(ProcessFilterOutcome::Status(status)),
            other => Err(ProcessFilterFailure::protocol(format!(
                "external filter returned unsupported status '{other}'"
            ))),
        }
    }

    fn list_available_blobs(&mut self) -> std::result::Result<Vec<Vec<u8>>, ProcessFilterFailure> {
        if self.capabilities & PROCESS_CAP_DELAY == 0 {
            return Ok(Vec::new());
        }
        {
            let stdin = self
                .stdin
                .as_mut()
                .ok_or_else(|| ProcessFilterFailure::protocol("process filter stdin closed"))?;
            write_pkt_text(stdin, "command=list_available_blobs\n")?;
            write_flush(stdin)?;
        }

        let mut paths = Vec::new();
        while let Some(line) = read_pkt_text(&mut self.stdout)? {
            if let Some(path) = line.strip_prefix("pathname=") {
                paths.push(path.as_bytes().to_vec());
            }
        }
        let status = read_process_status(&mut self.stdout)?.unwrap_or_default();
        match status.as_str() {
            "" | "success" => Ok(paths),
            other => Err(ProcessFilterFailure::protocol(format!(
                "external filter returned unsupported status '{other}'"
            ))),
        }
    }

    fn finish_gracefully(&mut self) {
        self.stdin.take();
        for _ in 0..10 {
            match self.child.try_wait() {
                Ok(Some(_)) => {
                    self.closed = true;
                    return;
                }
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(10)),
                Err(_) => {
                    self.closed = true;
                    return;
                }
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.closed = true;
    }
}

impl Drop for ProcessFilter {
    fn drop(&mut self) {
        if self.closed {
            return;
        }
        if let Some(stdin) = self.stdin.as_mut() {
            let _ = stdin.flush();
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub(crate) fn write_pkt_text(
    writer: &mut ChildStdin,
    text: &str,
) -> std::result::Result<(), ProcessFilterFailure> {
    write_pkt_data(writer, text.as_bytes())
}

pub(crate) fn write_pkt_content(
    writer: &mut ChildStdin,
    content: &[u8],
) -> std::result::Result<(), ProcessFilterFailure> {
    for chunk in content.chunks(PKT_DATA_MAX) {
        write_pkt_data(writer, chunk)?;
    }
    Ok(())
}

pub(crate) fn write_pkt_data(
    writer: &mut ChildStdin,
    data: &[u8],
) -> std::result::Result<(), ProcessFilterFailure> {
    let len = data.len() + 4;
    write!(writer, "{len:04x}")
        .and_then(|_| writer.write_all(data))
        .map_err(|err| {
            ProcessFilterFailure::protocol(format!("process filter write failed: {err}"))
        })
}

pub(crate) fn write_flush(
    writer: &mut ChildStdin,
) -> std::result::Result<(), ProcessFilterFailure> {
    writer
        .write_all(b"0000")
        .and_then(|_| writer.flush())
        .map_err(|err| {
            ProcessFilterFailure::protocol(format!("process filter write failed: {err}"))
        })
}

pub(crate) fn read_pkt_text(
    reader: &mut ChildStdout,
) -> std::result::Result<Option<String>, ProcessFilterFailure> {
    let Some(mut data) = read_pkt_data(reader)? else {
        return Ok(None);
    };
    if data.last() == Some(&b'\n') {
        data.pop();
    }
    Ok(Some(String::from_utf8_lossy(&data).into_owned()))
}

pub(crate) fn read_pkt_content(
    reader: &mut ChildStdout,
) -> std::result::Result<Vec<u8>, ProcessFilterFailure> {
    let mut out = Vec::new();
    while let Some(data) = read_pkt_data(reader)? {
        out.extend_from_slice(&data);
    }
    Ok(out)
}

pub(crate) fn read_pkt_data(
    reader: &mut ChildStdout,
) -> std::result::Result<Option<Vec<u8>>, ProcessFilterFailure> {
    let mut header = [0u8; 4];
    reader.read_exact(&mut header).map_err(|err| {
        ProcessFilterFailure::protocol(format!("process filter read failed: {err}"))
    })?;
    let header = std::str::from_utf8(&header)
        .map_err(|err| ProcessFilterFailure::protocol(format!("invalid pkt-line header: {err}")))?;
    let len = usize::from_str_radix(header, 16)
        .map_err(|err| ProcessFilterFailure::protocol(format!("invalid pkt-line length: {err}")))?;
    if len == 0 {
        return Ok(None);
    }
    if len < 4 {
        return Err(ProcessFilterFailure::protocol(format!(
            "invalid pkt-line length {len}"
        )));
    }
    let mut data = vec![0; len - 4];
    reader.read_exact(&mut data).map_err(|err| {
        ProcessFilterFailure::protocol(format!("process filter read failed: {err}"))
    })?;
    Ok(Some(data))
}

pub(crate) fn read_process_status(
    reader: &mut ChildStdout,
) -> std::result::Result<Option<String>, ProcessFilterFailure> {
    let mut status = None;
    while let Some(line) = read_pkt_text(reader)? {
        if let Some(value) = line.strip_prefix("status=") {
            status = Some(value.to_string());
        }
    }
    Ok(status)
}

/// Minimal POSIX single-quote escaping for substituting `%f` into a shell
/// command (used only for the path passed to driver filters).
pub(crate) fn shell_quote(value: &str) -> String {
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

/// Apply the *clean* conversion to `content` for `path` (worktree -> blob):
/// first the configured `filter.<name>.clean` driver (if any), then CRLF->LF
/// normalization when EOL conversion applies.
///
/// `config` is the repository config (`GitConfig`) and `path` is the
/// repository-relative path of the file (forward-slash separated, e.g.
/// `src/main.rs`). When no filter or EOL conversion applies the input is
/// returned unchanged.
///
/// A *required* driver (`filter.<name>.required=true`) whose `clean` command is
/// missing or fails produces a [`GitError::Command`]; a non-required driver
/// failure (or absence of a `clean` command) passes the content through
/// unfiltered, matching git.
pub fn apply_clean_filter(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    config: &GitConfig,
    path: &[u8],
    content: &[u8],
) -> Result<Vec<u8>> {
    // On clean the worktree file exists, so the live `.gitattributes` chain is
    // authoritative. `git_dir` is accepted for symmetry with the smudge entry
    // point (which falls back to the index) and for future use.
    let _ = git_dir.as_ref();
    let checks = filter_attribute_checks(worktree_root.as_ref(), path)?;
    apply_clean_filter_with_attributes(config, &checks, path, content)
}

/// Like [`apply_clean_filter`] but with git's `CONV_EOL_KEEP_CRLF`: clean driver
/// and ident still run, but CRLF→LF conversion is suppressed. Used by
/// `git apply` when the patch's preimage context already contains CRLF
/// (`crlf_in_old`), so worktree content is matched byte-for-byte against the
/// patch lines rather than renormalized under `text=auto`.
pub fn apply_clean_filter_keep_crlf(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    config: &GitConfig,
    path: &[u8],
    content: &[u8],
) -> Result<Vec<u8>> {
    let _ = git_dir.as_ref();
    let checks = filter_attribute_checks(worktree_root.as_ref(), path)?;
    Ok(apply_clean_filter_cow_inner(
        config,
        &checks,
        path,
        content,
        ConvFlags::Off,
        SafeCrlfIndexBlob::None,
        false,
        true, // keep_crlf
    )?
    .into_owned())
}

/// A reusable handle that captures repository-wide attribute sources once so
/// repeated clean-filter calls (e.g. `hash-object --stdin-paths` hashing many
/// paths in one process) don't re-read config / global attributes per path.
///
/// Git only consults `.gitattributes` on the path's directory chain, not the
/// rest of the worktree. This handle matches that: construction reads
/// `core.attributesFile` (or the default global) once, then each path folds
/// only its ancestor `.gitattributes` plus
/// `$GIT_COMMON_DIR/info/attributes`. Matchers are cached per parent directory
/// so sibling files in the same folder reuse the folded chain.
///
/// Build it once with [`WorktreeAttributes::from_worktree_root`] (or
/// [`WorktreeAttributes::from_worktree_and_git_dir`]), then call
/// [`WorktreeAttributes::apply_clean_filter`] per path. This mirrors
/// [`apply_clean_filter`] except the expensive config parse is amortized.
pub struct WorktreeAttributes {
    worktree_root: PathBuf,
    common_git_dir: PathBuf,
    base: AttributeMatcher,
    dir_matchers: RefCell<HashMap<Vec<u8>, AttributeMatcher>>,
}

impl WorktreeAttributes {
    /// Read the worktree's global attribute sources. In-tree `.gitattributes`
    /// and `$GIT_COMMON_DIR/info/attributes` are folded on first use of each
    /// directory.
    pub fn from_worktree_root(worktree_root: impl AsRef<Path>) -> Result<Self> {
        let root = worktree_root.as_ref();
        Self::from_worktree_and_git_dir(root, root.join(".git"))
    }

    /// Like [`Self::from_worktree_root`] but starts from the repository's git
    /// directory (linked worktrees, `GIT_DIR`, etc.) instead of `root/.git`.
    /// The shared common directory is resolved once here, including
    /// `commondir` and `GIT_COMMON_DIR` overrides.
    pub fn from_worktree_and_git_dir(
        worktree_root: impl AsRef<Path>,
        git_dir: impl AsRef<Path>,
    ) -> Result<Self> {
        let worktree_root = worktree_root.as_ref();
        let git_dir = git_dir.as_ref();
        let common_git_dir = common_git_dir_for_git_dir(git_dir)?;
        let base = AttributeMatcher::from_global_sources(worktree_root, &common_git_dir);
        Ok(Self {
            worktree_root: worktree_root.to_path_buf(),
            common_git_dir,
            base,
            dir_matchers: RefCell::new(HashMap::new()),
        })
    }

    /// Construct from the command's already-loaded effective config. This is
    /// the preferred path for multi-file commands: attribute lookup and clean
    /// filtering share one config snapshot, including global settings,
    /// command-line overrides, and linked-worktree include context.
    pub fn from_worktree_and_git_dir_with_config(
        worktree_root: impl AsRef<Path>,
        git_dir: impl AsRef<Path>,
        config: &GitConfig,
    ) -> Result<Self> {
        let worktree_root = worktree_root.as_ref();
        let common_git_dir = common_git_dir_for_git_dir(git_dir.as_ref())?;
        Ok(Self {
            worktree_root: worktree_root.to_path_buf(),
            common_git_dir,
            base: AttributeMatcher::from_global_config(worktree_root, config),
            dir_matchers: RefCell::new(HashMap::new()),
        })
    }

    fn checks_for_path(&self, path: &[u8]) -> Result<Vec<AttributeCheck>> {
        let dir_key = attribute_parent_dir(path);
        if let Some(matcher) = self.dir_matchers.borrow().get(&dir_key) {
            return Ok(matcher.attributes_for_path(path, &filter_attribute_names(), false));
        }
        let mut matcher = self.base.clone();
        fold_in_tree_attribute_frames(&self.worktree_root, path, &mut matcher)?;
        fold_info_attributes(&self.common_git_dir, &mut matcher);
        let checks = matcher.attributes_for_path(path, &filter_attribute_names(), false);
        self.dir_matchers.borrow_mut().insert(dir_key, matcher);
        Ok(checks)
    }

    /// Apply the clean conversion to `content` for `path`, reusing the cached
    /// attribute sources. Behaviourally identical to [`apply_clean_filter`].
    pub fn apply_clean_filter(
        &self,
        config: &GitConfig,
        path: &[u8],
        content: &[u8],
    ) -> Result<Vec<u8>> {
        let checks = self.checks_for_path(path)?;
        apply_clean_filter_with_attributes(config, &checks, path, content)
    }

    /// Clean conversion that honours git's `has_crlf_in_index` safeguard for
    /// `text=auto` / `core.autocrlf`: when `index_blob` already contains CRLF,
    /// the worktree CRLF is left unconverted so a diff against that blob
    /// keeps matching line endings on both sides.
    pub fn apply_clean_filter_respecting_index(
        &self,
        config: &GitConfig,
        path: &[u8],
        content: &[u8],
        index_blob: SafeCrlfIndexBlob<'_>,
    ) -> Result<Vec<u8>> {
        let checks = self.checks_for_path(path)?;
        Ok(apply_clean_filter_with_attributes_cow_safecrlf(
            config,
            &checks,
            path,
            content,
            ConvFlags::Off,
            index_blob,
        )?
        .into_owned())
    }
}

fn attribute_parent_dir(path: &[u8]) -> Vec<u8> {
    match path.iter().rposition(|byte| *byte == b'/') {
        Some(index) => path[..index].to_vec(),
        None => Vec::new(),
    }
}

/// Fold `.gitattributes` from the worktree root down to `path`'s parent.
fn fold_in_tree_attribute_frames(
    worktree_root: &Path,
    path: &[u8],
    matcher: &mut AttributeMatcher,
) -> Result<()> {
    read_dir_attribute_patterns_for_base(worktree_root, &[], matcher)?;
    let mut prefix = Vec::new();
    let mut parts = path.split(|byte| *byte == b'/').peekable();
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            break;
        }
        if !prefix.is_empty() {
            prefix.push(b'/');
        }
        prefix.extend_from_slice(part);
        let dir = worktree_root.join(repo_path_to_os_path(&prefix)?);
        read_dir_attribute_patterns_for_base(&dir, &prefix, matcher)?;
    }
    Ok(())
}

fn fold_info_attributes(git_dir: &Path, matcher: &mut AttributeMatcher) {
    read_attribute_patterns(
        git_dir.join("info").join("attributes"),
        matcher,
        &[],
        b".git/info/attributes",
        false,
    );
}

/// A reusable handle that captures a *tree's* `.gitattributes` chain once so
/// repeated smudge-filter calls (e.g. `git archive` streaming every blob in a
/// tree) resolve attributes from the tree being processed rather than the live
/// worktree.
///
/// This is the attribute direction `git archive` uses: upstream unpacks the
/// archived tree into a scratch index and sets `GIT_ATTR_INDEX`, so the
/// `.gitattributes` that govern conversion come from the *archived tree* (plus
/// the global/`core.attributesFile` chain and `$GIT_DIR/info/attributes`), not
/// from whatever happens to be checked out. `--worktree-attributes` callers
/// should use [`WorktreeAttributes`] instead.
///
/// Build it once with [`TreeAttributes::from_tree`], then call
/// [`TreeAttributes::apply_smudge_filter`] per blob. Behaviourally this mirrors
/// [`apply_smudge_filter`] except the attribute source is the supplied tree and
/// the expensive source scan is amortized across calls.
pub struct TreeAttributes {
    matcher: AttributeMatcher,
}

impl TreeAttributes {
    /// Read the attribute sources for `tree_oid` once: the global /
    /// `core.attributesFile` chain, every `.gitattributes` blob found while
    /// walking `tree_oid`, and `$GIT_DIR/info/attributes`.
    ///
    /// `attr_root` locates the global config (`read_configured_attributes`);
    /// pass the worktree root for a non-bare repo, or the git dir for a bare
    /// one. `git_dir` locates `info/attributes` directly (so this works for bare
    /// repos, where there is no nested `.git`). No worktree `.gitattributes`
    /// files are read — use [`WorktreeAttributes`] for the
    /// `--worktree-attributes` direction.
    pub fn from_tree(
        attr_root: impl AsRef<Path>,
        git_dir: impl AsRef<Path>,
        db: &FileObjectDatabase,
        format: ObjectFormat,
        tree_oid: &ObjectId,
    ) -> Result<Self> {
        let attr_root = attr_root.as_ref();
        let git_dir = git_dir.as_ref();
        let mut matcher = AttributeMatcher::default();
        matcher.configure_case_sensitivity(git_dir);
        if !matcher.read_configured_attributes(attr_root, git_dir) {
            matcher.read_default_global_attributes();
        }
        collect_attribute_patterns_from_tree(db, format, tree_oid, Vec::new(), &mut matcher)?;
        read_attribute_patterns(
            git_dir.join("info").join("attributes"),
            &mut matcher,
            &[],
            b"info/attributes",
            false,
        );
        Ok(Self { matcher })
    }

    /// Apply the smudge conversion (blob -> worktree: EOL `LF`->`CRLF` plus any
    /// configured `filter.<name>.smudge` driver) to `content` for `path`,
    /// reusing the cached attribute chain. Behaviourally identical to
    /// [`apply_smudge_filter`] except attributes come from the tree this handle
    /// was built from.
    pub fn apply_smudge_filter(
        &self,
        config: &GitConfig,
        path: &[u8],
        content: &[u8],
    ) -> Result<Vec<u8>> {
        let checks = self
            .matcher
            .attributes_for_path(path, &filter_attribute_names(), false);
        apply_smudge_filter_with_attributes(config, &checks, path, content)
    }

    pub fn attributes_for_path(&self, path: &[u8], requested: &[Vec<u8>]) -> Vec<AttributeCheck> {
        self.matcher.attributes_for_path(path, requested, false)
    }

    /// True when `path` has the `export-subst` attribute set (git's
    /// `check_attr_export_subst`), meaning `git archive` should run
    /// `$Format:…$` keyword substitution on its content.
    pub fn export_subst_for_path(&self, path: &[u8]) -> bool {
        self.attribute_is_set(path, b"export-subst")
    }

    /// True when `path` has the `export-ignore` attribute set (git's
    /// `check_attr_export_ignore`), meaning `git archive` should omit the path
    /// (and, for a directory, its whole subtree) from the archive.
    pub fn export_ignore_for_path(&self, path: &[u8]) -> bool {
        self.attribute_is_set(path, b"export-ignore")
    }

    fn attribute_is_set(&self, path: &[u8], attribute: &[u8]) -> bool {
        let requested = [attribute.to_vec()];
        let checks = self.matcher.attributes_for_path(path, &requested, false);
        matches!(
            checks.first().and_then(|check| check.state.as_ref()),
            Some(AttributeState::Set)
        )
    }

    /// The `diff` attribute state for `path` (`Set` for `diff`, `Unset` for
    /// `-diff`, `Value(name)` for `diff=<name>`, `None` when unspecified). Used
    /// by `git archive`'s zip backend to classify text vs. binary via the
    /// path's userdiff driver.
    pub fn diff_attribute_for_path(&self, path: &[u8]) -> Option<AttributeState> {
        let requested = [b"diff".to_vec()];
        let checks = self.matcher.attributes_for_path(path, &requested, false);
        checks.into_iter().next().and_then(|check| check.state)
    }
}

/// Like [`apply_clean_filter`] but takes already-resolved attribute checks,
/// letting callers that have computed attributes once reuse them.
pub fn apply_clean_filter_with_attributes(
    config: &GitConfig,
    attributes: &[AttributeCheck],
    path: &[u8],
    content: &[u8],
) -> Result<Vec<u8>> {
    Ok(apply_clean_filter_with_attributes_cow(config, attributes, path, content)?.into_owned())
}

/// Borrow-first variant of [`apply_clean_filter_with_attributes`].
///
/// When no filter or EOL conversion changes the content, the returned value
/// borrows `content`; callers that can consume a [`Cow`] avoid allocating for
/// the common pass-through case.
pub fn apply_clean_filter_with_attributes_cow<'a>(
    config: &GitConfig,
    attributes: &[AttributeCheck],
    path: &[u8],
    content: &'a [u8],
) -> Result<Cow<'a, [u8]>> {
    apply_clean_filter_with_attributes_cow_safecrlf(
        config,
        attributes,
        path,
        content,
        ConvFlags::Off,
        SafeCrlfIndexBlob::None,
    )
}

/// How clean conversion should learn whether this path's *current index blob*
/// already contains a CRLF (git's `has_crlf_in_index`). Only consulted on the
/// `text=auto` / `core.autocrlf` path.
pub enum SafeCrlfIndexBlob<'a> {
    /// No index blob is available (the staging caller has none, or conversion
    /// is explicitly renormalizing) — treated as "no CRLF in index".
    None,
    /// The path's current index blob, read on demand from this object database
    /// only when the auto-crlf decision actually needs it.
    Lookup {
        odb: &'a FileObjectDatabase,
        oid: ObjectId,
    },
}

impl SafeCrlfIndexBlob<'_> {
    fn has_crlf(&self) -> bool {
        match self {
            SafeCrlfIndexBlob::None => false,
            SafeCrlfIndexBlob::Lookup { odb, oid } => has_crlf_in_index(odb, oid),
        }
    }
}

/// [`apply_clean_filter_with_attributes_cow`] plus git's additive `core.safecrlf`
/// round-trip warning (convert.c `crlf_to_git`).
///
/// `flags` drives the stderr warning git prints when a CRLF<->LF round-trip
/// would not be reversible. `index_blob` also implements git's "new safer
/// autocrlf" rule: normal cleaning preserves CRLF for an auto-text path whose
/// current index blob already contains CRLF. The warning is computed on the
/// *post-driver, pre-EOL-conversion* content, matching git's ordering in
/// `convert_to_git` (apply_filter -> crlf_to_git).
pub fn apply_clean_filter_with_attributes_cow_safecrlf<'a>(
    config: &GitConfig,
    attributes: &[AttributeCheck],
    path: &[u8],
    content: &'a [u8],
    flags: ConvFlags,
    index_blob: SafeCrlfIndexBlob<'_>,
) -> Result<Cow<'a, [u8]>> {
    // Non-object-writing callers (diff/status comparison): an encoding failure
    // is reported but not fatal.
    apply_clean_filter_cow_inner(
        config, attributes, path, content, flags, index_blob, false, false,
    )
}

/// Clean conversion core. `write_object` is set on the paths that hash content
/// into the object database (add / hash-object): there, an invalid
/// `working-tree-encoding` (bad BOM, undecodable bytes) is fatal, mirroring
/// convert.c's `CONV_WRITE_OBJECT` die.
///
/// `keep_crlf` mirrors convert.c `CONV_EOL_KEEP_CRLF`: when true the EOL pass is
/// skipped entirely (drivers/ident still run).
pub(crate) fn apply_clean_filter_cow_inner<'a>(
    config: &GitConfig,
    attributes: &[AttributeCheck],
    path: &[u8],
    content: &'a [u8],
    flags: ConvFlags,
    index_blob: SafeCrlfIndexBlob<'_>,
    write_object: bool,
    keep_crlf: bool,
) -> Result<Cow<'a, [u8]>> {
    let plan = ContentFilterPlan::resolve(config, attributes);
    check_wt_encoding_valid(&plan.encoding)?;
    let mut data = Cow::Borrowed(content);
    if let Some(driver) = &plan.driver {
        data = run_driver(driver, driver.clean.as_deref(), "clean", None, path, data)?;
    }
    // encode_to_git runs before the EOL pass (convert.c order: filter →
    // encode_to_git → crlf_to_git): the worktree charset is decoded to UTF-8 so
    // the line-ending stats and conversion below see real LF/CRLF bytes.
    data = encode_to_git(config, &plan.encoding, path, data, write_object)?;
    let index_has_crlf = plan.text == TextDecision::Auto && index_blob.has_crlf();
    // The safecrlf check scans the (post-driver) buffer once for line-ending
    // stats. Gate it tightly so the extra scan never runs on the dominant
    // pass-through paths: only when safecrlf is enabled, the path is a real
    // conversion candidate (not `CRLF_BINARY`), and the buffer is non-empty.
    if flags != ConvFlags::Off && !data.is_empty() && plan.safecrlf_applies() {
        let old_stats = gather_convert_stats(&data);
        plan.check_safe_crlf_stats(&old_stats, index_has_crlf, flags, path)?;
    }
    // convert.c's CONV_EOL_KEEP_CRLF skips crlf_to_git entirely. CONV_EOL_RENORMALIZE
    // bypasses the index-CRLF safeguard by not consulting the current index blob;
    // callers express that as `SafeCrlfIndexBlob::None`.
    if !keep_crlf && plan.convert_eol(&data) && !(plan.text == TextDecision::Auto && index_has_crlf)
    {
        data = convert_crlf_to_lf_cow(data);
    }
    if plan.ident {
        data = ident_to_git_cow(data);
    }
    Ok(data)
}

/// Apply the *smudge* conversion to `content` for `path` (blob -> worktree):
/// first LF->CRLF when EOL conversion applies, then the configured
/// `filter.<name>.smudge` driver (if any).
///
/// Semantics mirror [`apply_clean_filter`]: a required driver with a missing or
/// failing `smudge` command errors, while a non-required one passes the content
/// through.
pub fn apply_smudge_filter(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    config: &GitConfig,
    path: &[u8],
    content: &[u8],
) -> Result<Vec<u8>> {
    // On smudge (checkout) the worktree file may not exist yet, so resolve the
    // attributes from the `.gitattributes` recorded in the index.
    let checks =
        smudge_attribute_checks_from_index(worktree_root.as_ref(), git_dir.as_ref(), format, path)?;
    Ok(
        apply_smudge_filter_with_attributes_cow_format(config, &checks, path, content, format)?
            .into_owned(),
    )
}

/// Like [`apply_smudge_filter`] but takes already-resolved attribute checks.
pub fn apply_smudge_filter_with_attributes(
    config: &GitConfig,
    attributes: &[AttributeCheck],
    path: &[u8],
    content: &[u8],
) -> Result<Vec<u8>> {
    Ok(apply_smudge_filter_with_attributes_cow(config, attributes, path, content)?.into_owned())
}

/// Borrow-first variant of [`apply_smudge_filter_with_attributes`].
///
/// When no filter or EOL conversion changes the content, the returned value
/// borrows `content`; callers that can consume a [`Cow`] avoid allocating for
/// the common pass-through case.
pub fn apply_smudge_filter_with_attributes_cow<'a>(
    config: &GitConfig,
    attributes: &[AttributeCheck],
    path: &[u8],
    content: &'a [u8],
) -> Result<Cow<'a, [u8]>> {
    apply_smudge_filter_with_attributes_cow_format(
        config,
        attributes,
        path,
        content,
        ObjectFormat::Sha1,
    )
}

pub(crate) fn apply_smudge_filter_with_attributes_cow_format<'a>(
    config: &GitConfig,
    attributes: &[AttributeCheck],
    path: &[u8],
    content: &'a [u8],
    format: ObjectFormat,
) -> Result<Cow<'a, [u8]>> {
    match apply_smudge_filter_with_attributes_maybe_delayed(
        config, attributes, path, content, format, false,
    )? {
        SmudgeFilterResult::Content(data) => Ok(data),
        SmudgeFilterResult::Delayed { .. } => unreachable!("delay was not enabled"),
    }
}

pub(crate) fn apply_smudge_filter_with_attributes_maybe_delayed<'a>(
    config: &GitConfig,
    attributes: &[AttributeCheck],
    path: &[u8],
    content: &'a [u8],
    format: ObjectFormat,
    allow_delay: bool,
) -> Result<SmudgeFilterResult<'a>> {
    let plan = ContentFilterPlan::resolve(config, attributes);
    check_wt_encoding_valid(&plan.encoding)?;
    let process_blob = if plan
        .driver
        .as_ref()
        .and_then(|driver| driver.process.as_ref())
        .is_some()
    {
        Some(EncodedObject::new(ObjectType::Blob, content.to_vec()).object_id(format)?)
    } else {
        None
    };
    let mut data = Cow::Borrowed(content);
    if plan.ident {
        data = ident_to_worktree_cow(format, data)?;
    }
    if plan.eol == EolConversion::Crlf
        && plan.convert_eol(&data)
        && plan.will_convert_lf_to_crlf(&data)
    {
        data = Cow::Owned(convert_lf_to_crlf(&data));
    }
    // encode_to_worktree runs after the EOL pass (convert.c order:
    // crlf_to_worktree → encode_to_worktree → smudge filter): the UTF-8 blob is
    // line-ending-converted, then reencoded into the worktree charset.
    data = encode_to_worktree(&plan.encoding, path, data)?;
    if let Some(driver) = &plan.driver {
        return match run_driver_maybe_delayed(
            driver,
            driver.smudge.as_deref(),
            "smudge",
            Some(format),
            process_blob,
            path,
            data,
            allow_delay,
        )? {
            DriverFilterResult::Content(data) => Ok(SmudgeFilterResult::Content(data)),
            DriverFilterResult::Delayed { process } => Ok(SmudgeFilterResult::Delayed { process }),
        };
    }
    Ok(SmudgeFilterResult::Content(data))
}

/// Execute one direction of a driver filter, honouring the `required` flag.
pub(crate) fn run_driver<'a>(
    driver: &FilterDriver,
    command: Option<&str>,
    direction: &str,
    format: Option<ObjectFormat>,
    path: &[u8],
    content: Cow<'a, [u8]>,
) -> Result<Cow<'a, [u8]>> {
    match run_driver_maybe_delayed(
        driver, command, direction, format, None, path, content, false,
    )? {
        DriverFilterResult::Content(data) => Ok(data),
        DriverFilterResult::Delayed { .. } => unreachable!("delay was not enabled"),
    }
}

pub(crate) fn run_driver_maybe_delayed<'a>(
    driver: &FilterDriver,
    command: Option<&str>,
    direction: &str,
    format: Option<ObjectFormat>,
    process_blob: Option<ObjectId>,
    path: &[u8],
    content: Cow<'a, [u8]>,
    allow_delay: bool,
) -> Result<DriverFilterResult<'a>> {
    if let Some(process) = &driver.process {
        let blob = if direction == "smudge" {
            match (process_blob, format) {
                (Some(blob), _) => Some(blob),
                (None, Some(format)) => {
                    Some(EncodedObject::new(ObjectType::Blob, content.to_vec()).object_id(format)?)
                }
                (None, None) => None,
            }
        } else {
            None
        };
        match run_process_filter(process, direction, path, &content, blob, allow_delay) {
            Ok(ProcessFilterOutcome::Filtered(output)) => {
                return Ok(DriverFilterResult::Content(Cow::Owned(output)));
            }
            Ok(ProcessFilterOutcome::Unsupported) => {}
            Ok(ProcessFilterOutcome::Status(status)) => {
                if allow_delay && status == "delayed" {
                    return Ok(DriverFilterResult::Delayed {
                        process: process.clone(),
                    });
                }
                if driver.required {
                    return Err(GitError::Command(format!(
                        "external filter '{}' returned status {status}",
                        process
                    )));
                }
                return Ok(DriverFilterResult::Content(content));
            }
            Err(err) => {
                // Git surfaces a malformed process-filter handshake before its
                // generic external-filter failure. An immediate EOF (for a
                // missing command) is intentionally left as the generic
                // failure plus the path-specific required-filter fatal below.
                if err.message.starts_with("Unexpected line") {
                    eprintln!("error: {}", err.message);
                }
                if err.protocol {
                    eprintln!("error: external filter '{}' failed", process);
                }
                if driver.required {
                    let path = String::from_utf8_lossy(path);
                    let name = String::from_utf8_lossy(&driver.name);
                    eprintln!("fatal: {path}: {direction} filter '{name}' failed");
                    return Err(GitError::Exit(128));
                }
                return Ok(DriverFilterResult::Content(content));
            }
        }
    }
    let Some(command) = command else {
        // No command in this direction. Required filters must error; optional
        // ones pass content through unchanged.
        if driver.required {
            let path = String::from_utf8_lossy(path);
            let name = String::from_utf8_lossy(&driver.name);
            if direction == "clean" {
                eprintln!("fatal: {path}: clean filter '{name}' failed");
            } else {
                eprintln!("fatal: {path}: smudge filter {name} failed");
            }
            return Err(GitError::Exit(128));
        }
        return Ok(DriverFilterResult::Content(content));
    };
    match run_filter_command(command, path, &content) {
        Ok(output) => Ok(DriverFilterResult::Content(Cow::Owned(output))),
        Err(err) => {
            if driver.required {
                Err(err)
            } else {
                // Non-required filter failure: fall back to the unfiltered
                // content, matching git's behaviour.
                Ok(DriverFilterResult::Content(content))
            }
        }
    }
}

/// Compute the attributes relevant to content filtering (`text`, `eol`,
/// `filter`) for `path` from the worktree `.gitattributes` chain.
pub(crate) fn filter_attribute_checks(
    worktree_root: &Path,
    path: &[u8],
) -> Result<Vec<AttributeCheck>> {
    let requested = filter_attribute_names();
    let git_dir = worktree_root.join(".git");
    let mut matcher = AttributeMatcher::from_global_sources(worktree_root, &git_dir);
    fold_in_tree_attribute_frames(worktree_root, path, &mut matcher)?;
    fold_info_attributes(&git_dir, &mut matcher);
    Ok(matcher.attributes_for_path(path, &requested, false))
}

/// Compute filtering attributes for a checkout (blob -> worktree).
///
/// `git checkout -- <pathspec>` / `git restore` materialize through git's
/// **default** attr direction, which is `GIT_ATTR_CHECKIN` (attr.c: the static
/// `direction` is zero-initialized and `builtin/checkout.c` never overrides it
/// for the pathspec path). Under that direction `read_attr` reads each
/// `.gitattributes` frame from the **worktree file first**, falling back to the
/// staged blob only when no worktree file exists at that directory level
/// (sparse-checkout). This is the precedence the smudge filter must use:
/// t0027 commits an *empty* root `.gitattributes`, then overwrites the worktree
/// copy with `*.txt text eol=crlf` *without re-staging* — and git's checkout
/// still honours the worktree copy. Reading the index alone (or index-first)
/// made checkout under-convert line endings, because the staged blob was empty.
pub(crate) fn smudge_attribute_checks_from_index(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    path: &[u8],
) -> Result<Vec<AttributeCheck>> {
    let requested = filter_attribute_names();
    let mut matcher = AttributeMatcher::default();
    matcher.configure_case_sensitivity(git_dir);
    if !matcher.read_configured_attributes(worktree_root, git_dir) {
        matcher.read_default_global_attributes();
    }

    // Build the set of `.gitattributes` blobs the index carries, keyed by the
    // directory they govern, so each ancestry frame can prefer the staged copy.
    let index_attributes = index_gitattributes_by_base(git_dir, format)?;

    // Walk root -> ... -> the file's parent directory, folding each frame's
    // `.gitattributes` in shallow-to-deep order so deeper directories win.
    fold_checkout_attribute_frame(worktree_root, &[], &index_attributes, &mut matcher)?;
    let mut prefix = Vec::new();
    let mut parts = path.split(|byte| *byte == b'/').peekable();
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            break;
        }
        if !prefix.is_empty() {
            prefix.push(b'/');
        }
        prefix.extend_from_slice(part);
        let dir = worktree_root.join(repo_path_to_os_path(&prefix)?);
        fold_checkout_attribute_frame(&dir, &prefix, &index_attributes, &mut matcher)?;
    }

    read_attribute_patterns(
        worktree_root.join(".git").join("info").join("attributes"),
        &mut matcher,
        &[],
        b".git/info/attributes",
        false,
    );
    Ok(matcher.attributes_for_path(path, &requested, false))
}

/// Fold the `.gitattributes` governing directory `base` (whose on-disk location
/// is `dir`) into `matcher`, preferring the worktree file and falling back to
/// the staged blob. Mirrors one attr-stack frame under `GIT_ATTR_CHECKIN`
/// (git's default direction, used by `checkout -- <pathspec>` / `restore`).
pub(crate) fn fold_checkout_attribute_frame(
    dir: &Path,
    base: &[u8],
    index_attributes: &BTreeMap<Vec<u8>, Vec<u8>>,
    matcher: &mut AttributeMatcher,
) -> Result<()> {
    let worktree_file = dir.join(".gitattributes");
    let source = attribute_source_for_base(base);
    if let Ok(contents) = fs::read(&worktree_file) {
        // A worktree `.gitattributes` exists at this level: it wins outright
        // (git only consults the index when the worktree file is absent).
        read_attribute_patterns_from_bytes(&contents, matcher, base, &source);
    } else if let Some(contents) = index_attributes.get(base) {
        read_attribute_patterns_from_bytes(contents, matcher, base, &source);
    }
    Ok(())
}

/// Read every staged `.gitattributes` blob, keyed by the repo-relative directory
/// it governs (`""` for the worktree root). Stage-0 blob entries only.
pub(crate) fn index_gitattributes_by_base(
    git_dir: &Path,
    format: ObjectFormat,
) -> Result<BTreeMap<Vec<u8>, Vec<u8>>> {
    let mut map = BTreeMap::new();
    let index_path = repository_index_path(git_dir);
    if !index_path.exists() {
        return Ok(map);
    }
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let entries = Index::parse(&fs::read(index_path)?, format)?.entries;
    for entry in entries {
        let is_attributes_file =
            entry.path == b".gitattributes" || entry.path.as_bytes().ends_with(b"/.gitattributes");
        if index_entry_stage(&entry) != 0
            || tree_entry_object_type(entry.mode) != ObjectType::Blob
            || !is_attributes_file
        {
            continue;
        }
        let base = match entry.path.as_bytes().strip_suffix(b".gitattributes") {
            Some(b"") => Vec::new(),
            Some(parent) => parent.strip_suffix(b"/").unwrap_or(parent).to_vec(),
            None => continue,
        };
        let object = db
            .read_object(&entry.oid)
            .map_err(|err| expect_missing_object_kind(err, entry.oid, MissingObjectKind::Blob))?;
        if object.object_type == ObjectType::Blob {
            map.insert(base, object.body.clone());
        }
    }
    Ok(map)
}

pub(crate) fn filter_attribute_names() -> Vec<Vec<u8>> {
    // `crlf` is git's legacy alias for `text` (convert.c registers both); it is
    // consulted as a fallback when `text` is unspecified, so we must resolve it.
    vec![
        b"text".to_vec(),
        b"crlf".to_vec(),
        b"ident".to_vec(),
        b"eol".to_vec(),
        b"filter".to_vec(),
        b"working-tree-encoding".to_vec(),
    ]
}

// ---------------------------------------------------------------------------
// `ls-files --eol` line-ending information
//
// Git's `git ls-files --eol` prints, for each path, three fields:
//   i/<stat>  — line-ending statistics of the *index* blob content
//   w/<stat>  — line-ending statistics of the *worktree* file content
//   attr/<a>  — the resolved crlf/eol attribute action (attributes only, no
//               config) — `get_convert_attr_ascii` in convert.c
// The two stat fields mirror `gather_convert_stats_ascii`; the attr field
// mirrors `convert_attrs` up to `ca->attr_action` (i.e. *before* the config
// derived `text` -> input/crlf substitution and the `core.autocrlf` fallback).
// ---------------------------------------------------------------------------

/// Line-ending statistics of a byte buffer, mirroring convert.c `gather_stats`.
#[derive(Clone)]
pub(crate) struct ConvertStats {
    nul: u32,
    lonecr: u32,
    lonelf: u32,
    crlf: u32,
    printable: u32,
    nonprintable: u32,
}

pub(crate) fn gather_convert_stats(buf: &[u8]) -> ConvertStats {
    let mut stats = ConvertStats {
        nul: 0,
        lonecr: 0,
        lonelf: 0,
        crlf: 0,
        printable: 0,
        nonprintable: 0,
    };
    let mut i = 0;
    while i < buf.len() {
        let c = buf[i];
        if c == b'\r' {
            if buf.get(i + 1) == Some(&b'\n') {
                stats.crlf += 1;
                i += 1;
            } else {
                stats.lonecr += 1;
            }
            i += 1;
            continue;
        }
        if c == b'\n' {
            stats.lonelf += 1;
            i += 1;
            continue;
        }
        if c == 127 {
            // DEL
            stats.nonprintable += 1;
        } else if c < 32 {
            match c {
                // BS, HT, ESC and FF are printable.
                0x08 | 0x09 | 0x1b | 0x0c => stats.printable += 1,
                0 => {
                    stats.nul += 1;
                    stats.nonprintable += 1;
                }
                _ => stats.nonprintable += 1,
            }
        } else {
            stats.printable += 1;
        }
        i += 1;
    }
    // A trailing EOF (^Z, 0x1a) is not counted as non-printable.
    if buf.last() == Some(&0x1a) {
        stats.nonprintable = stats.nonprintable.saturating_sub(1);
    }
    stats
}

/// Mirror of convert.c `has_crlf_in_index`: whether the blob currently recorded
/// in the index for this path is non-binary text containing a CRLF. Used only by
/// the auto-crlf safecrlf decision to keep an already-CRLF index blob from being
/// silently collapsed. A missing/unreadable blob (or a non-blob entry) counts as
/// "no CRLF", matching git's `read_blob_data_from_index` returning NULL.
pub(crate) fn has_crlf_in_index(odb: &FileObjectDatabase, oid: &ObjectId) -> bool {
    let Ok(object) = odb.read_object(oid) else {
        return false;
    };
    if object.object_type != ObjectType::Blob {
        return false;
    }
    let data = &object.body;
    // git short-circuits on the first '\r' via memchr before gathering stats.
    if !data.contains(&b'\r') {
        return false;
    }
    let stats = gather_convert_stats(data);
    !convert_is_binary(&stats) && stats.crlf > 0
}

/// Mirror of convert.c `convert_is_binary`: a lone CR or NUL, or a high
/// non-printable ratio, marks the content as binary.
pub(crate) fn convert_is_binary(stats: &ConvertStats) -> bool {
    if stats.lonecr > 0 {
        return true;
    }
    if stats.nul > 0 {
        return true;
    }
    (stats.printable >> 7) < stats.nonprintable
}

/// The `core.safecrlf` round-trip-warning mode, mirroring git's
/// `global_conv_flags_eol` (environment.c). git's *default* — when
/// `core.safecrlf` is unset — is [`ConvFlags::Warn`], so the warning fires even
/// without any explicit config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConvFlags {
    /// `core.safecrlf=false`: never warn.
    Off,
    /// `core.safecrlf=warn` (and the unset default): emit a warning when a
    /// CRLF<->LF round-trip would not be reversible.
    Warn,
    /// `core.safecrlf=true`: die instead of warn.
    Die,
}

impl ConvFlags {
    /// Resolve `core.safecrlf` from config, mirroring environment.c
    /// `git_default_core_config`: `warn` -> [`ConvFlags::Warn`], a boolean-true
    /// value -> [`ConvFlags::Die`], a boolean-false value -> [`ConvFlags::Off`].
    /// When the key is absent git leaves `global_conv_flags_eol` at its initial
    /// [`ConvFlags::Warn`], so unset also resolves to [`ConvFlags::Warn`].
    pub fn from_config(config: &GitConfig) -> Self {
        match config.get("core", None, "safecrlf") {
            Some(value) if value.eq_ignore_ascii_case("warn") => ConvFlags::Warn,
            Some(_) => {
                if config.get_bool("core", None, "safecrlf") == Some(true) {
                    ConvFlags::Die
                } else {
                    ConvFlags::Off
                }
            }
            None => ConvFlags::Warn,
        }
    }
}

/// Mirror of convert.c `check_global_conv_flags_eol`: compare the pre-conversion
/// `old_stats` against the simulated round-trip `new_stats` and, when the
/// CRLF/LF content would not survive a clean+smudge cycle, warn (or die under
/// `core.safecrlf=true`).
///
/// Returns `Err(GitError::Exit(128))` when `flags` is [`ConvFlags::Die`] and the
/// round-trip is irreversible (git `die`s with exit 128 here); otherwise prints
/// the warning to stderr and returns `Ok(())`. This is a pure stderr-side
/// effect: it never changes the bytes written to the object store.
pub(crate) fn check_safe_crlf(
    old_stats: &ConvertStats,
    new_stats: &ConvertStats,
    flags: ConvFlags,
    path: &[u8],
) -> Result<()> {
    if flags == ConvFlags::Off {
        return Ok(());
    }
    let display = String::from_utf8_lossy(path);
    if old_stats.crlf > 0 && new_stats.crlf == 0 {
        // CRLFs would not be restored by checkout.
        match flags {
            ConvFlags::Die => {
                eprintln!("fatal: CRLF would be replaced by LF in {display}");
                return Err(GitError::Exit(128));
            }
            ConvFlags::Warn => {
                eprintln!(
                    "warning: in the working copy of '{display}', CRLF will be replaced by LF the next time Git touches it"
                );
            }
            ConvFlags::Off => unreachable!("handled above"),
        }
    } else if old_stats.lonelf > 0 && new_stats.lonelf == 0 {
        // CRLFs would be added by checkout.
        match flags {
            ConvFlags::Die => {
                eprintln!("fatal: LF would be replaced by CRLF in {display}");
                return Err(GitError::Exit(128));
            }
            ConvFlags::Warn => {
                eprintln!(
                    "warning: in the working copy of '{display}', LF will be replaced by CRLF the next time Git touches it"
                );
            }
            ConvFlags::Off => unreachable!("handled above"),
        }
    }
    Ok(())
}

/// Compute the `i/` or `w/` stat string for `content`, mirroring
/// convert.c `gather_convert_stats_ascii`.
pub(crate) fn convert_stats_ascii(content: &[u8]) -> &'static str {
    if content.is_empty() {
        return "none";
    }
    let stats = gather_convert_stats(content);
    if convert_is_binary(&stats) {
        return "-text";
    }
    match (stats.lonelf > 0, stats.crlf > 0) {
        (true, false) => "lf",
        (false, true) => "crlf",
        (true, true) => "mixed",
        (false, false) => "none",
    }
}

/// The resolved crlf/eol attribute action for a path, mirroring convert.c
/// `convert_attrs` up to `ca->attr_action` (attributes only, no config), and
/// `get_convert_attr_ascii` for the ascii spelling.
pub(crate) fn convert_attr_ascii(checks: &[AttributeCheck]) -> &'static str {
    fn state_of<'a>(checks: &'a [AttributeCheck], name: &[u8]) -> Option<&'a AttributeState> {
        checks
            .iter()
            .find(|check| check.attribute == name)
            .and_then(|check| check.state.as_ref())
    }

    // git_path_check_crlf: ATTR_TRUE -> TEXT, ATTR_FALSE -> BINARY,
    // ATTR_UNSET -> (fall through), "input" -> TEXT_INPUT, "auto" -> AUTO,
    // anything else -> UNDEFINED.
    #[derive(Clone, Copy, PartialEq)]
    enum Action {
        Undefined,
        Binary,
        Text,
        TextInput,
        TextCrlf,
        Auto,
        AutoCrlf,
        AutoInput,
    }
    fn check_crlf(state: Option<&AttributeState>) -> Action {
        match state {
            Some(AttributeState::Set) => Action::Text,
            Some(AttributeState::Unset) => Action::Binary,
            Some(AttributeState::Value(value)) if value == b"input" => Action::TextInput,
            Some(AttributeState::Value(value)) if value == b"auto" => Action::Auto,
            // ATTR_UNSET / any other value -> CRLF_UNDEFINED.
            _ => Action::Undefined,
        }
    }

    // Resolve from the `text` attribute, then fall back to the legacy `crlf`
    // alias only when `text` left the action undefined.
    let mut action = check_crlf(state_of(checks, b"text"));
    if action == Action::Undefined {
        action = check_crlf(state_of(checks, b"crlf"));
    }

    if action != Action::Binary {
        // git_path_check_eol: only "lf"/"crlf" values matter.
        let eol = match state_of(checks, b"eol") {
            Some(AttributeState::Value(value)) if value == b"lf" => Some(false),
            Some(AttributeState::Value(value)) if value == b"crlf" => Some(true),
            _ => None,
        };
        action = match (action, eol) {
            (Action::Auto, Some(false)) => Action::AutoInput,
            (Action::Auto, Some(true)) => Action::AutoCrlf,
            (_, Some(false)) if action != Action::Auto => Action::TextInput,
            (_, Some(true)) if action != Action::Auto => Action::TextCrlf,
            _ => action,
        };
    }

    match action {
        Action::Undefined => "",
        Action::Binary => "-text",
        Action::Text => "text",
        Action::TextInput => "text eol=lf",
        Action::TextCrlf => "text eol=crlf",
        Action::Auto => "text=auto",
        Action::AutoCrlf => "text=auto eol=crlf",
        Action::AutoInput => "text=auto eol=lf",
    }
}

/// The three `ls-files --eol` fields for a single path.
pub struct EolInfo {
    /// Stat of the index blob (`i/...`); empty when there is no index blob.
    pub index: &'static str,
    /// Stat of the worktree file (`w/...`); empty when the file is absent.
    pub worktree: &'static str,
    /// Resolved crlf/eol attribute action (`attr/...`).
    pub attr: &'static str,
}

impl EolInfo {
    /// Format as git's `ls-files --eol` prefix: `i/%-5s w/%-5s attr/%-17s\t`.
    pub fn format_prefix(&self) -> String {
        format!(
            "i/{:<5} w/{:<5} attr/{:<17}\t",
            self.index, self.worktree, self.attr
        )
    }
}

/// Compute the `ls-files --eol` info for `path`.
///
/// `index_content` is the raw index blob bytes (None when the path has no
/// index entry or is not a regular file). The worktree file is read from
/// `worktree_root/path`; if it is absent or not a regular file the `w/` field
/// is empty. Attributes are resolved from the worktree `.gitattributes` chain
/// via `attr_checks`.
pub fn eol_info_for_path(
    worktree_root: impl AsRef<Path>,
    path: &[u8],
    index_content: Option<&[u8]>,
    attr_checks: &[AttributeCheck],
) -> EolInfo {
    let index = index_content.map(convert_stats_ascii).unwrap_or("");

    let worktree_root = worktree_root.as_ref();
    let worktree = match repo_path_to_os_path(path) {
        Ok(rel) => {
            let absolute = worktree_root.join(rel);
            match fs::symlink_metadata(&absolute) {
                // git: only regular files get a `w/` stat (lstat + S_ISREG).
                Ok(meta) if meta.file_type().is_file() => match fs::read(&absolute) {
                    Ok(content) => convert_stats_ascii_owned(&content),
                    Err(_) => "",
                },
                _ => "",
            }
        }
        Err(_) => "",
    };

    let attr = convert_attr_ascii(attr_checks);

    EolInfo {
        index,
        worktree,
        attr,
    }
}

/// `convert_stats_ascii` over an owned buffer; the result is a `'static` str so
/// the buffer can be dropped.
pub(crate) fn convert_stats_ascii_owned(content: &[u8]) -> &'static str {
    convert_stats_ascii(content)
}

/// Resolve the crlf/eol/text/filter attributes for `path` from the worktree
/// `.gitattributes` chain (the set `ls-files --eol` needs for its `attr/`
/// field).
pub fn eol_attribute_checks(
    worktree_root: impl AsRef<Path>,
    path: &[u8],
) -> Result<Vec<AttributeCheck>> {
    filter_attribute_checks(worktree_root.as_ref(), path)
}
