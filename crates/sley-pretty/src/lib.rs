//! Public pre-1.0 API surface for compiling pretty-format strings.
//!
//! The compiler in this crate keeps tokens borrowed from the input format
//! string wherever possible. Rendering is expressed as a streaming interaction
//! between [`CompiledPrettyFormat`], a [`PrettyValueSource`], and a [`PrettySink`]:
//! command code can parse once, then write atom values directly to stdout,
//! buffers, or pagers without materializing a full commit message first.
//!
//! This is a skeleton for later extraction from the current CLI implementation.
//! It recognizes the common atoms needed to stabilize the public surface and
//! preserves unknown atoms by default so the real compiler can grow in place.

#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::fmt;
use std::io::{self, Write};

/// Pretty-format dialect that controls atom compatibility.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum PrettyDialect {
    /// `git log --pretty=format:...`.
    #[default]
    Log,
    /// `git rev-list --format=...`.
    RevList,
    /// `git stash list` reflog-oriented pretty formats.
    Stash,
}

/// Compile-time options for a pretty-format string.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct PrettyCompileOptions {
    /// Dialect whose atoms should be accepted.
    pub dialect: PrettyDialect,
    /// Reject unknown atoms instead of preserving them as [`PrettyAtom::Raw`].
    pub strict_unknown_atoms: bool,
}

/// A compiled pretty-format program that borrows from its source string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledPrettyFormat<'fmt> {
    source: &'fmt str,
    tokens: Vec<PrettyToken<'fmt>>,
    requirements: PrettyRequirements,
}

impl<'fmt> CompiledPrettyFormat<'fmt> {
    /// Compile a format using default options.
    pub fn compile(source: &'fmt str) -> Result<Self, PrettyCompileError<'fmt>> {
        Self::compile_with_options(source, PrettyCompileOptions::default())
    }

    /// Compile a format using explicit options.
    pub fn compile_with_options(
        source: &'fmt str,
        options: PrettyCompileOptions,
    ) -> Result<Self, PrettyCompileError<'fmt>> {
        let mut parser = Parser {
            source,
            options,
            tokens: Vec::new(),
            requirements: PrettyRequirements::empty(),
        };
        parser.parse()?;
        Ok(Self {
            source,
            tokens: parser.tokens,
            requirements: parser.requirements,
        })
    }

    /// Original format string borrowed by this compiled value.
    #[must_use]
    pub const fn source(&self) -> &'fmt str {
        self.source
    }

    /// Borrowed token stream.
    #[must_use]
    pub fn tokens(&self) -> &[PrettyToken<'fmt>] {
        &self.tokens
    }

    /// Minimum commit data required by the token stream.
    #[must_use]
    pub const fn requirements(&self) -> PrettyRequirements {
        self.requirements
    }

    /// Stream this format into `out`, resolving atoms on demand.
    pub fn write_to<S, V>(&self, out: &mut S, values: &mut V) -> io::Result<()>
    where
        S: PrettySink,
        V: PrettyValueSource,
    {
        for token in &self.tokens {
            match *token {
                PrettyToken::Literal(literal) => out.write_bytes(literal.as_bytes())?,
                PrettyToken::Percent => out.write_bytes(b"%")?,
                PrettyToken::Newline => out.write_bytes(b"\n")?,
                PrettyToken::HexByte(byte) => out.write_bytes(&[byte])?,
                PrettyToken::Atom { magic, atom } => values.write_atom(atom, magic, out)?,
            }
        }
        Ok(())
    }
}

/// A compiled pretty-format token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrettyToken<'fmt> {
    /// Literal bytes borrowed from the format string.
    Literal(&'fmt str),
    /// `%%`.
    Percent,
    /// `%n`.
    Newline,
    /// `%xNN`.
    HexByte(u8),
    /// A placeholder atom, possibly with Git's magic prefix.
    Atom {
        /// Magic prefix attached to this atom.
        magic: PrettyMagic,
        /// Atom to resolve.
        atom: PrettyAtom<'fmt>,
    },
}

/// Git's per-placeholder magic prefix.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum PrettyMagic {
    /// No magic prefix.
    #[default]
    None,
    /// `%+`: add a newline before non-empty output.
    AddLfBeforeNonEmpty,
    /// `%-`: delete preceding linefeeds when output is empty.
    DeleteLfBeforeEmpty,
    /// `% `: add a space before non-empty output.
    AddSpaceBeforeNonEmpty,
}

