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
                Some('g') if dialect == LogFormatDialect::Stash => {
                    consume_stash_g_placeholder(&mut chars, &mut tokens)?;
                }
                Some('g') => consume_g_date_placeholder(&mut chars, &mut tokens)?,
                Some('a') => consume_identity_placeholder(
                    &mut chars,
                    &mut tokens,
                    &mut fields,
                    true,
                )?,
                Some('c') => consume_identity_placeholder(
                    &mut chars,
                    &mut tokens,
                    &mut fields,
                    false,
                )?,
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

    /// True when the format emits only full oids (`%H`) plus inert literals/newlines.
    pub(crate) fn is_oid_only(&self) -> bool {
        self.tokens.iter().any(|token| *token == FormatToken::OidFull)
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
                    self.tokens.insert(index + 1, FormatToken::Literal(" ".into()));
                    self.tokens.insert(index + 2, FormatToken::ParentsFull);
                    self.fields |= FormatFields::PARENTS;
                    return;
                }
                FormatToken::OidAbbrev => {
                    self.tokens.insert(index + 1, FormatToken::Literal(" ".into()));
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

/// Skip a `%C` color placeholder (stash list reuses this helper).
pub(crate) fn consume_log_format_color(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
) -> Result<()> {
    consume_color(chars, &mut Vec::new())
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

fn consume_stash_g_placeholder(
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

    /// Plain `rev-list` / `log --format=%H` oid listing.
    pub(crate) fn oid_line() -> Result<CompiledLogFormat> {
        CompiledLogFormat::compile("%H", LogFormatDialect::RevList)
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
        let compiled =
            CompiledLogFormat::compile("%G?|%GS", LogFormatDialect::Log).unwrap();
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
        assert!(compiled
            .tokens
            .windows(3)
            .any(|w| {
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
}