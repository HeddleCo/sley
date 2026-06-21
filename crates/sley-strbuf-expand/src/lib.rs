//! Shared `%` placeholder expansion mechanics.
//!
//! This crate mirrors git's `strbuf_expand` split: it owns scanning, literal
//! decoding, magic prefixes, and padding/truncation, while callers own atom
//! syntax and values through small dispatch traits.

use sley_core::{GitError, Result};
use std::io::Write;
use std::ops::Range;

const FORMATTING_LIMIT: i32 = 1 << 30;

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
    LeftAndSteal,
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
    Prefixed,
    ParenthesizedAndPrefixed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiteralHex {
    /// Do not decode percent hex escapes in the scanner.
    None,
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

    fn parse_prefix_atom(&self, _value: &str) -> Result<Option<(Self::Atom, usize)>> {
        Ok(None)
    }
}

pub trait AtomResolver<A> {
    fn resolve_atom(&mut self, out: &mut Vec<u8>, atom: &A) -> Result<()>;

    fn is_modifier_atom(&self, _atom: &A) -> bool {
        false
    }
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

    pub fn from_segments(segments: Vec<ExpandSegment<A>>) -> Self {
        Self { segments }
    }

    pub fn write_to<W, R, E>(&self, out: &mut W, resolver: &mut R, mut emit_atom: E) -> Result<()>
    where
        W: Write,
        R: AtomResolver<A>,
        E: FnMut(&mut Vec<u8>, &[u8]) -> Result<()>,
    {
        let mut rendered = Vec::new();
        self.append_to(&mut rendered, resolver, &mut emit_atom)?;
        out.write_all(&rendered)?;
        Ok(())
    }

    pub fn append_to<R, E>(
        &self,
        out: &mut Vec<u8>,
        resolver: &mut R,
        mut emit_atom: E,
    ) -> Result<()>
    where
        R: AtomResolver<A>,
        E: FnMut(&mut Vec<u8>, &[u8]) -> Result<()>,
    {
        self.append_range_to(out, 0..self.segments.len(), resolver, &mut emit_atom)
    }

    pub fn append_range_to<R, E>(
        &self,
        out: &mut Vec<u8>,
        range: Range<usize>,
        resolver: &mut R,
        mut emit_atom: E,
    ) -> Result<()>
    where
        R: AtomResolver<A>,
        E: FnMut(&mut Vec<u8>, &[u8]) -> Result<()>,
    {
        self.append_range_to_with_atom(out, range, resolver, |out, _atom, value| {
            emit_atom(out, value)
        })
    }

    pub fn append_range_to_with_atom<R, E>(
        &self,
        out: &mut Vec<u8>,
        range: Range<usize>,
        resolver: &mut R,
        mut emit_atom: E,
    ) -> Result<()>
    where
        R: AtomResolver<A>,
        E: FnMut(&mut Vec<u8>, &A, &[u8]) -> Result<()>,
    {
        let mut pending_padding = None;
        let mut padding_prefix = Vec::new();
        for segment in &self.segments[range] {
            match segment {
                ExpandSegment::Literal(literal) => out.extend_from_slice(literal),
                ExpandSegment::Padding(padding) => pending_padding = Some(*padding),
                ExpandSegment::Atom(atom) => {
                    let mut value = Vec::new();
                    resolver.resolve_atom(&mut value, &atom.atom)?;
                    if pending_padding.is_some() && resolver.is_modifier_atom(&atom.atom) {
                        apply_magic(&mut padding_prefix, atom.magic, &value);
                        padding_prefix.extend_from_slice(&value);
                        continue;
                    }
                    if !padding_prefix.is_empty() {
                        let mut combined = std::mem::take(&mut padding_prefix);
                        combined.extend_from_slice(&value);
                        value = combined;
                    }
                    apply_padding(out, &mut value, pending_padding.take());
                    apply_magic(out, atom.magic, &value);
                    emit_atom(out, &atom.atom, &value)?;
                }
            }
        }
        Ok(())
    }
}