/// Pretty atom recognized by the public compiler surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrettyAtom<'fmt> {
    /// `%H`.
    FullOid,
    /// `%h`.
    AbbrevOid,
    /// `%T`.
    TreeOid,
    /// `%t`.
    AbbrevTreeOid,
    /// `%P`.
    ParentOids,
    /// `%p`.
    AbbrevParentOids,
    /// `%m`.
    LeftRightMarker,
    /// `%s`.
    Subject,
    /// `%f`.
    SanitizedSubject,
    /// `%B`.
    Body,
    /// `%an`.
    AuthorName,
    /// `%ae`.
    AuthorEmail,
    /// `%at`.
    AuthorTimestamp,
    /// `%cn`.
    CommitterName,
    /// `%ce`.
    CommitterEmail,
    /// `%ct`.
    CommitterTimestamp,
    /// `%(...)` or an atom not yet modeled by this skeleton.
    Raw(&'fmt str),
}

/// Coarse set of commit fields needed to render a format.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PrettyRequirements(u16);

impl PrettyRequirements {
    /// Full or abbreviated commit object id.
    pub const OID: Self = Self(1 << 0);
    /// Tree object id.
    pub const TREE: Self = Self(1 << 1);
    /// Parent object ids.
    pub const PARENTS: Self = Self(1 << 2);
    /// Subject line.
    pub const SUBJECT: Self = Self(1 << 3);
    /// Full commit message body.
    pub const BODY: Self = Self(1 << 4);
    /// Author identity or date.
    pub const AUTHOR: Self = Self(1 << 5);
    /// Committer identity or date.
    pub const COMMITTER: Self = Self(1 << 6);
    /// Decorations, signatures, trailers, or other late-bound data.
    pub const EXTENDED: Self = Self(1 << 7);

    /// Empty requirement set.
    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    /// Whether every bit in `other` is present.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether any bit in `other` is present.
    #[must_use]
    pub const fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    /// Return the union of two requirement sets.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Convert the bit set into a broad loading tier.
    #[must_use]
    pub const fn tier(self) -> PrettyFormatTier {
        if self.intersects(
            Self::BODY
                .union(Self::AUTHOR)
                .union(Self::COMMITTER)
                .union(Self::EXTENDED),
        ) {
            PrettyFormatTier::Full
        } else if self.intersects(Self::TREE.union(Self::SUBJECT)) {
            PrettyFormatTier::Header
        } else if self.intersects(Self::OID.union(Self::PARENTS)) {
            PrettyFormatTier::Metadata
        } else {
            PrettyFormatTier::LiteralOnly
        }
    }
}

impl std::ops::BitOr for PrettyRequirements {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        self.union(rhs)
    }
}

impl std::ops::BitOrAssign for PrettyRequirements {
    fn bitor_assign(&mut self, rhs: Self) {
        *self = self.union(rhs);
    }
}

/// Approximate amount of commit data needed to render a format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PrettyFormatTier {
    /// Only literals, percent escapes, newlines, and hex bytes.
    LiteralOnly,
    /// Object ids and parent metadata.
    Metadata,
    /// Parsed commit header is enough.
    Header,
    /// Full commit body or late-bound decoration/signature/trailer data.
    Full,
}

/// Byte sink used by streaming pretty renderers.
pub trait PrettySink {
    /// Write bytes to the sink.
    fn write_bytes(&mut self, bytes: &[u8]) -> io::Result<()>;
}

impl<T: Write + ?Sized> PrettySink for T {
    fn write_bytes(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.write_all(bytes)
    }
}

/// Atom value provider used by [`CompiledPrettyFormat::write_to`].
///
/// The atom is borrowed from the compiled format. Implementations should write
/// directly into `out` and avoid allocating unless a specific atom requires it.
pub trait PrettyValueSource {
    /// Resolve one atom into a byte sink.
    fn write_atom(
        &mut self,
        atom: PrettyAtom<'_>,
        magic: PrettyMagic,
        out: &mut dyn PrettySink,
    ) -> io::Result<()>;
}

/// Pretty-format compile error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrettyCompileError<'fmt> {
    kind: PrettyCompileErrorKind<'fmt>,
    offset: usize,
}

impl<'fmt> PrettyCompileError<'fmt> {
    /// Error kind.
    #[must_use]
    pub const fn kind(&self) -> &PrettyCompileErrorKind<'fmt> {
        &self.kind
    }

    /// Byte offset in the source format where parsing failed.
    #[must_use]
    pub const fn offset(&self) -> usize {
        self.offset
    }
}

impl fmt::Display for PrettyCompileError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            PrettyCompileErrorKind::UnexpectedEnd => {
                write!(f, "pretty format ended after `%` at byte {}", self.offset)
            }
            PrettyCompileErrorKind::InvalidHex(hex) => {
                write!(f, "invalid pretty-format hex escape `{hex}`")
            }
            PrettyCompileErrorKind::UnterminatedParenthesizedAtom => {
                write!(
                    f,
                    "unterminated parenthesized pretty atom at byte {}",
                    self.offset
                )
            }
            PrettyCompileErrorKind::UnknownAtom(atom) => {
                write!(f, "unknown pretty-format atom `{atom}`")
            }
        }
    }
}

