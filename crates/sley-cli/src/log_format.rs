//! Compiled `git log` / `rev-list` pretty-format strings.
//!
//! Formats are parsed once at CLI arg time into [`CompiledLogFormat`] (a token
//! stream + a [`FormatTier`] that describes how much commit data emission needs).
//! Command fast paths consult the tier instead of hand-maintained string guards.

use sley_core::{GitError, Result};
use sley_strbuf_expand::{
    AtomSyntax, AtomTable, ExpandFormat, ExpandOptions, ExpandSegment, LiteralHex,
};
use std::cell::Cell;

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

/// A parsed `%<`/`%>`/... padding placeholder.
pub(crate) type PaddingSpec = sley_strbuf_expand::PaddingSpec;

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
    pub(crate) expand: ExpandFormat<FormatToken>,
    token_segments: Vec<usize>,
}

impl CompiledLogFormat {
    pub(crate) fn compile(format: &str, dialect: LogFormatDialect) -> Result<Self> {
        let table = LogFormatAtomTable {
            dialect,
            fields: Cell::new(FormatFields::default()),
        };
        let expand = ExpandFormat::parse_with_options(
            format,
            &table,
            ExpandOptions {
                atom_syntax: AtomSyntax::Prefixed,
                literal_hex: LiteralHex::None,
            },
        )?;
        let fields = table.fields.get();
        let (tokens, token_segments) = tokens_from_expand(&expand);
        Ok(Self {
            tokens,
            fields,
            dialect,
            expand,
            token_segments,
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
                    self.rebuild_expand_from_tokens();
                    return;
                }
                FormatToken::OidAbbrev => {
                    self.tokens
                        .insert(index + 1, FormatToken::Literal(" ".into()));
                    self.tokens.insert(index + 2, FormatToken::ParentsAbbrev);
                    self.fields |= FormatFields::PARENTS;
                    self.rebuild_expand_from_tokens();
                    return;
                }
                _ => {}
            }
        }
    }

    fn rebuild_expand_from_tokens(&mut self) {
        self.expand = expand_from_tokens(&self.tokens);
        self.token_segments = token_segments_from_expand(&self.expand);
    }

    pub(crate) fn segment_range_for_tokens(&self, token_range: std::ops::Range<usize>) -> std::ops::Range<usize> {
        let start = self
            .token_segments
            .get(token_range.start)
            .copied()
            .unwrap_or(self.expand.segments().len());
        let end = self
            .token_segments
            .get(token_range.end)
            .copied()
            .unwrap_or(self.expand.segments().len());
        start..end
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

struct LogFormatAtomTable {
    dialect: LogFormatDialect,
    fields: Cell<FormatFields>,
}

impl LogFormatAtomTable {
    fn add_fields(&self, fields: FormatFields) {
        let mut current = self.fields.get();
        current |= fields;
        self.fields.set(current);
    }
}

impl AtomTable for LogFormatAtomTable {
    type Atom = FormatToken;

    fn parse_atom(&self, value: &str) -> Result<Self::Atom> {
        Ok(FormatToken::Literal(format!("%({value})")))
    }

    fn parse_prefix_atom(&self, value: &str) -> Result<Option<(Self::Atom, usize)>> {
        let Some(first) = value.chars().next() else {
            return Err(GitError::Command(
                "unterminated log format placeholder %".into(),
            ));
        };
        if first == '(' {
            return Ok(parse_parenthesized_atom(value, self)?);
        }
        if first == '<' || first == '>' {
            return Ok(None);
        }
        if first == 'w' {
            if value.as_bytes().get(1) != Some(&b'(') {
                return Err(GitError::Command(
                    "unsupported log format placeholder %w".into(),
                ));
            }
            return Ok(parse_wrap_placeholder(value)
                .map(|(spec, consumed)| (FormatToken::Wrap(spec), consumed)));
        }
        if first == 'x' {
            let bytes = value.as_bytes();
            if let (Some(high), Some(low)) = (bytes.get(1), bytes.get(2))
                && let (Some(high), Some(low)) = (
                    (*high as char).to_digit(16),
                    (*low as char).to_digit(16),
                )
            {
                return Ok(Some((FormatToken::HexByte(((high << 4) | low) as u8), 3)));
            }
            return Ok(Some((FormatToken::Literal("%x".into()), 1)));
        }
        let token = match first {
            'H' => {
                self.add_fields(FormatFields::OID);
                FormatToken::OidFull
            }
            'h' => {
                self.add_fields(FormatFields::OID);
                FormatToken::OidAbbrev
            }
            'T' => {
                self.add_fields(FormatFields::TREE);
                FormatToken::TreeFull
            }
            't' => {
                self.add_fields(FormatFields::TREE);
                FormatToken::TreeAbbrev
            }
            'P' => {
                self.add_fields(FormatFields::PARENTS);
                FormatToken::ParentsFull
            }
            'p' => {
                self.add_fields(FormatFields::PARENTS);
                FormatToken::ParentsAbbrev
            }
            'm' => FormatToken::Marker,
            's' => {
                self.add_fields(FormatFields::SUBJECT);
                FormatToken::Subject
            }
            'f' => {
                self.add_fields(FormatFields::SUBJECT);
                FormatToken::SanitizedSubject
            }
            'e' => {
                self.add_fields(FormatFields::ENCODING);
                FormatToken::Encoding
            }
            'N' => FormatToken::NoteName,
            'S' => {
                self.add_fields(FormatFields::REV_SOURCE);
                FormatToken::RevisionSource
            }
            'C' => return parse_color_atom(value).map(Some),
            'b' => {
                self.add_fields(FormatFields::BODY);
                FormatToken::Body
            }
            'B' => {
                self.add_fields(FormatFields::BODY);
                FormatToken::FullMessage
            }
            'd' if self.dialect == LogFormatDialect::Stash => FormatToken::StashDecoParen,
            'd' => {
                self.add_fields(FormatFields::DECORATIONS);
                FormatToken::DecorationsParen
            }
            'D' if self.dialect == LogFormatDialect::Stash => FormatToken::StashDecoBare,
            'D' => {
                self.add_fields(FormatFields::DECORATIONS);
                FormatToken::DecorationsBare
            }
            'G' => return parse_g_atom(value).map(Some),
            'g' if matches!(self.dialect, LogFormatDialect::Stash | LogFormatDialect::Log) => {
                return parse_reflog_g_atom(value).map(Some);
            }
            'g' => return parse_g_date_atom(value).map(Some),
            'a' => return parse_identity_atom(value, self, true).map(Some),
            'c' => return parse_identity_atom(value, self, false).map(Some),
            'n' => FormatToken::Newline,
            other => {
                return Err(GitError::Command(format!(
                    "unsupported log format placeholder %{other}"
                )));
            }
        };
        Ok(Some((token, first.len_utf8())))
    }
}

fn parse_parenthesized_atom(
    value: &str,
    table: &LogFormatAtomTable,
) -> Result<Option<(FormatToken, usize)>> {
    let Some(end) = value.find(')') else {
        return Ok(None);
    };
    let inner = &value[1..end];
    let consumed = end + 1;
    let literal = || Some((FormatToken::Literal(format!("%({inner})")), consumed));
    if let Some(opts) = inner.strip_prefix("trailers") {
        let opts = opts.strip_prefix(':').unwrap_or("");
        if !(inner == "trailers" || inner.starts_with("trailers:"))
            || (!opts.is_empty()
                && crate::commands::for_each_ref::parse_for_each_ref_trailer_options(opts).is_err())
        {
            return Ok(literal());
        }
        table.add_fields(FormatFields::BODY);
        Ok(Some((FormatToken::Trailers(opts.to_string()), consumed)))
    } else if inner == "decorate" || inner.starts_with("decorate:") {
        let opts = inner.strip_prefix("decorate").unwrap_or("");
        let opts = opts.strip_prefix(':').unwrap_or("");
        match parse_decorate_spec(opts) {
            Some(spec) => {
                table.add_fields(FormatFields::DECORATIONS);
                Ok(Some((FormatToken::Decorate(spec), consumed)))
            }
            None => Ok(literal()),
        }
    } else if inner == "describe" || inner.starts_with("describe:") {
        let opts = inner.strip_prefix("describe").unwrap_or("");
        let opts = opts.strip_prefix(':').unwrap_or("");
        match parse_describe_spec(opts) {
            Some(spec) => {
                table.add_fields(FormatFields::BODY);
                Ok(Some((FormatToken::Describe(spec), consumed)))
            }
            None => Ok(literal()),
        }
    } else {
        Ok(literal())
    }
}

fn parse_color_atom(value: &str) -> Result<(FormatToken, usize)> {
    if value.as_bytes().get(1) == Some(&b'(') {
        let Some(end) = value.find(')') else {
            return Err(GitError::Command(
                "unterminated log format placeholder %C".into(),
            ));
        };
        return Ok((FormatToken::ColorParen, end + 1));
    }
    for name in [
        "reset", "normal", "black", "red", "green", "yellow", "blue", "magenta", "cyan", "white",
        "bold", "dim", "ul", "blink", "reverse", "italic", "strike",
    ] {
        if value[1..].starts_with(name) {
            return Ok((FormatToken::ColorName(name), 1 + name.len()));
        }
    }
    Err(GitError::Command(
        "unsupported log format placeholder %C".into(),
    ))
}

fn parse_g_atom(value: &str) -> Result<(FormatToken, usize)> {
    let token = match value.chars().nth(1) {
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
    Ok((token, 2))
}

fn parse_reflog_g_atom(value: &str) -> Result<(FormatToken, usize)> {
    let token = match value.chars().nth(1) {
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
    Ok((token, 2))
}

fn parse_g_date_atom(value: &str) -> Result<(FormatToken, usize)> {
    let token = match value.chars().nth(1) {
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
    Ok((token, 2))
}

fn parse_identity_atom(
    value: &str,
    table: &LogFormatAtomTable,
    author: bool,
) -> Result<(FormatToken, usize)> {
    table.add_fields(if author {
        FormatFields::AUTHOR
    } else {
        FormatFields::COMMITTER
    });
    let token = match value.chars().nth(1) {
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
    Ok((token, 2))
}

fn tokens_from_expand(
    expand: &ExpandFormat<FormatToken>,
) -> (Vec<FormatToken>, Vec<usize>) {
    let mut tokens = Vec::new();
    let mut token_segments = Vec::new();
    for (segment_index, segment) in expand.segments().iter().enumerate() {
        match segment {
            ExpandSegment::Literal(literal) => {
                push_literal_bytes(&mut tokens, &mut token_segments, literal, segment_index);
            }
            ExpandSegment::Padding(spec) => {
                tokens.push(FormatToken::Padding(*spec));
                token_segments.push(segment_index);
            }
            ExpandSegment::Atom(atom) => {
                if let Some(magic) = token_magic(atom.magic) {
                    tokens.push(FormatToken::Magic(magic));
                    token_segments.push(segment_index);
                }
                tokens.push(atom.atom.clone());
                token_segments.push(segment_index);
            }
        }
    }
    (tokens, token_segments)
}

fn token_segments_from_expand(expand: &ExpandFormat<FormatToken>) -> Vec<usize> {
    tokens_from_expand(expand).1
}

fn push_literal_bytes(
    tokens: &mut Vec<FormatToken>,
    token_segments: &mut Vec<usize>,
    literal: &[u8],
    segment_index: usize,
) {
    let mut start = 0usize;
    for (idx, byte) in literal.iter().enumerate() {
        if *byte != b'%' {
            continue;
        }
        push_literal_chunk(tokens, token_segments, &literal[start..idx], segment_index);
        tokens.push(FormatToken::Percent);
        token_segments.push(segment_index);
        start = idx + 1;
    }
    push_literal_chunk(tokens, token_segments, &literal[start..], segment_index);
}

fn push_literal_chunk(
    tokens: &mut Vec<FormatToken>,
    token_segments: &mut Vec<usize>,
    chunk: &[u8],
    segment_index: usize,
) {
    if chunk.is_empty() {
        return;
    }
    match std::str::from_utf8(chunk) {
        Ok(text) => {
            tokens.push(FormatToken::Literal(text.to_string()));
            token_segments.push(segment_index);
        }
        Err(_) => {
            for byte in chunk {
                tokens.push(FormatToken::HexByte(*byte));
                token_segments.push(segment_index);
            }
        }
    }
}

fn expand_from_tokens(tokens: &[FormatToken]) -> ExpandFormat<FormatToken> {
    let mut segments = Vec::new();
    let mut magic = sley_strbuf_expand::MagicPrefix::None;
    for token in tokens {
        match token {
            FormatToken::Literal(text) => push_expand_literal(&mut segments, text.as_bytes()),
            FormatToken::Percent => push_expand_literal(&mut segments, b"%"),
            FormatToken::HexByte(byte) => push_expand_literal(&mut segments, &[*byte]),
            FormatToken::Padding(spec) => segments.push(ExpandSegment::Padding(*spec)),
            FormatToken::Magic(prefix) => magic = expand_magic(*prefix),
            other => {
                segments.push(ExpandSegment::Atom(sley_strbuf_expand::ExpandAtom {
                    magic,
                    atom: other.clone(),
                }));
                magic = sley_strbuf_expand::MagicPrefix::None;
            }
        }
    }
    ExpandFormat::from_segments(segments)
}

fn push_expand_literal(segments: &mut Vec<ExpandSegment<FormatToken>>, literal: &[u8]) {
    if literal.is_empty() {
        return;
    }
    if let Some(ExpandSegment::Literal(previous)) = segments.last_mut() {
        previous.extend_from_slice(literal);
    } else {
        segments.push(ExpandSegment::Literal(literal.to_vec()));
    }
}

fn token_magic(prefix: sley_strbuf_expand::MagicPrefix) -> Option<MagicPrefix> {
    match prefix {
        sley_strbuf_expand::MagicPrefix::None => None,
        sley_strbuf_expand::MagicPrefix::AddLfBeforeNonEmpty => {
            Some(MagicPrefix::AddLfBeforeNonEmpty)
        }
        sley_strbuf_expand::MagicPrefix::DeleteLfBeforeEmpty => Some(MagicPrefix::DelLfBeforeEmpty),
        sley_strbuf_expand::MagicPrefix::AddSpaceBeforeNonEmpty => {
            Some(MagicPrefix::AddSpBeforeNonEmpty)
        }
    }
}

fn expand_magic(prefix: MagicPrefix) -> sley_strbuf_expand::MagicPrefix {
    match prefix {
        MagicPrefix::DelLfBeforeEmpty => sley_strbuf_expand::MagicPrefix::DeleteLfBeforeEmpty,
        MagicPrefix::AddLfBeforeNonEmpty => sley_strbuf_expand::MagicPrefix::AddLfBeforeNonEmpty,
        MagicPrefix::AddSpBeforeNonEmpty => sley_strbuf_expand::MagicPrefix::AddSpaceBeforeNonEmpty,
    }
}

/// Limit padding/wrap widths the way git's FORMATTING_LIMIT does, so an
/// overflowing directive is rejected (emitted verbatim) instead of allocating.
const FORMATTING_LIMIT: i64 = 1 << 30;

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
        value = value
            .saturating_mul(10)
            .saturating_add((bytes[idx] - b'0') as i64);
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
    sley_strbuf_expand::term_columns() as i64
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
