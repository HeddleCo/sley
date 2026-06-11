//! Compiled `git log` / `rev-list` pretty-format strings.
//!
//! Formats are parsed once at CLI arg time into [`CompiledLogFormat`] (a token
//! stream + a [`FormatTier`] that describes how much commit data emission needs).
//! Command fast paths consult the tier instead of hand-maintained string guards.

use sley_core::{GitError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LogFormatDialect {
    Log,
    RevList,
    Stash,
}

/// How much commit data a format needs to render.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[allow(dead_code)]
pub(crate) enum FormatTier {
    /// `%H` (and literals / no-op color codes only).
    OidOnly,
    /// Oids + parents + left/right marker — satisfied by [`CommitMetadata`].
    Metadata,
    /// Tree oid and/or subject line — needs a parsed commit header, not the body.
    Header,
    /// Author/committer/body/encoding/decorations placeholders.
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct FormatFields(u16);

#[allow(dead_code)]
impl FormatFields {
    pub(crate) const OID: Self = Self(1 << 0);
    pub(crate) const TREE: Self = Self(1 << 1);
    pub(crate) const PARENTS: Self = Self(1 << 2);
    pub(crate) const SUBJECT: Self = Self(1 << 3);
    pub(crate) const BODY: Self = Self(1 << 4);
    pub(crate) const AUTHOR: Self = Self(1 << 5);
    pub(crate) const COMMITTER: Self = Self(1 << 6);
    pub(crate) const ENCODING: Self = Self(1 << 7);
    pub(crate) const DECORATIONS: Self = Self(1 << 8);
    pub(crate) const REV_SOURCE: Self = Self(1 << 9);

    pub(crate) const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub(crate) const fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    pub(crate) const fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub(crate) fn tier(self) -> FormatTier {
        if self.intersects(
            Self::BODY | Self::AUTHOR | Self::COMMITTER | Self::ENCODING | Self::DECORATIONS,
        ) {
            return FormatTier::Full;
        }
        if self.intersects(Self::SUBJECT | Self::TREE) {
            return FormatTier::Header;
        }
        if self.intersects(Self::PARENTS) {
            return FormatTier::Metadata;
        }
        if self.contains(Self::OID) {
            FormatTier::Metadata
        } else if self.is_empty() {
            FormatTier::Full
        } else {
            FormatTier::Full
        }
    }
}

impl std::ops::BitOrAssign for FormatFields {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl std::ops::BitOr for FormatFields {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FormatToken {
    Literal(String),
    Percent,
    OidFull,
    OidAbbrev,
    TreeFull,
    TreeAbbrev,
    ParentsFull,
    ParentsAbbrev,
    Marker,
    Subject,
    SanitizedSubject,
    Encoding,
    NoteName,
    RevisionSource,
    ColorParen,
    ColorName(&'static str),
    Body,
    FullMessage,
    DecorationsParen,
    DecorationsBare,
    AuthorName,
    AuthorEmail,
    AuthorEmailLocal,
    AuthorTimestamp,
    AuthorDate,
    AuthorDateIso,
    AuthorDateIsoStrict,
    AuthorDateShort,
    AuthorDateRfc2822,
    CommitterName,
    CommitterEmail,
    CommitterEmailLocal,
    CommitterTimestamp,
    CommitterDate,
    CommitterDateIso,
    CommitterDateIsoStrict,
    CommitterDateShort,
    CommitterDateRfc2822,
    Newline,
    HexByte(u8),
    GRefname,
    GTrailers,
    GPlaceholder,
    GKey,
    GFingerprint,
    GPassthrough,
    GSignature,
    GDate,
    GDateShort,
    GDateIso,
    GDateIsoStrict,
    GDateRfc2822,
    /// `stash list` — `%d` when `stash@{0}`.
    StashDecoParen,
    /// `stash list` — `%D` when `stash@{0}`.
    StashDecoBare,
    /// `stash list` — `%gd`.
    ReflogGd,
    /// `stash list` — `%gD`.
    ReflogGD,
    /// `stash list` — `%gn` / `%gN`.
    ReflogGn,
    /// `stash list` — `%ge` / `%gE`.
    ReflogGe,
    /// `stash list` — `%gs` (reflog subject).
    ReflogGs,
    /// `%<(N[,trunc])` style alignment/padding directive.
    Padding(PaddingSpec),
    /// `%w(width[,indent1[,indent2]])` wrapping directive.
    Wrap(WrapSpec),
    /// `%(trailers[:opts])`.
    Trailers(String),
    /// `%(decorate[:opts])`.
    Decorate(DecorateSpec),
    /// `%(describe[:opts])`.
    Describe(DescribeSpec),
    /// `%C(...)` color directive that should still flush pending padding even
    /// though it produces no visible width (matches git's modifier handling).
    ColorAuto,
    /// A `%-`/`%+`/`% ` magic prefix applied to the following placeholder.
    Magic(MagicPrefix),
}

/// git's per-placeholder magic prefix (`%-`/`%+`/`% `).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MagicPrefix {
    /// `%-`: delete preceding newline(s) when the placeholder is empty.
    DelLfBeforeEmpty,
    /// `%+`: insert a newline before a non-empty placeholder.
    AddLfBeforeNonEmpty,
    /// `% `: insert a space before a non-empty placeholder.
    AddSpBeforeNonEmpty,
}

/// Flush direction for a `%<`/`%>`/`%><`/`%>>` padding directive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PaddingFlush {
    Left,
    Right,
    Both,
    LeftAndSteal,
}

/// Truncation mode for a padding directive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PaddingTrunc {
    None,
    Left,
    Middle,
    Right,
}

/// A parsed `%<`/`%>`/... padding placeholder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PaddingSpec {
    pub flush: PaddingFlush,
    pub trunc: PaddingTrunc,
    /// Positive = fixed width; negative = "pad to that column" (`%<|`).
    pub padding: i64,
}

/// A parsed `%w(width,indent1,indent2)` wrap directive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WrapSpec {
    pub width: usize,
    pub indent1: usize,
    pub indent2: usize,
}