fn current_column(out: &[u8]) -> usize {
    let start = out
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |position| position + 1);
    display_width(&out[start..])
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
        if magic != MagicPrefix::None
            && self.input.as_bytes().get(placeholder_start).copied() == Some(b'w')
        {
            self.push_literal(b"%".to_vec());
            self.cursor = after_percent;
            return Ok(true);
        }

        if magic == MagicPrefix::None
            && let Some((padding, consumed)) = parse_padding(&self.input[placeholder_start..])
        {
            self.push_segment(ExpandSegment::Padding(padding));
            self.cursor = placeholder_start + consumed;
            return Ok(true);
        }

        if matches!(
            self.options.atom_syntax,
            AtomSyntax::Parenthesized | AtomSyntax::ParenthesizedAndPrefixed
        ) && self.input.as_bytes().get(placeholder_start).copied() == Some(b'(')
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

        if matches!(
            self.options.atom_syntax,
            AtomSyntax::Prefixed | AtomSyntax::ParenthesizedAndPrefixed
        ) && let Some((atom, consumed)) = self
            .table
            .parse_prefix_atom(&self.input[placeholder_start..])?
        {
            self.push_segment(ExpandSegment::Atom(ExpandAtom { magic, atom }));
            self.cursor = placeholder_start + consumed;
            return Ok(true);
        }

        self.push_literal(b"%".to_vec());
        self.cursor = after_percent;
        Ok(true)
    }

    fn parse_hex_escape(&self, start: usize) -> Option<(u8, usize)> {
        match self.options.literal_hex {
            LiteralHex::None => None,
            LiteralHex::Bare => {
                parse_hex_pair(self.input.as_bytes().get(start..start + 2)?).map(|byte| (byte, 2))
            }
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
        b'>' if bytes.get(1).copied() == Some(b'>') => (PaddingAlign::LeftAndSteal, 2),
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
    let mut width = input[width_start..width_end].parse::<i32>().ok()?;
    if width == 0 || !(-FORMATTING_LIMIT..=FORMATTING_LIMIT).contains(&width) {
        return None;
    }
    if width < 0 {
        if to_column {
            width += term_columns();
        }
        if width < 0 {
            return None;
        }
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

fn apply_padding(out: &mut Vec<u8>, value: &mut Vec<u8>, padding: Option<PaddingSpec>) {
    let Some(padding) = padding else {
        return;
    };
    let mut target_width = if padding.to_column {
        (padding.width as usize).saturating_sub(current_column(out))
    } else {
        padding.width as usize
    };
    let mut align = padding.align;
    let width = display_width(value);
    if align == PaddingAlign::LeftAndSteal {
        while width > target_width && out.last().copied() == Some(b' ') {
            out.pop();
            target_width += 1;
        }
        align = PaddingAlign::Right;
    }
    if width > target_width {
        truncate_text(value, target_width, padding.truncate);
        return;
    }
    if width == target_width {
        return;
    }
    let extra = target_width - width;
    let (left, right) = match align {
        PaddingAlign::Left => (0, extra),
        PaddingAlign::Right => (extra, 0),
        PaddingAlign::Center => (extra / 2, extra - (extra / 2)),
        PaddingAlign::LeftAndSteal => (0, extra),
    };
    let mut padded = Vec::with_capacity(value.len() + extra);
    padded.extend(std::iter::repeat_n(b' ', left));
    padded.extend_from_slice(value);
    padded.extend(std::iter::repeat_n(b' ', right));
    *value = padded;
}

fn display_width(value: &[u8]) -> usize {
    let mut width = 0usize;
    let mut idx = 0usize;
    while idx < value.len() {
        if let Some(len) = ansi_escape_len(value, idx) {
            idx += len;
            continue;
        }
        match pick_utf8(value, idx) {
            Some((cp, len)) => {
                let w = wcwidth(cp);
                if w > 0 {
                    width += w as usize;
                }
                idx += len;
            }
            None => return value.len(),
        }
    }
    width
}

fn truncate_text(text: &mut Vec<u8>, target_width: usize, mode: TruncateMode) {
    if mode == TruncateMode::None {
        return;
    }
    if target_width == 0 {
        text.clear();
        return;
    }
    if target_width <= 2 {
        text.clear();
        text.extend(std::iter::repeat_n(b'.', target_width));
        return;
    }
    let width = display_width(text);
    if width <= target_width {
        return;
    }
    let keep = target_width - 2;
    let replace_width = width - keep;
    let pos = match mode {
        TruncateMode::None => return,
        TruncateMode::Left => 0,
        TruncateMode::Middle => target_width / 2 - 1,
        TruncateMode::Right => target_width - 2,
    };
    *text = utf8_replace(text, pos, replace_width, b"..");
}

fn utf8_replace(src: &[u8], pos: usize, width: usize, subst: &[u8]) -> Vec<u8> {
    let mut dst = Vec::with_capacity(src.len());
    let mut display_pos = 0usize;
    let mut idx = 0usize;
    let mut subst_done = false;
    while idx < src.len() {
        if let Some(len) = ansi_escape_len(src, idx) {
            dst.extend_from_slice(&src[idx..idx + len]);
            idx += len;
            continue;
        }
        let (cp, len) = match pick_utf8(src, idx) {
            Some(v) => v,
            None => return src.to_vec(),
        };
        let mut glyph_width = wcwidth(cp);
        if glyph_width < 0 {
            glyph_width = 0;
        }
        let glyph_width = glyph_width as usize;
        if glyph_width != 0 && display_pos >= pos && display_pos < pos + width {
            if !subst_done {
                dst.extend_from_slice(subst);
                subst_done = true;
            }
        } else {
            dst.extend_from_slice(&src[idx..idx + len]);
        }
        display_pos += glyph_width;
        idx += len;
    }
    dst
}

fn ansi_escape_len(bytes: &[u8], idx: usize) -> Option<usize> {
    if bytes.get(idx).copied() != Some(0x1b) {
        return None;
    }
    match bytes.get(idx + 1).copied()? {
        b'[' => {
            let mut cursor = idx + 2;
            while let Some(byte) = bytes.get(cursor).copied() {
                cursor += 1;
                if (0x40..=0x7e).contains(&byte) {
                    return Some(cursor - idx);
                }
            }
            None
        }
        b']' => {
            let mut cursor = idx + 2;
            while cursor < bytes.len() {
                if bytes[cursor] == 0x07 {
                    return Some(cursor + 1 - idx);
                }
                if bytes[cursor] == 0x1b && bytes.get(cursor + 1).copied() == Some(b'\\') {
                    return Some(cursor + 2 - idx);
                }
                cursor += 1;
            }
            None
        }
        _ => Some(2),
    }
}

fn wcwidth(ch: u32) -> i32 {
    if ch == 0 {
        return 0;
    }
    if ch < 32 || (0x7f..0xa0).contains(&ch) {
        return -1;
    }
    if bisearch(
        ch,
        &[
            (0x0300, 0x036f),
            (0x0483, 0x0489),
            (0x0591, 0x05bd),
            (0x05bf, 0x05bf),
            (0x05c1, 0x05c2),
            (0x05c4, 0x05c5),
            (0x05c7, 0x05c7),
            (0x0600, 0x0605),
            (0x0610, 0x061a),
            (0x061c, 0x061c),
            (0x064b, 0x065f),
            (0x0670, 0x0670),
            (0x06d6, 0x06dd),
            (0x06df, 0x06e4),
            (0x06e7, 0x06e8),
            (0x06ea, 0x06ed),
            (0x070f, 0x070f),
            (0x0711, 0x0711),
            (0x0730, 0x074a),
            (0x07a6, 0x07b0),
            (0x07eb, 0x07f3),
            (0x07fd, 0x07fd),
            (0x0816, 0x0819),
            (0x081b, 0x0823),
            (0x0825, 0x0827),
            (0x0829, 0x082d),
            (0x0859, 0x085b),
            (0x0890, 0x0891),
            (0x0898, 0x089f),
            (0x08ca, 0x08e1),
            (0x08e3, 0x0903),
            (0x093a, 0x093c),
            (0x0941, 0x0948),
            (0x094d, 0x094d),
            (0x0951, 0x0957),
            (0x0962, 0x0963),
            (0x0981, 0x0981),
            (0x09bc, 0x09bc),
            (0x09c1, 0x09c4),
            (0x09cd, 0x09cd),
            (0x09e2, 0x09e3),
            (0x09fe, 0x09fe),
            (0x0a01, 0x0a02),
            (0x0a3c, 0x0a3c),
            (0x0a41, 0x0a42),
            (0x0a47, 0x0a48),
            (0x0a4b, 0x0a4d),
            (0x0a51, 0x0a51),
            (0x0a70, 0x0a71),
            (0x0a75, 0x0a75),
            (0x0a81, 0x0a82),
            (0x0abc, 0x0abc),
            (0x0ac1, 0x0ac5),
            (0x0ac7, 0x0ac8),
            (0x0acd, 0x0acd),
            (0x0ae2, 0x0ae3),
            (0x0afa, 0x0aff),
            (0x0b01, 0x0b01),
            (0x0b3c, 0x0b3c),
            (0x0b3f, 0x0b3f),
            (0x0b41, 0x0b44),
            (0x0b4d, 0x0b4d),
            (0x0b55, 0x0b56),
            (0x0b62, 0x0b63),
            (0x0b82, 0x0b82),
            (0x0bc0, 0x0bc0),
            (0x0bcd, 0x0bcd),
            (0x0c00, 0x0c00),
            (0x0c04, 0x0c04),
            (0x0c3c, 0x0c3c),
            (0x0c3e, 0x0c40),
            (0x0c46, 0x0c48),
            (0x0c4a, 0x0c4d),
            (0x0c55, 0x0c56),
            (0x0c62, 0x0c63),
            (0x0c81, 0x0c81),
            (0x0cbc, 0x0cbc),
            (0x0cbf, 0x0cbf),
            (0x0cc6, 0x0cc6),
            (0x0ccc, 0x0ccd),
            (0x0ce2, 0x0ce3),
            (0x0d00, 0x0d01),
            (0x0d3b, 0x0d3c),
            (0x0d41, 0x0d44),
            (0x0d4d, 0x0d4d),
            (0x0d62, 0x0d63),
            (0x0d81, 0x0d81),
            (0x0dca, 0x0dca),
            (0x0dd2, 0x0dd4),
            (0x0dd6, 0x0dd6),
            (0x0e31, 0x0e31),
            (0x0e34, 0x0e3a),
            (0x0e47, 0x0e4e),
            (0x0eb1, 0x0eb1),
            (0x0eb4, 0x0ebc),
            (0x0ec8, 0x0ece),
            (0x0f18, 0x0f19),
            (0x0f35, 0x0f35),
            (0x0f37, 0x0f37),
            (0x0f39, 0x0f39),
            (0x0f71, 0x0f7e),
            (0x0f80, 0x0f84),
            (0x0f86, 0x0f87),
            (0x0f8d, 0x0f97),
            (0x0f99, 0x0fbc),
            (0x0fc6, 0x0fc6),
            (0x102d, 0x1030),
            (0x1032, 0x1037),
            (0x1039, 0x103a),
            (0x103d, 0x103e),
            (0x1058, 0x1059),
            (0x105e, 0x1060),
            (0x1071, 0x1074),
            (0x1082, 0x1082),
            (0x1085, 0x1086),
            (0x108d, 0x108d),
            (0x109d, 0x109d),
            (0x135d, 0x135f),
            (0x1712, 0x1714),
            (0x1732, 0x1734),
            (0x1752, 0x1753),
            (0x1772, 0x1773),
            (0x17b4, 0x17b5),
            (0x17b7, 0x17bd),
            (0x17c6, 0x17c6),
            (0x17c9, 0x17d3),
            (0x17dd, 0x17dd),
            (0x180b, 0x180f),
            (0x1885, 0x1886),
            (0x18a9, 0x18a9),
            (0x1920, 0x1922),
            (0x1927, 0x1928),
            (0x1932, 0x1932),
            (0x1939, 0x193b),
            (0x1a17, 0x1a18),
            (0x1a1b, 0x1a1b),
            (0x1a56, 0x1a56),
            (0x1a58, 0x1a5e),
            (0x1a60, 0x1a60),
            (0x1a62, 0x1a62),
            (0x1a65, 0x1a6c),
            (0x1a73, 0x1a7c),
            (0x1a7f, 0x1a7f),
            (0x1ab0, 0x1ace),
            (0x1b00, 0x1b03),
            (0x1b34, 0x1b34),
            (0x1b36, 0x1b3a),
            (0x1b3c, 0x1b3c),
            (0x1b42, 0x1b42),
            (0x1b6b, 0x1b73),
            (0x1b80, 0x1b81),
            (0x1ba2, 0x1ba5),
            (0x1ba8, 0x1ba9),
            (0x1bab, 0x1bad),
            (0x1be6, 0x1be6),
            (0x1be8, 0x1be9),
            (0x1bed, 0x1bed),
            (0x1bef, 0x1bf1),
            (0x1c2c, 0x1c33),
            (0x1c36, 0x1c37),
            (0x1cd0, 0x1cd2),
            (0x1cd4, 0x1ce0),
            (0x1ce2, 0x1ce8),
            (0x1ced, 0x1ced),
            (0x1cf4, 0x1cf4),
            (0x1cf8, 0x1cf9),
            (0x1dc0, 0x1dff),
            (0x200b, 0x200f),
            (0x202a, 0x202e),
            (0x2060, 0x2064),
            (0x2066, 0x206f),
            (0x20d0, 0x20f0),
            (0x2cef, 0x2cf1),
            (0x2d7f, 0x2d7f),
            (0x2de0, 0x2dff),
            (0x302a, 0x302f),
            (0x3099, 0x309a),
            (0xa66f, 0xa672),
            (0xa674, 0xa67d),
            (0xa69e, 0xa69f),
            (0xa6f0, 0xa6f1),
            (0xa802, 0xa802),
            (0xa806, 0xa806),
            (0xa80b, 0xa80b),
            (0xa825, 0xa826),
            (0xa82c, 0xa82c),
            (0xa8c4, 0xa8c5),
            (0xa8e0, 0xa8f1),
            (0xa8ff, 0xa8ff),
            (0xa926, 0xa92d),
            (0xa947, 0xa951),
            (0xa980, 0xa982),
            (0xa9b3, 0xa9b3),
            (0xa9b6, 0xa9b9),
            (0xa9bc, 0xa9bd),
            (0xa9e5, 0xa9e5),
            (0xaa29, 0xaa2e),
            (0xaa31, 0xaa32),
            (0xaa35, 0xaa36),
            (0xaa43, 0xaa43),
            (0xaa4c, 0xaa4c),
            (0xaa7c, 0xaa7c),
            (0xaab0, 0xaab0),
            (0xaab2, 0xaab4),
            (0xaab7, 0xaab8),
            (0xaabe, 0xaabf),
            (0xaac1, 0xaac1),
            (0xaaec, 0xaaed),
            (0xaaf6, 0xaaf6),
            (0xabe5, 0xabe5),
            (0xabe8, 0xabe8),
            (0xabed, 0xabed),
            (0xfb1e, 0xfb1e),
            (0xfe00, 0xfe0f),
            (0xfe20, 0xfe2f),
            (0xfeff, 0xfeff),
            (0xfff9, 0xfffb),
            (0x101fd, 0x101fd),
            (0x102e0, 0x102e0),
            (0x10376, 0x1037a),
            (0x10a01, 0x10a03),
            (0x10a05, 0x10a06),
            (0x10a0c, 0x10a0f),
            (0x10a38, 0x10a3a),
            (0x10a3f, 0x10a3f),
            (0x10ae5, 0x10ae6),
            (0x10d24, 0x10d27),
            (0x10eab, 0x10eac),
            (0x10f46, 0x10f50),
            (0x10f82, 0x10f85),
            (0x11001, 0x11001),
            (0x11038, 0x11046),
            (0x11070, 0x11070),
            (0x11073, 0x11074),
            (0x1107f, 0x11081),
            (0x110b3, 0x110b6),
            (0x110b9, 0x110ba),
            (0x110bd, 0x110bd),
            (0x110c2, 0x110c2),
            (0x11100, 0x11102),
            (0x11127, 0x1112b),
            (0x1112d, 0x11134),
            (0x11173, 0x11173),
            (0x11180, 0x11181),
            (0x111b6, 0x111be),
            (0x111c9, 0x111cc),
            (0x111cf, 0x111cf),
            (0x1122f, 0x11231),
            (0x11234, 0x11234),
            (0x11236, 0x11237),
            (0x1123e, 0x1123e),
            (0x112df, 0x112df),
            (0x112e3, 0x112ea),
            (0x11300, 0x11301),
            (0x1133b, 0x1133c),
            (0x11340, 0x11340),
            (0x11366, 0x1136c),
            (0x11370, 0x11374),
            (0x11438, 0x1143f),
            (0x11442, 0x11444),
            (0x11446, 0x11446),
            (0x1145e, 0x1145e),
            (0x114b3, 0x114b8),
            (0x114ba, 0x114ba),
            (0x114bf, 0x114c0),
            (0x114c2, 0x114c3),
            (0x115b2, 0x115b5),
            (0x115bc, 0x115bd),
            (0x115bf, 0x115c0),
            (0x115dc, 0x115dd),
            (0x11633, 0x1163a),
            (0x1163d, 0x1163d),
            (0x1163f, 0x11640),
            (0x116ab, 0x116ab),
            (0x116ad, 0x116ad),
            (0x116b0, 0x116b5),
            (0x116b7, 0x116b7),
            (0x1171d, 0x1171f),
            (0x11722, 0x11725),
            (0x11727, 0x1172b),
            (0x1182f, 0x11837),
            (0x11839, 0x1183a),
            (0x1193b, 0x1193c),
            (0x1193e, 0x1193e),
            (0x11943, 0x11943),
            (0x119d4, 0x119d7),
            (0x119da, 0x119db),
            (0x119e0, 0x119e0),
            (0x11a01, 0x11a0a),
            (0x11a33, 0x11a38),
            (0x11a3b, 0x11a3e),
            (0x11a47, 0x11a47),
            (0x11a51, 0x11a56),
            (0x11a59, 0x11a5b),
            (0x11a8a, 0x11a96),
            (0x11a98, 0x11a99),
            (0x11c30, 0x11c36),
            (0x11c38, 0x11c3d),
            (0x11c3f, 0x11c3f),
            (0x11c92, 0x11ca7),
            (0x11caa, 0x11cb0),
            (0x11cb2, 0x11cb3),
            (0x11cb5, 0x11cb6),
            (0x11d31, 0x11d36),
            (0x11d3a, 0x11d3a),
            (0x11d3c, 0x11d3d),
            (0x11d3f, 0x11d45),
            (0x11d47, 0x11d47),
            (0x11d90, 0x11d91),
            (0x11d95, 0x11d95),
            (0x11d97, 0x11d97),
            (0x11ef3, 0x11ef4),
            (0x13430, 0x13438),
            (0x16af0, 0x16af4),
            (0x16b30, 0x16b36),
            (0x16f4f, 0x16f4f),
            (0x16f8f, 0x16f92),
            (0x16fe4, 0x16fe4),
            (0x1bc9d, 0x1bc9e),
            (0x1bca0, 0x1bca3),
            (0x1cf00, 0x1cf2d),
            (0x1cf30, 0x1cf46),
            (0x1d167, 0x1d169),
            (0x1d173, 0x1d182),
            (0x1d185, 0x1d18b),
            (0x1d1aa, 0x1d1ad),
            (0x1d242, 0x1d244),
            (0x1da00, 0x1da36),
            (0x1da3b, 0x1da6c),
            (0x1da75, 0x1da75),
            (0x1da84, 0x1da84),
            (0x1da9b, 0x1da9f),
            (0x1daa1, 0x1daaf),
            (0x1e000, 0x1e006),
            (0x1e008, 0x1e018),
            (0x1e01b, 0x1e021),
            (0x1e023, 0x1e024),
            (0x1e026, 0x1e02a),
            (0x1e08f, 0x1e08f),
            (0x1e130, 0x1e136),
            (0x1e2ae, 0x1e2ae),
            (0x1e2ec, 0x1e2ef),
            (0x1e4ec, 0x1e4ef),
            (0x1e8d0, 0x1e8d6),
            (0x1e944, 0x1e94a),
            (0xe0100, 0xe01ef),
        ],
    ) {
        return 0;
    }
    if bisearch(
        ch,
        &[
            (0x1100, 0x115f),
            (0x231a, 0x231b),
            (0x2329, 0x232a),
            (0x23e9, 0x23ec),
            (0x23f0, 0x23f0),
            (0x23f3, 0x23f3),
            (0x25fd, 0x25fe),
            (0x2614, 0x2615),
            (0x2648, 0x2653),
            (0x267f, 0x267f),
            (0x2693, 0x2693),
            (0x26a1, 0x26a1),
            (0x26aa, 0x26ab),
            (0x26bd, 0x26be),
            (0x26c4, 0x26c5),
            (0x26ce, 0x26ce),
            (0x26d4, 0x26d4),
            (0x26ea, 0x26ea),
            (0x26f2, 0x26f3),
            (0x26f5, 0x26f5),
            (0x26fa, 0x26fa),
            (0x26fd, 0x26fd),
            (0x2705, 0x2705),
            (0x270a, 0x270b),
            (0x2728, 0x2728),
            (0x274c, 0x274c),
            (0x274e, 0x274e),
            (0x2753, 0x2755),
            (0x2757, 0x2757),
            (0x2795, 0x2797),
            (0x27b0, 0x27b0),
            (0x27bf, 0x27bf),
            (0x2b1b, 0x2b1c),
            (0x2b50, 0x2b50),
            (0x2b55, 0x2b55),
            (0x2e80, 0x303e),
            (0x3040, 0xa4cf),
            (0xac00, 0xd7a3),
            (0xf900, 0xfaff),
            (0xfe10, 0xfe19),
            (0xfe30, 0xfe6f),
            (0xff00, 0xff60),
            (0xffe0, 0xffe6),
            (0x16fe0, 0x16fe3),
            (0x17000, 0x187f7),
            (0x18800, 0x18cd5),
            (0x18d00, 0x18d08),
            (0x1aff0, 0x1aff3),
            (0x1aff5, 0x1affb),
            (0x1affd, 0x1affe),
            (0x1b000, 0x1b122),
            (0x1b132, 0x1b132),
            (0x1b150, 0x1b152),
            (0x1b155, 0x1b155),
            (0x1b164, 0x1b167),
            (0x1f004, 0x1f004),
            (0x1f0cf, 0x1f0cf),
            (0x1f18e, 0x1f18e),
            (0x1f191, 0x1f19a),
            (0x1f200, 0x1f202),
            (0x1f210, 0x1f23b),
            (0x1f240, 0x1f248),
            (0x1f250, 0x1f251),
            (0x1f260, 0x1f265),
            (0x1f300, 0x1f320),
            (0x1f32d, 0x1f335),
            (0x1f337, 0x1f37c),
            (0x1f37e, 0x1f393),
            (0x1f3a0, 0x1f3ca),
            (0x1f3cf, 0x1f3d3),
            (0x1f3e0, 0x1f3f0),
            (0x1f3f4, 0x1f3f4),
            (0x1f3f8, 0x1f43e),
            (0x1f440, 0x1f440),
            (0x1f442, 0x1f4fc),
            (0x1f4ff, 0x1f53d),
            (0x1f54b, 0x1f54e),
            (0x1f550, 0x1f567),
            (0x1f57a, 0x1f57a),
            (0x1f595, 0x1f596),
            (0x1f5a4, 0x1f5a4),
            (0x1f5fb, 0x1f64f),
            (0x1f680, 0x1f6c5),
            (0x1f6cc, 0x1f6cc),
            (0x1f6d0, 0x1f6d2),
            (0x1f6d5, 0x1f6d7),
            (0x1f6dc, 0x1f6df),
            (0x1f6eb, 0x1f6ec),
            (0x1f6f4, 0x1f6fc),
            (0x1f7e0, 0x1f7eb),
            (0x1f7f0, 0x1f7f0),
            (0x1f90c, 0x1f93a),
            (0x1f93c, 0x1f945),
            (0x1f947, 0x1f9ff),
            (0x1fa70, 0x1fa7c),
            (0x1fa80, 0x1fa88),
            (0x1fa90, 0x1fabd),
            (0x1fabf, 0x1fac5),
            (0x1face, 0x1fadb),
            (0x1fae0, 0x1fae8),
            (0x1faf0, 0x1faf8),
            (0x20000, 0x2fffd),
            (0x30000, 0x3fffd),
        ],
    ) {
        return 2;
    }
    1
}

fn bisearch(needle: u32, ranges: &[(u32, u32)]) -> bool {
    let mut low = 0usize;
    let mut high = ranges.len();
    while low < high {
        let mid = low + (high - low) / 2;
        let (start, end) = ranges[mid];
        if needle < start {
            high = mid;
        } else if needle > end {
            low = mid + 1;
        } else {
            return true;
        }
    }
    false
}

fn pick_utf8(bytes: &[u8], idx: usize) -> Option<(u32, usize)> {
    let s = &bytes[idx..];
    let b0 = *s.first()?;
    if b0 < 0x80 {
        Some((b0 as u32, 1))
    } else if b0 & 0xe0 == 0xc0 {
        let b1 = *s.get(1)?;
        if b1 & 0xc0 != 0x80 || b0 & 0xfe == 0xc0 {
            return None;
        }
        Some(((((b0 & 0x1f) as u32) << 6) | (b1 & 0x3f) as u32, 2))
    } else if b0 & 0xf0 == 0xe0 {
        let b1 = *s.get(1)?;
        let b2 = *s.get(2)?;
        if b1 & 0xc0 != 0x80
            || b2 & 0xc0 != 0x80
            || (b0 == 0xe0 && b1 & 0xe0 == 0x80)
            || (b0 == 0xed && b1 & 0xe0 == 0xa0)
        {
            return None;
        }
        Some((
            (((b0 & 0x0f) as u32) << 12) | (((b1 & 0x3f) as u32) << 6) | (b2 & 0x3f) as u32,
            3,
        ))
    } else if b0 & 0xf8 == 0xf0 {
        let b1 = *s.get(1)?;
        let b2 = *s.get(2)?;
        let b3 = *s.get(3)?;
        if b1 & 0xc0 != 0x80
            || b2 & 0xc0 != 0x80
            || b3 & 0xc0 != 0x80
            || (b0 == 0xf0 && b1 & 0xf0 == 0x80)
            || (b0 == 0xf4 && b1 > 0x8f)
            || b0 > 0xf4
        {
            return None;
        }
        Some((
            (((b0 & 0x07) as u32) << 18)
                | (((b1 & 0x3f) as u32) << 12)
                | (((b2 & 0x3f) as u32) << 6)
                | (b3 & 0x3f) as u32,
            4,
        ))
    } else {
        None
    }
}

pub fn term_columns() -> i32 {
    if let Ok(cols) = std::env::var("COLUMNS")
        && let Ok(n) = cols.trim().parse::<i32>()
        && n > 0
    {
        return n;
    }
    80
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
