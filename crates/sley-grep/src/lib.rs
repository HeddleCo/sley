//! Public pre-1.0 API surface for grep matching.
//!
//! This crate defines the matcher boundary that later extraction can fill with
//! the existing POSIX/fixed/PCRE-compatible logic. Patterns are borrowed at
//! compile time, the compiled matcher owns only reusable search state, and each
//! search borrows its haystack while streaming [`MatchEvent`] values into a
//! [`MatchSink`].
//!
//! The current implementation intentionally supports fixed-string matching
//! only. Regex pattern kinds are modeled in the public API and return an
//! explicit [`GrepError::UnsupportedPatternKind`] until the full engine is
//! extracted.

#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::fmt;

/// How pattern text should be interpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatternKind {
    /// POSIX basic regular expression.
    Basic,
    /// POSIX extended regular expression.
    Extended,
    /// Fixed byte string.
    Fixed,
    /// Perl-compatible regular expression.
    Perl,
}

/// Case matching policy.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum CaseMode {
    /// Match bytes exactly.
    #[default]
    Sensitive,
    /// Match ASCII bytes case-insensitively.
    Insensitive,
}

/// Matcher options that are independent from the pattern storage.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct GrepOptions {
    /// Case policy for all compiled patterns.
    pub case: CaseMode,
    /// Require a match to cover the complete haystack line.
    pub line_regexp: bool,
    /// Require matches to start and end on ASCII word boundaries.
    pub word_regexp: bool,
}

/// Borrowed pattern passed into [`GrepMatcher::compile`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrepPattern<'pat> {
    /// Raw pattern bytes.
    pub text: &'pat [u8],
    /// Interpretation of `text`.
    pub kind: PatternKind,
}

impl<'pat> GrepPattern<'pat> {
    /// Construct a fixed-string pattern from borrowed bytes.
    #[must_use]
    pub const fn fixed(text: &'pat [u8]) -> Self {
        Self {
            text,
            kind: PatternKind::Fixed,
        }
    }

    /// Construct a pattern with an explicit kind.
    #[must_use]
    pub const fn new(text: &'pat [u8], kind: PatternKind) -> Self {
        Self { text, kind }
    }
}

/// Stable identifier for a compiled pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PatternId(pub usize);

/// Half-open byte span in a borrowed haystack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteSpan {
    /// Inclusive start byte offset.
    pub start: usize,
    /// Exclusive end byte offset.
    pub end: usize,
}

impl ByteSpan {
    /// Construct a byte span.
    #[must_use]
    pub const fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    /// Span length in bytes.
    #[must_use]
    pub const fn len(self) -> usize {
        self.end - self.start
    }

    /// Whether the span is empty.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.start == self.end
    }
}

/// One borrowed match event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MatchEvent<'haystack> {
    /// Pattern that matched.
    pub pattern: PatternId,
    /// Full haystack supplied to the search call.
    pub haystack: &'haystack [u8],
    /// Matched byte range inside `haystack`.
    pub span: ByteSpan,
}

impl<'haystack> MatchEvent<'haystack> {
    /// Borrow the bytes covered by this match.
    #[must_use]
    pub fn matched_bytes(self) -> &'haystack [u8] {
        &self.haystack[self.span.start..self.span.end]
    }
}

/// Control signal returned by [`MatchSink`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum MatchControl {
    /// Keep scanning.
    #[default]
    Continue,
    /// Stop scanning successfully.
    Stop,
}

/// Streaming consumer for borrowed match events.
pub trait MatchSink<'haystack> {
    /// Consume a match event.
    fn matched(&mut self, event: MatchEvent<'haystack>) -> MatchControl;
}

impl<'haystack, F> MatchSink<'haystack> for F
where
    F: FnMut(MatchEvent<'haystack>) -> MatchControl,
{
    fn matched(&mut self, event: MatchEvent<'haystack>) -> MatchControl {
        self(event)
    }
}

/// Compiled matcher state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrepMatcher {
    patterns: Vec<CompiledPattern>,
    options: GrepOptions,
}

impl GrepMatcher {
    /// Compile borrowed patterns into reusable matcher state.
    pub fn compile<'pat, I>(patterns: I, options: GrepOptions) -> Result<Self, GrepError>
    where
        I: IntoIterator<Item = GrepPattern<'pat>>,
    {
        let mut compiled = Vec::new();
        for (index, pattern) in patterns.into_iter().enumerate() {
            if pattern.kind != PatternKind::Fixed {
                return Err(GrepError::UnsupportedPatternKind(pattern.kind));
            }
            compiled.push(CompiledPattern {
                id: PatternId(index),
                needle: pattern.text.to_vec(),
            });
        }
        Ok(Self {
            patterns: compiled,
            options,
        })
    }

    /// Number of compiled patterns.
    #[must_use]
    pub fn pattern_count(&self) -> usize {
        self.patterns.len()
    }

    /// Borrow matcher options.
    #[must_use]
    pub const fn options(&self) -> &GrepOptions {
        &self.options
    }