impl std::error::Error for PrettyCompileError<'_> {}

/// Specific compile failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrettyCompileErrorKind<'fmt> {
    /// The format ended immediately after `%`.
    UnexpectedEnd,
    /// `%xNN` did not contain two hex digits.
    InvalidHex(&'fmt str),
    /// `%(...)` was not closed.
    UnterminatedParenthesizedAtom,
    /// Unknown atom in strict mode.
    UnknownAtom(&'fmt str),
}

struct Parser<'fmt> {
    source: &'fmt str,
    options: PrettyCompileOptions,
    tokens: Vec<PrettyToken<'fmt>>,
    requirements: PrettyRequirements,
}

impl<'fmt> Parser<'fmt> {
    fn parse(&mut self) -> Result<(), PrettyCompileError<'fmt>> {
        let bytes = self.source.as_bytes();
        let mut literal_start = 0usize;
        let mut cursor = 0usize;

        while cursor < bytes.len() {
            if bytes[cursor] != b'%' {
                cursor += 1;
                continue;
            }

            if literal_start < cursor {
                self.tokens
                    .push(PrettyToken::Literal(&self.source[literal_start..cursor]));
            }

            let percent = cursor;
            cursor += 1;
            if cursor >= bytes.len() {
                return Err(self.error(PrettyCompileErrorKind::UnexpectedEnd, percent));
            }

            let (magic, atom_cursor) = parse_magic(bytes, cursor);
            cursor = atom_cursor;
            if cursor >= bytes.len() {
                return Err(self.error(PrettyCompileErrorKind::UnexpectedEnd, percent));
            }

            match bytes[cursor] {
                b'%' if magic == PrettyMagic::None => {
                    self.tokens.push(PrettyToken::Percent);
                    cursor += 1;
                }
                b'n' if magic == PrettyMagic::None => {
                    self.tokens.push(PrettyToken::Newline);
                    cursor += 1;
                }
                b'x' if magic == PrettyMagic::None => {
                    let end = cursor.saturating_add(3);
                    if end > bytes.len() {
                        return Err(self.error(
                            PrettyCompileErrorKind::InvalidHex(&self.source[cursor..]),
                            cursor,
                        ));
                    }
                    let hex = &self.source[cursor + 1..end];
                    let Some(byte) = parse_hex_byte(hex.as_bytes()) else {
                        return Err(self.error(PrettyCompileErrorKind::InvalidHex(hex), cursor));
                    };
                    self.tokens.push(PrettyToken::HexByte(byte));
                    cursor = end;
                }
                b'(' => {
                    let atom_start = cursor + 1;
                    let Some(close_offset) = bytes[atom_start..].iter().position(|b| *b == b')')
                    else {
                        return Err(self.error(
                            PrettyCompileErrorKind::UnterminatedParenthesizedAtom,
                            cursor,
                        ));
                    };
                    let atom_end = atom_start + close_offset;
                    let raw = &self.source[atom_start..atom_end];
                    self.push_atom(raw, magic, cursor)?;
                    cursor = atom_end + 1;
                }
                _ => {
                    let (raw, next) = read_prefixed_atom(self.source, cursor);
                    self.push_atom(raw, magic, cursor)?;
                    cursor = next;
                }
            }

            literal_start = cursor;
        }

        if literal_start < self.source.len() {
            self.tokens
                .push(PrettyToken::Literal(&self.source[literal_start..]));
        }

        Ok(())
    }

    fn push_atom(
        &mut self,
        raw: &'fmt str,
        magic: PrettyMagic,
        offset: usize,
    ) -> Result<(), PrettyCompileError<'fmt>> {
        let atom = parse_atom(raw);
        if matches!(atom, PrettyAtom::Raw(_)) && self.options.strict_unknown_atoms {
            return Err(self.error(PrettyCompileErrorKind::UnknownAtom(raw), offset));
        }
        self.requirements |= requirements_for_atom(atom);
        self.tokens.push(PrettyToken::Atom { magic, atom });
        Ok(())
    }

    const fn error(
        &self,
        kind: PrettyCompileErrorKind<'fmt>,
        offset: usize,
    ) -> PrettyCompileError<'fmt> {
        PrettyCompileError { kind, offset }
    }
}

fn parse_magic(bytes: &[u8], cursor: usize) -> (PrettyMagic, usize) {
    match bytes[cursor] {
        b'+' => (PrettyMagic::AddLfBeforeNonEmpty, cursor + 1),
        b'-' => (PrettyMagic::DeleteLfBeforeEmpty, cursor + 1),
        b' ' => (PrettyMagic::AddSpaceBeforeNonEmpty, cursor + 1),
        _ => (PrettyMagic::None, cursor),
    }
}