/// A parsed `%(decorate[:opts])` placeholder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DecorateSpec {
    pub prefix: String,
    pub suffix: String,
    pub separator: String,
    pub pointer: String,
    pub tag: String,
}

impl Default for DecorateSpec {
    fn default() -> Self {
        Self {
            prefix: " (".into(),
            suffix: ")".into(),
            separator: ", ".into(),
            pointer: " -> ".into(),
            tag: "tag: ".into(),
        }
    }
}

/// A parsed `%(describe[:opts])` placeholder.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct DescribeSpec {
    pub tags: bool,
    pub abbrev: Option<usize>,
    pub matches: Vec<String>,
    pub excludes: Vec<String>,
}

impl FormatToken {
    pub(crate) fn is_metadata_emitable(&self) -> bool {
        matches!(
            self,
            FormatToken::Literal(_)
                | FormatToken::Percent
                | FormatToken::OidFull
                | FormatToken::OidAbbrev
                | FormatToken::ParentsFull
                | FormatToken::ParentsAbbrev
                | FormatToken::Marker
                | FormatToken::NoteName
                | FormatToken::RevisionSource
                | FormatToken::ColorParen
                | FormatToken::ColorName(_)
                | FormatToken::Newline
                | FormatToken::HexByte(_)
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompiledLogFormat {
    pub tokens: Vec<FormatToken>,
    pub fields: FormatFields,
    pub dialect: LogFormatDialect,
}

impl CompiledLogFormat {
    pub(crate) fn compile(format: &str, dialect: LogFormatDialect) -> Result<Self> {
        let mut tokens = Vec::new();
        let mut fields = FormatFields::default();
        let mut chars = format.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch != '%' {
                push_literal(&mut tokens, ch);
                continue;
            }
            // A magic prefix (`%-`/`%+`/`% `) applies to the *following*
            // placeholder. `%+w()`/`% w()`/`%-w()` are refused by git (the magic
            // cannot reorder wrap output), so a following `w(` falls back to a
            // verbatim `%`.
            if let Some(&magic_ch) = chars.peek()
                && matches!(magic_ch, '-' | '+' | ' ')
            {
                let mut after = chars.clone();
                after.next(); // skip the magic char
                if after.peek() == Some(&'w') {
                    // `%±w(...)` — git refuses; emit a verbatim '%'.
                    push_literal(&mut tokens, '%');
                    continue;
                }
                let prefix = match magic_ch {
                    '-' => MagicPrefix::DelLfBeforeEmpty,
                    '+' => MagicPrefix::AddLfBeforeNonEmpty,
                    _ => MagicPrefix::AddSpBeforeNonEmpty,
                };
                tokens.push(FormatToken::Magic(prefix));
                chars.next(); // consume the magic char; placeholder follows
            }
            // Complex directives (`%<`, `%>`, `%w(`, `%(...)`) need byte-accurate
            // slicing of the remainder, so peek the rest of the format and parse
            // it directly; on success advance the char iterator past it.
            {
                let rest: String = chars.clone().collect();
                if let Some((token, consumed_bytes)) =
                    parse_complex_directive(&rest, dialect, &mut fields)?
                {
                    let consumed_chars = rest[..consumed_bytes].chars().count();
                    for _ in 0..consumed_chars {
                        chars.next();
                    }
                    if let Some(token) = token {
                        tokens.push(token);
                    }
                    continue;
                }
            }
            match chars.next() {
                Some('%') => tokens.push(FormatToken::Percent),
                Some('H') => {
                    fields = fields | FormatFields::OID;
                    tokens.push(FormatToken::OidFull);
                }
                Some('h') => {
                    fields |= FormatFields::OID;
                    tokens.push(FormatToken::OidAbbrev);
                }
                Some('T') => {
                    fields |= FormatFields::TREE;
                    tokens.push(FormatToken::TreeFull);
                }
                Some('t') => {
                    fields |= FormatFields::TREE;
                    tokens.push(FormatToken::TreeAbbrev);
                }
                Some('P') => {
                    fields |= FormatFields::PARENTS;
                    tokens.push(FormatToken::ParentsFull);
                }
                Some('p') => {
                    fields |= FormatFields::PARENTS;
                    tokens.push(FormatToken::ParentsAbbrev);
                }
                Some('m') => tokens.push(FormatToken::Marker),
                Some('s') => {
                    fields |= FormatFields::SUBJECT;
                    tokens.push(FormatToken::Subject);
                }
                Some('f') => {
                    fields |= FormatFields::SUBJECT;
                    tokens.push(FormatToken::SanitizedSubject);
                }
                Some('e') => {
                    fields |= FormatFields::ENCODING;
                    tokens.push(FormatToken::Encoding);
                }
                Some('N') => tokens.push(FormatToken::NoteName),
                Some('S') => {
                    fields |= FormatFields::REV_SOURCE;
                    tokens.push(FormatToken::RevisionSource);
                }
                Some('C') => {
                    consume_color(&mut chars, &mut tokens)?;
                }
                Some('b') => {
                    fields |= FormatFields::BODY;
                    tokens.push(FormatToken::Body);
                }
                Some('B') => {
                    fields |= FormatFields::BODY;
                    tokens.push(FormatToken::FullMessage);
                }
                Some('d') if dialect == LogFormatDialect::Stash => {
                    tokens.push(FormatToken::StashDecoParen);
                }
                Some('d') => {
                    fields |= FormatFields::DECORATIONS;
                    tokens.push(FormatToken::DecorationsParen);
                }
                Some('D') if dialect == LogFormatDialect::Stash => {
                    tokens.push(FormatToken::StashDecoBare);
                }
                Some('D') => {
                    fields |= FormatFields::DECORATIONS;
                    tokens.push(FormatToken::DecorationsBare);
                }
                Some('G') => consume_g_placeholder(&mut chars, &mut tokens)?,
                Some('g') if matches!(dialect, LogFormatDialect::Stash | LogFormatDialect::Log) => {
                    consume_reflog_g_placeholder(&mut chars, &mut tokens)?;
                }
                Some('g') => consume_g_date_placeholder(&mut chars, &mut tokens)?,
                Some('a') => {
                    consume_identity_placeholder(&mut chars, &mut tokens, &mut fields, true)?
                }
                Some('c') => {
                    consume_identity_placeholder(&mut chars, &mut tokens, &mut fields, false)?
                }
                Some('n') => tokens.push(FormatToken::Newline),
                Some('x') => {
                    let mut lookahead = chars.clone();
                    if let (Some(high), Some(low)) = (lookahead.next(), lookahead.next())
                        && let (Some(high), Some(low)) = (high.to_digit(16), low.to_digit(16))
                    {
                        chars = lookahead;
                        tokens.push(FormatToken::HexByte(((high << 4) | low) as u8));
                    } else {
                        push_literal(&mut tokens, '%');
                        push_literal(&mut tokens, 'x');
                    }
                }
                Some(other) => {
                    return Err(GitError::Command(format!(
                        "unsupported log format placeholder %{other}"
                    )));
                }
                None => {
                    return Err(GitError::Command(
                        "unterminated log format placeholder %".into(),
                    ));
                }
            }
        }
        Ok(Self {
            tokens,
            fields,
            dialect,
        })
    }

    #[allow(dead_code)]
    pub(crate) fn tier(&self) -> FormatTier {
        self.fields.tier()
    }

    pub(crate) fn uses_decorations(&self) -> bool {
        self.fields.contains(FormatFields::DECORATIONS)
    }

    pub(crate) fn uses_parents(&self) -> bool {
        self.fields.contains(FormatFields::PARENTS)
    }

    pub(crate) fn uses_oid(&self) -> bool {
        self.fields.contains(FormatFields::OID)
    }

    pub(crate) fn uses_source(&self) -> bool {
        self.fields.contains(FormatFields::REV_SOURCE)
    }

    /// True when the format emits only full oids (`%H`) plus inert literals/newlines.
    #[allow(dead_code)]
    pub(crate) fn is_oid_only(&self) -> bool {
        self.tokens
            .iter()
            .any(|token| *token == FormatToken::OidFull)
            && self
                .tokens
                .iter()
                .all(|token| matches!(token, FormatToken::Literal(_) | FormatToken::OidFull))
    }

    /// True when every token can be rendered from [`sley_rev::CommitMetadata`] alone.
    pub(crate) fn is_metadata_emitable(&self) -> bool {
        !self.tokens.is_empty() && self.tokens.iter().all(|t| t.is_metadata_emitable())
    }

    pub(crate) fn insert_parents_after_oid(&mut self) {
        for index in 0..self.tokens.len() {
            match self.tokens[index] {
                FormatToken::OidFull => {
                    self.tokens
                        .insert(index + 1, FormatToken::Literal(" ".into()));
                    self.tokens.insert(index + 2, FormatToken::ParentsFull);
                    self.fields |= FormatFields::PARENTS;
                    return;
                }
                FormatToken::OidAbbrev => {
                    self.tokens
                        .insert(index + 1, FormatToken::Literal(" ".into()));
                    self.tokens.insert(index + 2, FormatToken::ParentsAbbrev);
                    self.fields |= FormatFields::PARENTS;
                    return;
                }
                _ => {}
            }
        }
    }

    /// Pre-size a line buffer for one emission pass.
    pub(crate) fn estimated_line_capacity(&self) -> usize {
        self.tokens.iter().fold(64usize, |acc, token| {
            acc + match token {
                FormatToken::Literal(text) => text.len(),
                FormatToken::OidFull => 40,
                FormatToken::OidAbbrev => 12,
                FormatToken::TreeFull => 40,
                FormatToken::TreeAbbrev => 12,
                FormatToken::ParentsFull | FormatToken::ParentsAbbrev => 48,
                FormatToken::Subject | FormatToken::SanitizedSubject => 80,
                FormatToken::Body | FormatToken::FullMessage => 256,
                FormatToken::DecorationsParen | FormatToken::DecorationsBare => 32,
                FormatToken::AuthorDate
                | FormatToken::AuthorDateIso
                | FormatToken::AuthorDateIsoStrict
                | FormatToken::AuthorDateShort
                | FormatToken::AuthorDateRfc2822
                | FormatToken::CommitterDate
                | FormatToken::CommitterDateIso
                | FormatToken::CommitterDateIsoStrict
                | FormatToken::CommitterDateShort
                | FormatToken::CommitterDateRfc2822 => 32,
                FormatToken::AuthorName
                | FormatToken::CommitterName
                | FormatToken::AuthorEmail
                | FormatToken::CommitterEmail => 48,
                _ => 8,
            }
        })
    }
}

/// Limit padding/wrap widths the way git's FORMATTING_LIMIT does, so an
/// overflowing directive is rejected (emitted verbatim) instead of allocating.
const FORMATTING_LIMIT: i64 = 1 << 30;

/// Try to parse a complex directive (`%<`/`%>`/`%w(`/`%(...)`) from `rest`,
/// which is the format string *after* the leading `%`. Returns:
/// - `Ok(None)` if `rest` doesn't start a complex directive — fall through.
/// - `Ok(Some((Some(token), n)))` to push `token` and consume `n` chars.
/// - `Ok(Some((Some(Literal("%")), 0)))` to emit a verbatim `%` (git's
///   "unknown/invalid placeholder" fallback) without consuming the rest.
#[allow(clippy::type_complexity)]
fn parse_complex_directive(
    rest: &str,
    dialect: LogFormatDialect,
    fields: &mut FormatFields,
) -> Result<Option<(Option<FormatToken>, usize)>> {
    let first = match rest.chars().next() {
        Some(ch) => ch,
        None => return Ok(None),
    };
    match first {
        '<' | '>' => {
            // A padding directive — or a verbatim `%`/`%>` if it doesn't parse.
            if let Some((spec, consumed)) = parse_padding_placeholder(rest) {
                Ok(Some((Some(FormatToken::Padding(spec)), consumed)))
            } else {
                // git emits a verbatim '%' and rescans from the flush char.
                Ok(Some((Some(FormatToken::Literal("%".into())), 0)))
            }
        }
        'w' => {
            // `%w(...)`; a bare `%w` (no paren) is not ours.
            if rest.as_bytes().get(1) != Some(&b'(') {
                return Ok(None);
            }
            if let Some((spec, consumed)) = parse_wrap_placeholder(rest) {
                Ok(Some((Some(FormatToken::Wrap(spec)), consumed)))
            } else {
                Ok(Some((Some(FormatToken::Literal("%".into())), 0)))
            }
        }
        '(' => {
            // `%(trailers...)`, `%(decorate...)`, `%(describe...)`.
            let Some(end) = rest.find(')') else {
                return Ok(Some((Some(FormatToken::Literal("%".into())), 0)));
            };
            let inner = &rest[1..end];
            let consumed = end + 1; // include '(' .. ')'
            if let Some(opts) = inner.strip_prefix("trailers") {
                let opts = opts.strip_prefix(':').unwrap_or("");
                // Validate the option string; a bad option means git emits the
                // placeholder verbatim (return 0).
                if !inner.starts_with("trailers")
                    || (!opts.is_empty()
                        && crate::commands::for_each_ref::parse_for_each_ref_trailer_options(opts)
                            .is_err())
                {
                    // `%(trailers:key)` (no value) etc. — verbatim.
                    return Ok(Some((Some(FormatToken::Literal("%".into())), 0)));
                }
                // Accept `trailers` and `trailers:<opts>` only (not `trailersX`).
                if !(inner == "trailers" || inner.starts_with("trailers:")) {
                    return Ok(Some((Some(FormatToken::Literal("%".into())), 0)));
                }
                *fields |= FormatFields::BODY;
                Ok(Some((Some(FormatToken::Trailers(opts.to_string())), consumed)))
            } else if inner == "decorate" || inner.starts_with("decorate:") {
                let opts = inner.strip_prefix("decorate").unwrap_or("");
                let opts = opts.strip_prefix(':').unwrap_or("");
                match parse_decorate_spec(opts) {
                    Some(spec) => {
                        *fields |= FormatFields::DECORATIONS;
                        Ok(Some((Some(FormatToken::Decorate(spec)), consumed)))
                    }
                    None => Ok(Some((Some(FormatToken::Literal("%".into())), 0))),
                }
            } else if inner == "describe" || inner.starts_with("describe:") {
                let opts = inner.strip_prefix("describe").unwrap_or("");
                let opts = opts.strip_prefix(':').unwrap_or("");
                match parse_describe_spec(opts) {
                    Some(spec) => {
                        *fields |= FormatFields::BODY;
                        Ok(Some((Some(FormatToken::Describe(spec)), consumed)))
                    }
                    None => Ok(Some((Some(FormatToken::Literal("%".into())), 0))),
                }
            } else {
                let _ = dialect;
                // Unknown `%(...)` — verbatim.
                Ok(Some((Some(FormatToken::Literal("%".into())), 0)))
            }
        }
        _ => Ok(None),
    }
}

/// Port of pretty.c `parse_padding_placeholder`. `rest` begins at the flush
/// char (`<`/`>`). Returns the spec and the number of chars consumed.
fn parse_padding_placeholder(rest: &str) -> Option<(PaddingSpec, usize)> {
    let bytes = rest.as_bytes();
    let mut idx = 0usize;
    let flush = match bytes.first()? {
        b'<' => {
            idx += 1;
            PaddingFlush::Right
        }
        b'>' => {
            idx += 1;
            match bytes.get(1) {
                Some(b'<') => {
                    idx += 1;
                    PaddingFlush::Both
                }
                Some(b'>') => {
                    idx += 1;
                    PaddingFlush::LeftAndSteal
                }
                _ => PaddingFlush::Left,
            }
        }
        _ => return None,
    };
    let mut to_column = false;
    if bytes.get(idx) == Some(&b'|') {
        to_column = true;
        idx += 1;
    }
    if bytes.get(idx) != Some(&b'(') {
        return None;
    }
    idx += 1;
    let start = idx;
    // strcspn(start, ",)")
    let num_end = rest[start..]
        .find([',', ')'])
        .map(|off| start + off)
        .unwrap_or(rest.len());
    if num_end >= rest.len() || num_end == start {
        // !*end || end == start
        return None;
    }
    // strtol
    let (width, num_consumed) = parse_leading_i64(&rest[start..]);
    if num_consumed == 0 {
        return None;
    }
    if !(-FORMATTING_LIMIT..=FORMATTING_LIMIT).contains(&width) {
        return None;
    }
    if width == 0 {
        return None;
    }
    let mut width = width;
    if width < 0 {
        if to_column {
            width += term_columns();
        }
        if width < 0 {
            return None;
        }
    }
    let padding = if to_column { -width } else { width };
    let mut trunc = PaddingTrunc::None;
    let end_byte = bytes[num_end];
    let consumed_end;
    if end_byte == b',' {
        let tstart = num_end + 1;
        let close = rest[tstart..].find(')').map(|off| tstart + off)?;
        if close == tstart {
            return None;
        }
        let modifier = &rest[tstart..];
        trunc = if modifier.starts_with("trunc)") {
            PaddingTrunc::Right
        } else if modifier.starts_with("ltrunc)") {
            PaddingTrunc::Left
        } else if modifier.starts_with("mtrunc)") {
            PaddingTrunc::Middle
        } else {
            return None;
        };
        consumed_end = close;
    } else {
        consumed_end = num_end;
    }
    // git returns `end - placeholder + 1`; here that's consumed_end + 1 chars
    // measured from the flush char. Since the directive is all ASCII, char
    // count == byte count.
    Some((
        PaddingSpec {
            flush,
            trunc,
            padding,
        },
        consumed_end + 1,
    ))
}

/// Port of pretty.c `parse_wrap_args`/`%w(...)`. `rest` begins at `w`.
fn parse_wrap_placeholder(rest: &str) -> Option<(WrapSpec, usize)> {
    // rest[0] == 'w', rest[1] == '('
    let close = rest.find(')')?;
    let inner = &rest[2..close];
    let mut nums = [0i64; 3];
    let mut count = 0usize;
    if !inner.is_empty() {
        for part in inner.split(',') {
            if count >= 3 {
                return None;
            }
            let (val, consumed) = parse_leading_i64(part);
            // git uses strtoul; a trailing garbage or empty part is tolerated as
            // 0 only when the field is empty. Require the whole part be numeric.
            if consumed != part.len() {
                return None;
            }
            // Overflow guard like git's check against maximum_signed_value_of_type.
            if !(0..=FORMATTING_LIMIT).contains(&val) {
                return None;
            }
            nums[count] = val;
            count += 1;
        }
    }
    Some((
        WrapSpec {
            width: nums[0] as usize,
            indent1: nums[1] as usize,
            indent2: nums[2] as usize,
        },
        close + 1,
    ))
}

fn parse_decorate_spec(opts: &str) -> Option<DecorateSpec> {
    let mut spec = DecorateSpec::default();
    if opts.is_empty() {
        return Some(spec);
    }
    let mut rest = opts;
    loop {
        if rest.is_empty() {
            break;
        }
        if let Some((value, tail)) = decorate_match_value(rest, "prefix") {
            spec.prefix = expand_decorate_value(value);
            rest = tail;
        } else if let Some((value, tail)) = decorate_match_value(rest, "suffix") {
            spec.suffix = expand_decorate_value(value);
            rest = tail;
        } else if let Some((value, tail)) = decorate_match_value(rest, "separator") {
            spec.separator = expand_decorate_value(value);
            rest = tail;
        } else if let Some((value, tail)) = decorate_match_value(rest, "pointer") {
            spec.pointer = expand_decorate_value(value);
            rest = tail;
        } else if let Some((value, tail)) = decorate_match_value(rest, "tag") {
            spec.tag = expand_decorate_value(value);
            rest = tail;
        } else {
            return None;
        }
    }
    Some(spec)
}

/// Match `name=value` (value runs to the next unescaped `,` or end).
fn decorate_match_value<'a>(rest: &'a str, name: &str) -> Option<(&'a str, &'a str)> {
    let after = rest.strip_prefix(name)?;
    let after = after.strip_prefix('=')?;
    // value ends at the next ',' (commas are not escapable here; git uses
    // %x2C inside the value to encode literal commas).
    let end = after.find(',').unwrap_or(after.len());
    let value = &after[..end];
    let tail = &after[end..];
    let tail = tail.strip_prefix(',').unwrap_or(tail);
    Some((value, tail))
}

fn parse_describe_spec(opts: &str) -> Option<DescribeSpec> {
    let mut spec = DescribeSpec::default();
    if opts.is_empty() {
        return Some(spec);
    }
    for part in opts.split(',') {
        if part == "tags" {
            spec.tags = true;
        } else if let Some(v) = part.strip_prefix("abbrev=") {
            spec.abbrev = Some(v.parse::<usize>().ok()?);
        } else if let Some(v) = part.strip_prefix("match=") {
            spec.matches.push(v.to_string());
        } else if let Some(v) = part.strip_prefix("exclude=") {
            spec.excludes.push(v.to_string());
        } else {
            return None;
        }
    }
    Some(spec)
}

/// Parse a leading optionally-signed integer like C strtol; returns
/// (value, bytes_consumed). `bytes_consumed == 0` if no digits.
fn parse_leading_i64(s: &str) -> (i64, usize) {
    let bytes = s.as_bytes();
    let mut idx = 0usize;
    let mut neg = false;
    if matches!(bytes.first(), Some(b'+') | Some(b'-')) {
        neg = bytes[0] == b'-';
        idx += 1;
    }
    let digit_start = idx;
    let mut value: i64 = 0;
    while idx < bytes.len() && bytes[idx].is_ascii_digit() {
        value = value.saturating_mul(10).saturating_add((bytes[idx] - b'0') as i64);
        idx += 1;
    }
    if idx == digit_start {
        return (0, 0);
    }
    if neg {
        value = -value;
    }
    (value, idx)
}

/// git's `expand_string_arg` for decorate values: only `%%` and `%x##`.
fn expand_decorate_value(arg: &str) -> String {
    let bytes = arg.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut idx = 0;
    while idx < bytes.len() {
        if bytes[idx] != b'%' {
            out.push(bytes[idx]);
            idx += 1;
            continue;
        }
        if bytes.get(idx + 1) == Some(&b'%') {
            out.push(b'%');
            idx += 2;
        } else if bytes.get(idx + 1) == Some(&b'x')
            && let (Some(h), Some(l)) = (
                bytes.get(idx + 2).and_then(|b| (*b as char).to_digit(16)),
                bytes.get(idx + 3).and_then(|b| (*b as char).to_digit(16)),
            )
        {
            out.push((h * 16 + l) as u8);
            idx += 4;
        } else {
            out.push(b'%');
            idx += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// git `term_columns()`: respects COLUMNS env, defaults to 80.
pub(crate) fn term_columns() -> i64 {
    if let Ok(cols) = std::env::var("COLUMNS")
        && let Ok(n) = cols.trim().parse::<i64>()
        && n > 0
    {
        return n;
    }
    80
}

fn push_literal(tokens: &mut Vec<FormatToken>, ch: char) {
    if let Some(FormatToken::Literal(last)) = tokens.last_mut() {
        last.push(ch);
    } else {
        tokens.push(FormatToken::Literal(ch.to_string()));
    }
}

fn consume_literal(chars: &mut std::iter::Peekable<std::str::Chars<'_>>, literal: &str) -> bool {
    let mut lookahead = chars.clone();
    for expected in literal.chars() {
        if lookahead.next() != Some(expected) {
            return false;
        }
    }
    *chars = lookahead;
    true
}

fn consume_color(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    tokens: &mut Vec<FormatToken>,
) -> Result<()> {
    if chars.peek().copied() == Some('(') {
        chars.next();
        for ch in chars.by_ref() {
            if ch == ')' {
                tokens.push(FormatToken::ColorParen);
                return Ok(());
            }
        }
        return Err(GitError::Command(
            "unterminated log format placeholder %C".into(),
        ));
    }
    for name in [
        "reset", "normal", "black", "red", "green", "yellow", "blue", "magenta", "cyan", "white",
        "bold", "dim", "ul", "blink", "reverse", "italic", "strike",
    ] {
        if consume_literal(chars, name) {
            tokens.push(FormatToken::ColorName(name));
            return Ok(());
        }
    }
    Err(GitError::Command(
        "unsupported log format placeholder %C".into(),
    ))
}

fn consume_g_placeholder(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    tokens: &mut Vec<FormatToken>,
) -> Result<()> {
    let token = match chars.next() {
        Some('?') => FormatToken::GRefname,
        Some('T') => FormatToken::GTrailers,
        Some('G') => FormatToken::GPlaceholder,
        Some('S') => FormatToken::GSignature,
        Some('K') => FormatToken::GKey,
        Some('F') => FormatToken::GFingerprint,
        Some('P') => FormatToken::GPassthrough,
        Some(other) => {
            return Err(GitError::Command(format!(
                "unsupported log format placeholder %G{other}"
            )));
        }
        None => {
            return Err(GitError::Command(
                "unterminated log format placeholder %G".into(),
            ));
        }
    };
    tokens.push(token);
    Ok(())
}

fn consume_reflog_g_placeholder(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    tokens: &mut Vec<FormatToken>,
) -> Result<()> {
    let token = match chars.next() {
        Some('d') => FormatToken::ReflogGd,
        Some('D') => FormatToken::ReflogGD,
        Some('n') | Some('N') => FormatToken::ReflogGn,
        Some('e') | Some('E') => FormatToken::ReflogGe,
        Some('s') => FormatToken::ReflogGs,
        Some(other) => {
            return Err(GitError::Command(format!(
                "unsupported stash list format placeholder %g{other}"
            )));
        }
        None => {
            return Err(GitError::Command(
                "unterminated stash list format placeholder %g".into(),
            ));
        }
    };
    tokens.push(token);
    Ok(())
}

fn consume_g_date_placeholder(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    tokens: &mut Vec<FormatToken>,
) -> Result<()> {
    let token = match chars.next() {
        Some('D') => FormatToken::GDate,
        Some('d') => FormatToken::GDateShort,
        Some('n') | Some('N') => FormatToken::GDateIso,
        Some('e') | Some('E') => FormatToken::GDateIsoStrict,
        Some('s') => FormatToken::GDateRfc2822,
        Some(other) => {
            return Err(GitError::Command(format!(
                "unsupported log format placeholder %g{other}"
            )));
        }
        None => {
            return Err(GitError::Command(
                "unterminated log format placeholder %g".into(),
            ));
        }
    };
    tokens.push(token);
    Ok(())
}

fn consume_identity_placeholder(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    tokens: &mut Vec<FormatToken>,
    fields: &mut FormatFields,
    author: bool,
) -> Result<()> {
    let field = if author {
        FormatFields::AUTHOR
    } else {
        FormatFields::COMMITTER
    };
    *fields |= field;
    let token = match chars.next() {
        Some('n') | Some('N') if author => FormatToken::AuthorName,
        Some('n') | Some('N') => FormatToken::CommitterName,
        Some('e') | Some('E') if author => FormatToken::AuthorEmail,
        Some('e') | Some('E') => FormatToken::CommitterEmail,
        Some('l') | Some('L') if author => FormatToken::AuthorEmailLocal,
        Some('l') | Some('L') => FormatToken::CommitterEmailLocal,
        Some('t') if author => FormatToken::AuthorTimestamp,
        Some('t') => FormatToken::CommitterTimestamp,
        Some('d') if author => FormatToken::AuthorDate,
        Some('d') => FormatToken::CommitterDate,
        Some('i') if author => FormatToken::AuthorDateIso,
        Some('i') => FormatToken::CommitterDateIso,
        Some('I') if author => FormatToken::AuthorDateIsoStrict,
        Some('I') => FormatToken::CommitterDateIsoStrict,
        Some('s') if author => FormatToken::AuthorDateShort,
        Some('s') => FormatToken::CommitterDateShort,
        Some('D') if author => FormatToken::AuthorDateRfc2822,
        Some('D') => FormatToken::CommitterDateRfc2822,
        Some(other) => {
            let prefix = if author { 'a' } else { 'c' };
            return Err(GitError::Command(format!(
                "unsupported log format placeholder %{prefix}{other}"
            )));
        }
        None => {
            let prefix = if author { 'a' } else { 'c' };
            return Err(GitError::Command(format!(
                "unterminated log format placeholder %{prefix}"
            )));
        }
    };
    tokens.push(token);
    Ok(())
}

pub(crate) mod presets {
    use super::{CompiledLogFormat, LogFormatDialect, Result};

    /// `git log --oneline` / `--pretty=oneline` (%h/%H + optional %d + subject).
    pub(crate) fn log_oneline(
        decorate: bool,
        full_oid: bool,
        parents: bool,
    ) -> Result<CompiledLogFormat> {
        let spec = match (full_oid, decorate) {
            (true, true) => "%H%d %s",
            (true, false) => "%H %s",
            (false, true) => "%h%d %s",
            (false, false) => "%h %s",
        };
        let mut compiled = CompiledLogFormat::compile(spec, LogFormatDialect::Log)?;
        if parents {
            compiled.insert_parents_after_oid();
        }
        Ok(compiled)
    }

    /// `rev-list --oneline`.
    pub(crate) fn rev_list_oneline() -> Result<CompiledLogFormat> {
        CompiledLogFormat::compile("%h %s", LogFormatDialect::RevList)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oid_only_format() {
        let compiled = CompiledLogFormat::compile("%H", LogFormatDialect::Log).unwrap();
        assert!(compiled.is_oid_only());
        assert!(!compiled.uses_decorations());
    }

    #[test]
    fn g_placeholders_are_not_oid_only() {
        let compiled = CompiledLogFormat::compile("%G?|%GS", LogFormatDialect::Log).unwrap();
        assert!(!compiled.is_oid_only());
    }

    #[test]
    fn decorations_tier_is_full() {
        let compiled = CompiledLogFormat::compile("%h %d", LogFormatDialect::Log).unwrap();
        assert_eq!(compiled.tier(), FormatTier::Full);
        assert!(compiled.uses_decorations());
    }

    #[test]
    fn metadata_tier() {
        let compiled = CompiledLogFormat::compile("%H %P", LogFormatDialect::RevList).unwrap();
        assert_eq!(compiled.tier(), FormatTier::Metadata);
        assert!(compiled.is_metadata_emitable());
    }

    #[test]
    fn metadata_not_emitable_with_subject() {
        let compiled = CompiledLogFormat::compile("%H %s", LogFormatDialect::Log).unwrap();
        assert!(!compiled.is_metadata_emitable());
    }

    #[test]
    fn log_oneline_preset_inserts_parents() {
        let compiled = presets::log_oneline(false, false, true).unwrap();
        assert!(compiled.tokens.windows(3).any(|w| {
            matches!(w[0], FormatToken::OidAbbrev)
                && matches!(w[1], FormatToken::Literal(ref text) if text == " ")
                && matches!(w[2], FormatToken::ParentsAbbrev)
        }));
    }

    #[test]
    fn escaped_percent_before_literal() {
        let compiled = CompiledLogFormat::compile("%%H", LogFormatDialect::Log).unwrap();
        assert_eq!(
            compiled.tokens,
            vec![FormatToken::Percent, FormatToken::Literal("H".into())]
        );
        assert!(!compiled.is_oid_only());
    }

    #[test]
    fn log_format_gs_is_reflog_subject() {
        let compiled = CompiledLogFormat::compile("%gs", LogFormatDialect::Log).unwrap();
        assert_eq!(compiled.tokens, vec![FormatToken::ReflogGs]);
    }
}