    /// Return whether any pattern matches `haystack`.
    #[must_use]
    pub fn is_match(&self, haystack: &[u8]) -> bool {
        self.find_first(haystack).is_some()
    }

    /// Return the first match in pattern order, borrowing `haystack`.
    #[must_use]
    pub fn find_first<'haystack>(
        &self,
        haystack: &'haystack [u8],
    ) -> Option<MatchEvent<'haystack>> {
        for pattern in &self.patterns {
            if let Some(span) = self.find_pattern(pattern, haystack, 0) {
                return Some(MatchEvent {
                    pattern: pattern.id,
                    haystack,
                    span,
                });
            }
        }
        None
    }

    /// Visit non-overlapping matches in pattern order.
    ///
    /// The haystack is borrowed for the duration of each event; sinks that need
    /// ownership can copy just the matched slice they care about.
    pub fn visit_matches<'haystack, S>(&self, haystack: &'haystack [u8], sink: &mut S)
    where
        S: MatchSink<'haystack> + ?Sized,
    {
        for pattern in &self.patterns {
            let mut offset = 0usize;
            while offset <= haystack.len() {
                let Some(span) = self.find_pattern(pattern, haystack, offset) else {
                    break;
                };
                let event = MatchEvent {
                    pattern: pattern.id,
                    haystack,
                    span,
                };
                if sink.matched(event) == MatchControl::Stop {
                    return;
                }
                if span.end > span.start {
                    offset = span.end;
                } else {
                    offset = span.end.saturating_add(1);
                }
            }
        }
    }

    fn find_pattern(
        &self,
        pattern: &CompiledPattern,
        haystack: &[u8],
        from: usize,
    ) -> Option<ByteSpan> {
        if self.options.line_regexp {
            if from == 0 && bytes_eq(haystack, &pattern.needle, self.options.case) {
                return Some(ByteSpan::new(0, haystack.len()));
            }
            return None;
        }

        find_subslice(haystack, &pattern.needle, from, self.options.case).and_then(|span| {
            if self.options.word_regexp && !is_word_span(haystack, span) {
                let next = span.start.saturating_add(1);
                self.find_pattern(pattern, haystack, next)
            } else {
                Some(span)
            }
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CompiledPattern {
    id: PatternId,
    needle: Vec<u8>,
}

/// Grep compile error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrepError {
    /// Regex modes are part of the public surface but are not extracted yet.
    UnsupportedPatternKind(PatternKind),
}

impl fmt::Display for GrepError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedPatternKind(kind) => {
                write!(
                    f,
                    "pattern kind {kind:?} is not implemented in sley-grep yet"
                )
            }
        }
    }
}

impl std::error::Error for GrepError {}

fn find_subslice(haystack: &[u8], needle: &[u8], from: usize, case: CaseMode) -> Option<ByteSpan> {
    if from > haystack.len() {
        return None;
    }
    if needle.is_empty() {
        return Some(ByteSpan::new(from, from));
    }
    if needle.len() > haystack.len().saturating_sub(from) {
        return None;
    }

    let last_start = haystack.len() - needle.len();
    let mut start = from;
    while start <= last_start {
        let end = start + needle.len();
        if bytes_eq(&haystack[start..end], needle, case) {
            return Some(ByteSpan::new(start, end));
        }
        start += 1;
    }
    None
}

fn bytes_eq(left: &[u8], right: &[u8], case: CaseMode) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| byte_eq(*left, *right, case))
}

fn byte_eq(left: u8, right: u8, case: CaseMode) -> bool {
    match case {
        CaseMode::Sensitive => left == right,
        CaseMode::Insensitive => left.eq_ignore_ascii_case(&right),
    }
}

fn is_word_span(haystack: &[u8], span: ByteSpan) -> bool {
    let before = span
        .start
        .checked_sub(1)
        .and_then(|index| haystack.get(index).copied());
    let after = haystack.get(span.end).copied();
    !before.is_some_and(is_word_byte) && !after.is_some_and(is_word_byte)
}

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_matcher_streams_borrowed_events() -> Result<(), GrepError> {
        let matcher = GrepMatcher::compile(
            [GrepPattern::fixed(b"needle")],
            GrepOptions {
                case: CaseMode::Sensitive,
                line_regexp: false,
                word_regexp: false,
            },
        )?;
        let haystack = b"needle hay needle";
        let mut spans = Vec::new();

        matcher.visit_matches(haystack, &mut |event: MatchEvent<'_>| {
            assert_eq!(event.matched_bytes(), b"needle");
            spans.push(event.span);
            MatchControl::Continue
        });

        assert_eq!(spans, vec![ByteSpan::new(0, 6), ByteSpan::new(11, 17)]);
        Ok(())
    }

    #[test]
    fn regex_kinds_are_explicitly_reserved_for_extraction() {
        let err = GrepMatcher::compile(
            [GrepPattern::new(b"n.*e", PatternKind::Extended)],
            GrepOptions::default(),
        );

        assert_eq!(
            err,
            Err(GrepError::UnsupportedPatternKind(PatternKind::Extended))
        );
    }
}