fn read_prefixed_atom(source: &str, cursor: usize) -> (&str, usize) {
    let bytes = source.as_bytes();
    if cursor + 1 < bytes.len() {
        let first = bytes[cursor];
        let second = bytes[cursor + 1];
        if matches!(first, b'a' | b'c') && second.is_ascii_alphabetic() {
            return (&source[cursor..cursor + 2], cursor + 2);
        }
    }
    let Some(ch) = source[cursor..].chars().next() else {
        return (&source[cursor..cursor], cursor);
    };
    let end = cursor + ch.len_utf8();
    (&source[cursor..end], end)
}

fn parse_atom(raw: &str) -> PrettyAtom<'_> {
    match raw {
        "H" => PrettyAtom::FullOid,
        "h" => PrettyAtom::AbbrevOid,
        "T" => PrettyAtom::TreeOid,
        "t" => PrettyAtom::AbbrevTreeOid,
        "P" => PrettyAtom::ParentOids,
        "p" => PrettyAtom::AbbrevParentOids,
        "m" => PrettyAtom::LeftRightMarker,
        "s" => PrettyAtom::Subject,
        "f" => PrettyAtom::SanitizedSubject,
        "B" => PrettyAtom::Body,
        "an" => PrettyAtom::AuthorName,
        "ae" => PrettyAtom::AuthorEmail,
        "at" => PrettyAtom::AuthorTimestamp,
        "cn" => PrettyAtom::CommitterName,
        "ce" => PrettyAtom::CommitterEmail,
        "ct" => PrettyAtom::CommitterTimestamp,
        other => PrettyAtom::Raw(other),
    }
}

const fn requirements_for_atom(atom: PrettyAtom<'_>) -> PrettyRequirements {
    match atom {
        PrettyAtom::FullOid | PrettyAtom::AbbrevOid => PrettyRequirements::OID,
        PrettyAtom::TreeOid | PrettyAtom::AbbrevTreeOid => PrettyRequirements::TREE,
        PrettyAtom::ParentOids | PrettyAtom::AbbrevParentOids | PrettyAtom::LeftRightMarker => {
            PrettyRequirements::PARENTS
        }
        PrettyAtom::Subject | PrettyAtom::SanitizedSubject => PrettyRequirements::SUBJECT,
        PrettyAtom::Body => PrettyRequirements::BODY,
        PrettyAtom::AuthorName | PrettyAtom::AuthorEmail | PrettyAtom::AuthorTimestamp => {
            PrettyRequirements::AUTHOR
        }
        PrettyAtom::CommitterName | PrettyAtom::CommitterEmail | PrettyAtom::CommitterTimestamp => {
            PrettyRequirements::COMMITTER
        }
        PrettyAtom::Raw(_) => PrettyRequirements::EXTENDED,
    }
}

fn parse_hex_byte(hex: &[u8]) -> Option<u8> {
    if hex.len() != 2 {
        return None;
    }
    Some(hex_value(hex[0])? << 4 | hex_value(hex[1])?)
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StaticValues;

    impl PrettyValueSource for StaticValues {
        fn write_atom(
            &mut self,
            atom: PrettyAtom<'_>,
            _magic: PrettyMagic,
            out: &mut dyn PrettySink,
        ) -> io::Result<()> {
            let value = match atom {
                PrettyAtom::FullOid => b"OID".as_slice(),
                PrettyAtom::Subject => b"subject".as_slice(),
                _ => b"?".as_slice(),
            };
            out.write_bytes(value)
        }
    }

    #[test]
    fn compile_borrows_tokens_and_records_requirements() -> Result<(), PrettyCompileError<'static>>
    {
        let compiled = CompiledPrettyFormat::compile("commit %H %s%n")?;

        assert_eq!(compiled.source(), "commit %H %s%n");
        assert!(
            compiled
                .requirements()
                .contains(PrettyRequirements::OID | PrettyRequirements::SUBJECT)
        );
        assert_eq!(compiled.requirements().tier(), PrettyFormatTier::Header);
        assert!(matches!(
            compiled.tokens()[1],
            PrettyToken::Atom {
                atom: PrettyAtom::FullOid,
                ..
            }
        ));
        Ok(())
    }

    #[test]
    fn write_to_streams_literals_and_atoms() -> io::Result<()> {
        let compiled = match CompiledPrettyFormat::compile("id:%H%% %s%x21") {
            Ok(compiled) => compiled,
            Err(err) => panic!("{err}"),
        };
        let mut values = StaticValues;
        let mut out = Vec::new();

        compiled.write_to(&mut out, &mut values)?;

        assert_eq!(out, b"id:OID% subject!");
        Ok(())
    }
}
