//! Shared `%` placeholder expansion mechanics.
//!
//! This crate mirrors git's `strbuf_expand` split: it owns scanning, literal
//! decoding, magic prefixes, and padding/truncation, while callers own atom
//! syntax and values through small dispatch traits.

use sley_core::{GitError, Result};
use std::io::Write;

const FORMATTING_LIMIT: i32 = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpandFormat<A> {
    segments: Vec<ExpandSegment<A>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExpandSegment<A> {
    Literal(Vec<u8>),
    Padding(PaddingSpec),
    Atom(ExpandAtom<A>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpandAtom<A> {
    pub magic: MagicPrefix,
    pub atom: A,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum MagicPrefix {
    #[default]
    None,
    AddLfBeforeNonEmpty,
    DeleteLfBeforeEmpty,
    AddSpaceBeforeNonEmpty,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaddingSpec {
    pub align: PaddingAlign,
    pub width: i32,
    pub truncate: TruncateMode,
    pub to_column: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaddingAlign {
    Left,
    Right,
    Center,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum TruncateMode {
    #[default]
    None,
    Left,
    Middle,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExpandOptions {
    pub atom_syntax: AtomSyntax,
    pub literal_hex: LiteralHex,
}

impl Default for ExpandOptions {
    fn default() -> Self {
        Self {
            atom_syntax: AtomSyntax::Parenthesized,
            literal_hex: LiteralHex::Bare,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AtomSyntax {
    Parenthesized,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiteralHex {
    /// Decode `%NN` escapes, matching ref-filter literals.
    Bare,
    /// Decode `%xNN` escapes, matching git's generic `strbuf_expand_literal`.
    XPrefixed,
    /// Decode both `%NN` and `%xNN`.
    Both,
}

pub trait AtomTable {
    type Atom;

    fn parse_atom(&self, value: &str) -> Result<Self::Atom>;
}

pub trait AtomResolver<A> {
    fn resolve_atom(&mut self, out: &mut Vec<u8>, atom: &A) -> Result<()>;
}

impl<A, F> AtomResolver<A> for F
where
    F: FnMut(&mut Vec<u8>, &A) -> Result<()>,
{
    fn resolve_atom(&mut self, out: &mut Vec<u8>, atom: &A) -> Result<()> {
        self(out, atom)
    }
}

impl<A> ExpandFormat<A> {
    pub fn parse(format_spec: &str, table: &impl AtomTable<Atom = A>) -> Result<Self> {
        Self::parse_with_options(format_spec, table, ExpandOptions::default())
    }

    pub fn parse_with_options(
        format_spec: &str,
        table: &impl AtomTable<Atom = A>,
        options: ExpandOptions,
    ) -> Result<Self> {
        let mut parser = Parser {
            input: format_spec,
            table,
            options,
            segments: Vec::new(),
            cursor: 0,
        };
        parser.parse()?;
        Ok(Self {
            segments: parser.segments,
        })
    }

    pub fn segments(&self) -> &[ExpandSegment<A>] {
        &self.segments
    }

    pub fn write_to<W, R, E>(&self, out: &mut W, resolver: &mut R, mut emit_atom: E) -> Result<()>
    where
        W: Write,
        R: AtomResolver<A>,
        E: FnMut(&mut Vec<u8>, &[u8]) -> Result<()>,
    {
        let mut rendered = Vec::new();
        let mut pending_padding = None;
        for segment in &self.segments {
            match segment {
                ExpandSegment::Literal(literal) => rendered.extend_from_slice(literal),
                ExpandSegment::Padding(padding) => pending_padding = Some(*padding),
                ExpandSegment::Atom(atom) => {
                    let mut value = Vec::new();
                    resolver.resolve_atom(&mut value, &atom.atom)?;
                    apply_padding(&mut value, pending_padding.take(), current_column(&rendered));
                    apply_magic(&mut rendered, atom.magic, &value);
                    emit_atom(&mut rendered, &value)?;
                }
            }
        }
        out.write_all(&rendered)?;
        Ok(())
    }
}

fn current_column(out: &[u8]) -> usize {
    let start = out
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |position| position + 1);
    display_width(&String::from_utf8_lossy(&out[start..]))
}

struct Parser<'a, A, T>
where
    T: AtomTable<Atom = A>,
{
    input: &'a str,
    table: &'a T,
    options: ExpandOptions,
    segments: Vec<ExpandSegment<A>>,
    cursor: usize,
}

impl<A, T> Parser<'_, A, T>
where
    T: AtomTable<Atom = A>,
{
    fn parse(&mut self) -> Result<()> {
        while let Some(relative) = self.input[self.cursor..].find('%') {
            let percent = self.cursor + relative;
            self.push_literal(self.input.as_bytes()[self.cursor..percent].to_vec());
            if self.parse_percent(percent)? {
                continue;
            }
        }
        self.push_literal(self.input.as_bytes()[self.cursor..].to_vec());
        Ok(())
    }

    fn parse_percent(&mut self, percent: usize) -> Result<bool> {
        let after_percent = percent + 1;
        let Some(first) = self.input.as_bytes().get(after_percent).copied() else {
            self.push_literal(b"%".to_vec());
            self.cursor = after_percent;
            return Ok(true);
        };
        if first == b'%' {
            self.push_literal(b"%".to_vec());
            self.cursor = after_percent + 1;
            return Ok(true);
        }
        if let Some((byte, consumed)) = self.parse_hex_escape(after_percent) {
            self.push_literal(vec![byte]);
            self.cursor = after_percent + consumed;
            return Ok(true);
        }

        let (magic, placeholder_start) = match first {
            b'+' => (MagicPrefix::AddLfBeforeNonEmpty, after_percent + 1),
            b'-' => (MagicPrefix::DeleteLfBeforeEmpty, after_percent + 1),
            b' ' => (MagicPrefix::AddSpaceBeforeNonEmpty, after_percent + 1),
            _ => (MagicPrefix::None, after_percent),
        };

        if magic == MagicPrefix::None
            && let Some((padding, consumed)) = parse_padding(&self.input[placeholder_start..])
        {
            self.push_segment(ExpandSegment::Padding(padding));
            self.cursor = placeholder_start + consumed;
            return Ok(true);
        }

        if self.options.atom_syntax == AtomSyntax::Parenthesized
            && self.input.as_bytes().get(placeholder_start).copied() == Some(b'(')
        {
            let value_start = placeholder_start + 1;
            let Some(relative_end) = self.input[value_start..].find(')') else {
                return Err(GitError::Command(
                    "unterminated format placeholder".to_string(),
                ));
            };
            let value_end = value_start + relative_end;
            let atom = self.table.parse_atom(&self.input[value_start..value_end])?;
            self.push_segment(ExpandSegment::Atom(ExpandAtom { magic, atom }));
            self.cursor = value_end + 1;
            return Ok(true);
        }

        self.push_literal(b"%".to_vec());
        self.cursor = after_percent;
        Ok(true)
    }

    fn parse_hex_escape(&self, start: usize) -> Option<(u8, usize)> {
        match self.options.literal_hex {
            LiteralHex::Bare => parse_hex_pair(self.input.as_bytes().get(start..start + 2)?)
                .map(|byte| (byte, 2)),
            LiteralHex::XPrefixed => parse_x_hex(self.input.as_bytes(), start),
            LiteralHex::Both => parse_x_hex(self.input.as_bytes(), start).or_else(|| {
                parse_hex_pair(self.input.as_bytes().get(start..start + 2)?).map(|byte| (byte, 2))
            }),
        }
    }

    fn push_literal(&mut self, literal: Vec<u8>) {
        if literal.is_empty() {
            return;
        }
        if let Some(ExpandSegment::Literal(previous)) = self.segments.last_mut() {
            previous.extend_from_slice(&literal);
        } else {
            self.segments.push(ExpandSegment::Literal(literal));
        }
    }

    fn push_segment(&mut self, segment: ExpandSegment<A>) {
        self.segments.push(segment);
    }
}

fn parse_x_hex(bytes: &[u8], start: usize) -> Option<(u8, usize)> {
    if bytes.get(start).copied() != Some(b'x') {
        return None;
    }
    parse_hex_pair(bytes.get(start + 1..start + 3)?).map(|byte| (byte, 3))
}

fn parse_hex_pair(value: &[u8]) -> Option<u8> {
    let [high, low] = value else {
        return None;
    };
    Some(hex_digit(*high)? << 4 | hex_digit(*low)?)
}

fn hex_digit(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn parse_padding(input: &str) -> Option<(PaddingSpec, usize)> {
    let bytes = input.as_bytes();
    let (align, mut cursor) = match bytes.first().copied()? {
        b'<' => (PaddingAlign::Left, 1),
        b'>' if bytes.get(1).copied() == Some(b'<') => (PaddingAlign::Center, 2),
        b'>' => (PaddingAlign::Right, 1),
        _ => return None,
    };
    let to_column = bytes.get(cursor).copied() == Some(b'|');
    if to_column {
        cursor += 1;
    }
    if bytes.get(cursor).copied() != Some(b'(') {
        return None;
    }
    let width_start = cursor + 1;
    let mut width_end = width_start;
    while matches!(bytes.get(width_end), Some(b'-' | b'0'..=b'9')) {
        width_end += 1;
    }
    if width_end == width_start {
        return None;
    }
    let width = input[width_start..width_end].parse::<i32>().ok()?;
    if width == 0 || !(-FORMATTING_LIMIT..=FORMATTING_LIMIT).contains(&width) {
        return None;
    }
    // TODO(strbuf_expand): negative `%<|(-N)` needs terminal width. Keep it
    // rejected for now instead of guessing.
    if width < 0 {
        return None;
    }

    let (truncate, end) = match bytes.get(width_end).copied()? {
        b')' => (TruncateMode::None, width_end),
        b',' => {
            let mode_start = width_end + 1;
            let relative_end = input[mode_start..].find(')')?;
            let mode_end = mode_start + relative_end;
            let truncate = match &input[mode_start..mode_end] {
                "ltrunc" => TruncateMode::Left,
                "mtrunc" => TruncateMode::Middle,
                "rtrunc" | "trunc" => TruncateMode::Right,
                _ => return None,
            };
            (truncate, mode_end)
        }
        _ => return None,
    };
    Some((
        PaddingSpec {
            align,
            width,
            truncate,
            to_column,
        },
        end + 1,
    ))
}

fn apply_magic(out: &mut Vec<u8>, magic: MagicPrefix, value: &[u8]) {
    match (magic, value.is_empty()) {
        (MagicPrefix::None, _) | (MagicPrefix::DeleteLfBeforeEmpty, _) => {}
        (MagicPrefix::AddLfBeforeNonEmpty, false) => out.extend_from_slice(b"\n"),
        (MagicPrefix::AddSpaceBeforeNonEmpty, false) => out.extend_from_slice(b" "),
        (MagicPrefix::AddLfBeforeNonEmpty | MagicPrefix::AddSpaceBeforeNonEmpty, true) => {}
    }
    if magic == MagicPrefix::DeleteLfBeforeEmpty && value.is_empty() {
        while out.last().copied() == Some(b'\n') {
            out.pop();
        }
    }
}

fn apply_padding(value: &mut Vec<u8>, padding: Option<PaddingSpec>, current_column: usize) {
    let Some(padding) = padding else {
        return;
    };
    let target_width = if padding.to_column {
        (padding.width as usize).saturating_sub(current_column)
    } else {
        padding.width as usize
    };
    let mut text = String::from_utf8_lossy(value).into_owned();
    let width = display_width(&text);
    if width > target_width {
        truncate_text(&mut text, target_width, padding.truncate);
        value.clear();
        value.extend_from_slice(text.as_bytes());
        return;
    }
    if width == target_width {
        return;
    }
    let extra = target_width - width;
    let (left, right) = match padding.align {
        PaddingAlign::Left => (0, extra),
        PaddingAlign::Right => (extra, 0),
        PaddingAlign::Center => (extra / 2, extra - (extra / 2)),
    };
    let mut padded = String::with_capacity(text.len() + extra);
    padded.extend(std::iter::repeat_n(' ', left));
    padded.push_str(&text);
    padded.extend(std::iter::repeat_n(' ', right));
    value.clear();
    value.extend_from_slice(padded.as_bytes());
}

fn display_width(value: &str) -> usize {
    // TODO(strbuf_expand): use git's utf8_strnwidth/display_mode_esc_sequence_len
    // equivalent for wide codepoints and color escapes. Ref names and object ids
    // in the pilot are ASCII, where char count is byte-compatible.
    value.chars().count()
}

fn truncate_text(text: &mut String, target_width: usize, mode: TruncateMode) {
    if mode == TruncateMode::None {
        return;
    }
    if target_width == 0 {
        text.clear();
        return;
    }
    if target_width <= 2 {
        text.clear();
        text.extend(std::iter::repeat_n('.', target_width));
        return;
    }
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= target_width {
        return;
    }
    let keep = target_width - 2;
    let truncated = match mode {
        TruncateMode::None => return,
        TruncateMode::Left => {
            let tail: String = chars[chars.len() - keep..].iter().collect();
            format!("..{tail}")
        }
        TruncateMode::Middle => {
            let left = keep / 2;
            let right = keep - left;
            let head: String = chars[..left].iter().collect();
            let tail: String = chars[chars.len() - right..].iter().collect();
            format!("{head}..{tail}")
        }
        TruncateMode::Right => {
            let head: String = chars[..keep].iter().collect();
            format!("{head}..")
        }
    };
    *text = truncated;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Raw(String);

    struct RawTable;

    impl AtomTable for RawTable {
        type Atom = Raw;

        fn parse_atom(&self, value: &str) -> Result<Self::Atom> {
            Ok(Raw(value.to_string()))
        }
    }

    #[test]
    fn parser_splits_literals_atoms_and_escapes() {
        let format = ExpandFormat::parse("a%%b%09%(ref)%q", &RawTable).expect("valid format");
        assert_eq!(
            format.segments(),
            &[
                ExpandSegment::Literal(b"a%b\t".to_vec()),
                ExpandSegment::Atom(ExpandAtom {
                    magic: MagicPrefix::None,
                    atom: Raw("ref".to_string()),
                }),
                ExpandSegment::Literal(b"%q".to_vec()),
            ]
        );
    }

    #[test]
    fn parser_decodes_x_prefixed_hex_when_enabled() {
        let format = ExpandFormat::parse_with_options(
            "%x0a%(ref)",
            &RawTable,
            ExpandOptions {
                atom_syntax: AtomSyntax::Parenthesized,
                literal_hex: LiteralHex::Both,
            },
        )
        .expect("valid format");
        assert_eq!(
            format.segments(),
            &[
                ExpandSegment::Literal(b"\n".to_vec()),
                ExpandSegment::Atom(ExpandAtom {
                    magic: MagicPrefix::None,
                    atom: Raw("ref".to_string()),
                }),
            ]
        );
    }

    #[test]
    fn parser_records_magic_and_padding() {
        let format = ExpandFormat::parse("%<(8,mtrunc)%+(ref)", &RawTable).expect("valid format");
        assert_eq!(
            format.segments(),
            &[
                ExpandSegment::Padding(PaddingSpec {
                    align: PaddingAlign::Left,
                    width: 8,
                    truncate: TruncateMode::Middle,
                    to_column: false,
                }),
                ExpandSegment::Atom(ExpandAtom {
                    magic: MagicPrefix::AddLfBeforeNonEmpty,
                    atom: Raw("ref".to_string()),
                }),
            ]
        );
    }

    #[test]
    fn renderer_applies_padding_and_truncation() {
        let format = ExpandFormat::parse("%>(6)%(a)|%><(6)%(b)|%<(5,rtrunc)%(c)", &RawTable)
            .expect("valid format");
        let mut out = Vec::new();
        format
            .write_to(
                &mut out,
                &mut |value: &mut Vec<u8>, atom: &Raw| {
                    value.extend_from_slice(match atom.0.as_str() {
                        "a" => b"x",
                        "b" => b"xy",
                        "c" => b"abcdef",
                        _ => b"",
                    });
                    Ok(())
                },
                |out, value| {
                    out.write_all(value)?;
                    Ok(())
                },
            )
            .expect("write succeeds");
        assert_eq!(out, b"     x|  xy  |abc..");
    }
}
