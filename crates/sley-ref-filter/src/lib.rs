//! Shared ref-filter formatting primitives.
//!
//! Git reuses the same identity/date/refname formatting language across
//! `for-each-ref`, `branch`, `tag`, `log`, `show`, `stash`, and status output.
//! This crate owns those semantic primitives so the CLI can remain an entry
//! point instead of a home for every command's formatting state.

use sley_core::{DateMode, GitError, ObjectId, Result};
use sley_strbuf_expand::{
    AtomTable, ExpandFormat, ExpandSegment, MagicPrefix, PaddingAlign, PaddingSpec,
};
use std::io::Write;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForEachRefFormat {
    inner: ExpandFormat<ForEachRefAtom>,
    segments: Vec<ForEachRefFormatSegment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForEachRefFormatSegment {
    Literal(Vec<u8>),
    Atom(ForEachRefAtom),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForEachRefAtom {
    Raw(String),
    Color(String),
    RefName {
        source: ForEachRefNameSource,
        format: ForEachRefNameFormat,
    },
    ObjectName {
        peeled: bool,
        abbrev: Option<usize>,
    },
    Identity {
        peeled: bool,
        role: ForEachRefAtomIdentityRole,
        part: ForEachRefAtomIdentityPart,
    },
    ContentsLines {
        peeled: bool,
        count: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForEachRefNameSource {
    Ref,
    Upstream,
    Push,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForEachRefNameFormat {
    Full,
    Short,
    Strip(ForEachRefStrip),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForEachRefStrip {
    pub direction: ForEachRefStripDirection,
    pub count: isize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForEachRefStripDirection {
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForEachRefAtomIdentityRole {
    Author,
    Committer,
    Tagger,
    Creator,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForEachRefAtomIdentityPart {
    Full,
    Name,
    Email(ForEachRefEmailMode),
    Date(DateMode),
    DateRaw,
}

impl ForEachRefAtom {
    fn parse(value: &str) -> Result<Self> {
        // git's parse_ref_filter_atom: an empty sub-argument list is treated as
        // NULL, i.e. `%(atom:)` is equivalent to `%(atom)`. The arg is whatever
        // follows the FIRST colon, so only a trailing colon at that position is
        // dropped (e.g. `refname:` -> `refname`).
        let value = match value.split_once(':') {
            Some((name, "")) => name,
            _ => value,
        };
        if let Some(color) = value.strip_prefix("color:") {
            return Ok(Self::Color(color.to_string()));
        }
        if let Some(atom) = parse_for_each_ref_refname_atom(value)? {
            return Ok(atom);
        }
        if let Some(atom) = parse_for_each_ref_objectname_atom(value)? {
            return Ok(atom);
        }
        if let Some(atom) = parse_for_each_ref_identity_atom(value) {
            return Ok(atom);
        }
        if let Some(count) = value.strip_prefix("contents:lines=") {
            return Ok(Self::ContentsLines {
                peeled: false,
                count: parse_for_each_ref_contents_lines_count(count)?,
            });
        }
        if let Some(count) = value.strip_prefix("*contents:lines=") {
            return Ok(Self::ContentsLines {
                peeled: true,
                count: parse_for_each_ref_contents_lines_count(count)?,
            });
        }
        Ok(Self::Raw(value.to_string()))
    }
}

struct ForEachRefAtomTable;

impl AtomTable for ForEachRefAtomTable {
    type Atom = ForEachRefAtom;

    fn parse_atom(&self, value: &str) -> Result<Self::Atom> {
        ForEachRefAtom::parse(value)
    }
}

fn parse_for_each_ref_refname_atom(value: &str) -> Result<Option<ForEachRefAtom>> {
    for (prefix, source) in [
        ("refname", ForEachRefNameSource::Ref),
        ("upstream", ForEachRefNameSource::Upstream),
        ("push", ForEachRefNameSource::Push),
    ] {
        if value == prefix {
            return Ok(Some(ForEachRefAtom::RefName {
                source,
                format: ForEachRefNameFormat::Full,
            }));
        }
        let Some(modifier) = value
            .strip_prefix(prefix)
            .and_then(|value| value.strip_prefix(':'))
        else {
            continue;
        };
        let format = if modifier == "short" {
            ForEachRefNameFormat::Short
        } else if let Some(count) = modifier
            .strip_prefix("lstrip=")
            .or_else(|| modifier.strip_prefix("strip="))
        {
            ForEachRefNameFormat::Strip(ForEachRefStrip {
                direction: ForEachRefStripDirection::Left,
                count: parse_for_each_ref_strip_count(count)?,
            })
        } else if let Some(count) = modifier.strip_prefix("rstrip=") {
            ForEachRefNameFormat::Strip(ForEachRefStrip {
                direction: ForEachRefStripDirection::Right,
                count: parse_for_each_ref_strip_count(count)?,
            })
        } else if prefix == "refname" {
            // git's refname_atom_parser rejects unknown args outright (the
            // upstream/push variants accept extra modifiers handled later, so
            // only `refname` is strict here).
            eprintln!("fatal: unrecognized %({prefix}) argument: {modifier}");
            return Err(GitError::Exit(128));
        } else {
            continue;
        };
        return Ok(Some(ForEachRefAtom::RefName { source, format }));
    }
    Ok(None)
}

fn parse_for_each_ref_objectname_atom(value: &str) -> Result<Option<ForEachRefAtom>> {
    for (prefix, peeled) in [("objectname", false), ("*objectname", true)] {
        if value == prefix {
            return Ok(Some(ForEachRefAtom::ObjectName {
                peeled,
                abbrev: None,
            }));
        }
        if value.strip_prefix(prefix) == Some(":short") {
            return Ok(Some(ForEachRefAtom::ObjectName {
                peeled,
                abbrev: Some(0),
            }));
        }
        if let Some(width) = value
            .strip_prefix(prefix)
            .and_then(|value| value.strip_prefix(":short="))
        {
            return Ok(Some(ForEachRefAtom::ObjectName {
                peeled,
                abbrev: Some(parse_for_each_ref_abbrev_width(width)?),
            }));
        }
    }
    Ok(None)
}

fn parse_for_each_ref_identity_atom(value: &str) -> Option<ForEachRefAtom> {
    let (value, peeled) = value
        .strip_prefix('*')
        .map(|value| (value, true))
        .unwrap_or((value, false));
    let (atom, has_modifier) = value
        .split_once(':')
        .map_or((value, false), |(atom, _)| (atom, true));
    // `name` and the bare-identity atoms take no modifier in this typed path;
    // anything with a `:` (e.g. `authorname:mailmap`, `author:foo`) falls through
    // to the string/Raw renderer which owns the full option grammar + errors.
    let plain = |part: ForEachRefAtomIdentityPart| if has_modifier { None } else { Some(part) };
    let (role, part) = match atom {
        "author" => (
            ForEachRefAtomIdentityRole::Author,
            plain(ForEachRefAtomIdentityPart::Full)?,
        ),
        "authorname" => (
            ForEachRefAtomIdentityRole::Author,
            plain(ForEachRefAtomIdentityPart::Name)?,
        ),
        "committer" => (
            ForEachRefAtomIdentityRole::Committer,
            plain(ForEachRefAtomIdentityPart::Full)?,
        ),
        "committername" => (
            ForEachRefAtomIdentityRole::Committer,
            plain(ForEachRefAtomIdentityPart::Name)?,
        ),
        "tagger" => (
            ForEachRefAtomIdentityRole::Tagger,
            plain(ForEachRefAtomIdentityPart::Full)?,
        ),
        "taggername" => (
            ForEachRefAtomIdentityRole::Tagger,
            plain(ForEachRefAtomIdentityPart::Name)?,
        ),
        "creator" => (
            ForEachRefAtomIdentityRole::Creator,
            plain(ForEachRefAtomIdentityPart::Full)?,
        ),
        _ => return None,
    };
    Some(ForEachRefAtom::Identity { peeled, role, part })
}

pub fn parse_for_each_ref_contents_lines_count(value: &str) -> Result<usize> {
    value
        .parse::<usize>()
        .map_err(|_| GitError::Command(format!("invalid for-each-ref contents line count {value}")))
}

impl ForEachRefFormat {
    pub fn parse(format_spec: &str) -> Result<Self> {
        let inner = ExpandFormat::parse(format_spec, &ForEachRefAtomTable)?;
        let segments = inner
            .segments()
            .iter()
            .filter_map(|segment| match segment {
                ExpandSegment::Literal(literal) => {
                    Some(ForEachRefFormatSegment::Literal(literal.clone()))
                }
                ExpandSegment::Atom(atom) => Some(ForEachRefFormatSegment::Atom(atom.atom.clone())),
                ExpandSegment::Padding(_) => None,
            })
            .collect();
        Ok(Self { inner, segments })
    }

    pub fn segments(&self) -> &[ForEachRefFormatSegment] {
        &self.segments
    }

    /// Mirror git's `need_color_reset_at_eol`: true when the format contains at
    /// least one `%(color:...)` atom and the last such atom is not
    /// `%(color:reset)`. The caller still gates this on color being enabled.
    pub fn ends_with_unreset_color(&self) -> bool {
        let mut need_reset = false;
        for segment in &self.segments {
            if let ForEachRefFormatSegment::Atom(ForEachRefAtom::Color(value)) = segment {
                need_reset = value.trim() != "reset";
            }
        }
        need_reset
    }
}

pub fn write_for_each_ref_format(
    stdout: &mut impl Write,
    format: &ForEachRefFormat,
    quote: ForEachRefQuoteMode,
    reset_color_at_eol: bool,
    mut write_atom: impl FnMut(&mut Vec<u8>, &ForEachRefAtom) -> Result<()>,
) -> Result<()> {
    if !format
        .inner
        .segments()
        .iter()
        .any(for_each_ref_segment_has_control)
    {
        format
            .inner
            .write_to(stdout, &mut write_atom, |stdout, value| {
                write_for_each_ref_quoted_atom(stdout, value, quote)
            })?;
        if reset_color_at_eol {
            stdout.write_all(b"\x1b[m")?;
        }
        return Ok(());
    }

    let mut rendered = Vec::new();
    let (idx, stop) = write_for_each_ref_format_range(
        &mut rendered,
        format.inner.segments(),
        0,
        &[],
        quote,
        &mut write_atom,
    )?;
    if idx != format.inner.segments().len() || stop.is_some() {
        return Err(GitError::Command(
            "improper for-each-ref format control atom usage".into(),
        ));
    }
    stdout.write_all(&rendered)?;
    if reset_color_at_eol {
        stdout.write_all(b"\x1b[m")?;
    }
    Ok(())
}

fn for_each_ref_segment_has_control(segment: &ExpandSegment<ForEachRefAtom>) -> bool {
    match segment {
        ExpandSegment::Atom(atom) => for_each_ref_control_atom(&atom.atom).is_some(),
        ExpandSegment::Literal(_) | ExpandSegment::Padding(_) => false,
    }
}

fn write_for_each_ref_format_range(
    out: &mut Vec<u8>,
    segments: &[ExpandSegment<ForEachRefAtom>],
    mut idx: usize,
    stops: &[ForEachRefControlStop],
    quote: ForEachRefQuoteMode,
    write_atom: &mut impl FnMut(&mut Vec<u8>, &ForEachRefAtom) -> Result<()>,
) -> Result<(usize, Option<ForEachRefControlStop>)> {
    let mut pending_padding = None;
    while idx < segments.len() {
        match &segments[idx] {
            ExpandSegment::Literal(literal) => out.extend_from_slice(literal),
            ExpandSegment::Padding(padding) => pending_padding = Some(*padding),
            ExpandSegment::Atom(atom) => {
                if let Some(control) = for_each_ref_control_atom(&atom.atom) {
                    if let Some(stop) = control.stop()
                        && stops.contains(&stop)
                    {
                        return Ok((idx, Some(stop)));
                    }
                    match control {
                        ForEachRefControlAtom::Align(options) => {
                            let (value, next) =
                                render_for_each_ref_align(segments, idx + 1, &options, write_atom)?;
                            let mut value = value;
                            apply_for_each_ref_padding(&mut value, pending_padding.take());
                            apply_for_each_ref_magic(out, atom.magic, &value);
                            write_for_each_ref_quoted_atom(out, &value, quote)?;
                            idx = next;
                            continue;
                        }
                        ForEachRefControlAtom::If(condition) => {
                            let (value, next) = render_for_each_ref_if(
                                segments,
                                idx + 1,
                                &condition,
                                quote,
                                write_atom,
                            )?;
                            let mut value = value;
                            apply_for_each_ref_padding(&mut value, pending_padding.take());
                            apply_for_each_ref_magic(out, atom.magic, &value);
                            out.extend_from_slice(&value);
                            idx = next;
                            continue;
                        }
                        ForEachRefControlAtom::Then
                        | ForEachRefControlAtom::Else
                        | ForEachRefControlAtom::End => {
                            return Err(GitError::Command(
                                "improper for-each-ref format control atom usage".into(),
                            ));
                        }
                    }
                }

                let mut value = Vec::new();
                write_atom(&mut value, &atom.atom)?;
                apply_for_each_ref_padding(&mut value, pending_padding.take());
                apply_for_each_ref_magic(out, atom.magic, &value);
                write_for_each_ref_quoted_atom(out, &value, quote)?;
            }
        }
        idx += 1;
    }
    Ok((idx, None))
}

fn render_for_each_ref_align(
    segments: &[ExpandSegment<ForEachRefAtom>],
    start: usize,
    options: &ForEachRefAlignOptions,
    write_atom: &mut impl FnMut(&mut Vec<u8>, &ForEachRefAtom) -> Result<()>,
) -> Result<(Vec<u8>, usize)> {
    let mut value = Vec::new();
    let (idx, stop) = write_for_each_ref_format_range(
        &mut value,
        segments,
        start,
        &[ForEachRefControlStop::End],
        ForEachRefQuoteMode::None,
        write_atom,
    )?;
    if stop != Some(ForEachRefControlStop::End) {
        return Err(GitError::Command("missing %(end) atom for %(align)".into()));
    }
    apply_for_each_ref_align(&mut value, options);
    Ok((value, idx + 1))
}

fn render_for_each_ref_if(
    segments: &[ExpandSegment<ForEachRefAtom>],
    start: usize,
    condition: &ForEachRefIfCondition,
    quote: ForEachRefQuoteMode,
    write_atom: &mut impl FnMut(&mut Vec<u8>, &ForEachRefAtom) -> Result<()>,
) -> Result<(Vec<u8>, usize)> {
    let mut test = Vec::new();
    let (then_idx, stop) = write_for_each_ref_format_range(
        &mut test,
        segments,
        start,
        &[ForEachRefControlStop::Then],
        ForEachRefQuoteMode::None,
        write_atom,
    )?;
    if stop != Some(ForEachRefControlStop::Then) {
        return Err(GitError::Command("missing %(then) atom for %(if)".into()));
    }

    let mut true_value = Vec::new();
    let (branch_idx, branch_stop) = write_for_each_ref_format_range(
        &mut true_value,
        segments,
        then_idx + 1,
        &[ForEachRefControlStop::Else, ForEachRefControlStop::End],
        quote,
        write_atom,
    )?;

    let mut false_value = Vec::new();
    let end_idx = match branch_stop {
        Some(ForEachRefControlStop::End) => branch_idx,
        Some(ForEachRefControlStop::Else) => {
            let (idx, stop) = write_for_each_ref_format_range(
                &mut false_value,
                segments,
                branch_idx + 1,
                &[ForEachRefControlStop::End],
                quote,
                write_atom,
            )?;
            if stop != Some(ForEachRefControlStop::End) {
                return Err(GitError::Command("missing %(end) atom for %(if)".into()));
            }
            idx
        }
        Some(ForEachRefControlStop::Then) | None => {
            return Err(GitError::Command("missing %(end) atom for %(if)".into()));
        }
    };

    let test = trim_ascii(&test);
    let matched = match condition {
        ForEachRefIfCondition::NonEmpty => !test.is_empty(),
        ForEachRefIfCondition::Equals(value) => test == value.as_bytes(),
        ForEachRefIfCondition::NotEquals(value) => test != value.as_bytes(),
    };
    Ok((if matched { true_value } else { false_value }, end_idx + 1))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ForEachRefControlStop {
    Then,
    Else,
    End,
}

enum ForEachRefControlAtom {
    Align(ForEachRefAlignOptions),
    If(ForEachRefIfCondition),
    Then,
    Else,
    End,
}

impl ForEachRefControlAtom {
    fn stop(&self) -> Option<ForEachRefControlStop> {
        match self {
            Self::Then => Some(ForEachRefControlStop::Then),
            Self::Else => Some(ForEachRefControlStop::Else),
            Self::End => Some(ForEachRefControlStop::End),
            Self::Align(_) | Self::If(_) => None,
        }
    }
}

#[derive(Clone, Copy)]
enum ForEachRefAlignPosition {
    Left,
    Middle,
    Right,
}

struct ForEachRefAlignOptions {
    width: usize,
    position: ForEachRefAlignPosition,
}

enum ForEachRefIfCondition {
    NonEmpty,
    Equals(String),
    NotEquals(String),
}

fn for_each_ref_control_atom(atom: &ForEachRefAtom) -> Option<ForEachRefControlAtom> {
    let ForEachRefAtom::Raw(value) = atom else {
        return None;
    };
    if let Some(options) = value.strip_prefix("align:") {
        return parse_for_each_ref_align_options(options).map(ForEachRefControlAtom::Align);
    }
    if value == "if" {
        return Some(ForEachRefControlAtom::If(ForEachRefIfCondition::NonEmpty));
    }
    if let Some(expected) = value.strip_prefix("if:equals=") {
        return Some(ForEachRefControlAtom::If(ForEachRefIfCondition::Equals(
            expected.to_string(),
        )));
    }
    if let Some(expected) = value.strip_prefix("if:notequals=") {
        return Some(ForEachRefControlAtom::If(ForEachRefIfCondition::NotEquals(
            expected.to_string(),
        )));
    }
    match value.as_str() {
        "then" => Some(ForEachRefControlAtom::Then),
        "else" => Some(ForEachRefControlAtom::Else),
        "end" => Some(ForEachRefControlAtom::End),
        _ => None,
    }
}

fn parse_for_each_ref_align_options(value: &str) -> Option<ForEachRefAlignOptions> {
    let mut width = None;
    let mut position = ForEachRefAlignPosition::Left;
    for part in value.split(',') {
        if let Some(rest) = part.strip_prefix("width=") {
            width = rest.parse::<usize>().ok();
        } else if let Some(rest) = part.strip_prefix("position=") {
            position = parse_for_each_ref_align_position(rest)?;
        } else if let Ok(parsed) = part.parse::<usize>() {
            width = Some(parsed);
        } else {
            position = parse_for_each_ref_align_position(part)?;
        }
    }
    Some(ForEachRefAlignOptions {
        width: width?,
        position,
    })
}

fn parse_for_each_ref_align_position(value: &str) -> Option<ForEachRefAlignPosition> {
    match value {
        "left" => Some(ForEachRefAlignPosition::Left),
        "middle" => Some(ForEachRefAlignPosition::Middle),
        "right" => Some(ForEachRefAlignPosition::Right),
        _ => None,
    }
}

fn apply_for_each_ref_align(value: &mut Vec<u8>, options: &ForEachRefAlignOptions) {
    let width = for_each_ref_display_width(value);
    if width >= options.width {
        return;
    }
    let extra = options.width - width;
    let (left, right) = match options.position {
        ForEachRefAlignPosition::Left => (0, extra),
        ForEachRefAlignPosition::Middle => (extra / 2, extra - extra / 2),
        ForEachRefAlignPosition::Right => (extra, 0),
    };
    let mut padded = Vec::with_capacity(value.len() + extra);
    padded.extend(std::iter::repeat_n(b' ', left));
    padded.extend_from_slice(value);
    padded.extend(std::iter::repeat_n(b' ', right));
    *value = padded;
}

fn apply_for_each_ref_magic(out: &mut Vec<u8>, magic: MagicPrefix, value: &[u8]) {
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

fn apply_for_each_ref_padding(value: &mut Vec<u8>, padding: Option<PaddingSpec>) {
    let Some(padding) = padding else {
        return;
    };
    let width = for_each_ref_display_width(value);
    let target = padding.width.max(0) as usize;
    if width >= target {
        return;
    }
    let extra = target - width;
    let (left, right) = match padding.align {
        PaddingAlign::Left => (0, extra),
        PaddingAlign::Right | PaddingAlign::LeftAndSteal => (extra, 0),
        PaddingAlign::Center => (extra / 2, extra - extra / 2),
    };
    let mut padded = Vec::with_capacity(value.len() + extra);
    padded.extend(std::iter::repeat_n(b' ', left));
    padded.extend_from_slice(value);
    padded.extend(std::iter::repeat_n(b' ', right));
    *value = padded;
}

fn for_each_ref_display_width(value: &[u8]) -> usize {
    let mut width = 0usize;
    let mut idx = 0usize;
    while idx < value.len() {
        if value[idx] == 0x1b
            && value.get(idx + 1) == Some(&b'[')
            && let Some(end) = value[idx + 2..]
                .iter()
                .position(|byte| (0x40..=0x7e).contains(byte))
        {
            idx += end + 3;
            continue;
        }
        width += 1;
        idx += 1;
    }
    width
}

fn trim_ascii(value: &[u8]) -> &[u8] {
    let start = value
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(value.len());
    let end = value
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map(|idx| idx + 1)
        .unwrap_or(start);
    &value[start..end]
}

#[derive(Debug, Default, Clone, Copy, Eq, PartialEq)]
pub enum ForEachRefQuoteMode {
    #[default]
    None,
    Shell,
    Python,
    Perl,
    Tcl,
}

pub fn write_for_each_ref_quoted_atom(
    stdout: &mut impl Write,
    value: &[u8],
    quote: ForEachRefQuoteMode,
) -> Result<()> {
    match quote {
        ForEachRefQuoteMode::None => stdout.write_all(value)?,
        ForEachRefQuoteMode::Shell => {
            stdout.write_all(b"'")?;
            for byte in value {
                if *byte == b'\'' {
                    stdout.write_all(br#"'\''"#)?;
                } else {
                    stdout.write_all(&[*byte])?;
                }
            }
            stdout.write_all(b"'")?;
        }
        ForEachRefQuoteMode::Python | ForEachRefQuoteMode::Perl => {
            stdout.write_all(b"'")?;
            for byte in value {
                match (*byte, quote) {
                    (b'\\', _) => stdout.write_all(br#"\\"#)?,
                    (b'\'', _) => stdout.write_all(br#"\'"#)?,
                    (b'\n', ForEachRefQuoteMode::Python) => stdout.write_all(br#"\n"#)?,
                    _ => stdout.write_all(&[*byte])?,
                }
            }
            stdout.write_all(b"'")?;
        }
        ForEachRefQuoteMode::Tcl => {
            stdout.write_all(b"\"")?;
            for byte in value {
                match *byte {
                    b'\\' => stdout.write_all(br#"\\"#)?,
                    b'"' => stdout.write_all(br#"\""#)?,
                    b'\n' => stdout.write_all(br#"\n"#)?,
                    _ => stdout.write_all(&[*byte])?,
                }
            }
            stdout.write_all(b"\"")?;
        }
    }
    Ok(())
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ForEachRefTrack {
    pub ahead: usize,
    pub behind: usize,
    /// The upstream is configured but its ref no longer resolves; git renders
    /// `%(upstream:track)` as `[gone]` and `%(upstream:trackshort)` as empty.
    pub gone: bool,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum ForEachRefEmailMode {
    #[default]
    Bracketed,
    Trim,
    LocalPart,
}

pub fn write_for_each_ref_track(
    stdout: &mut impl Write,
    track: ForEachRefTrack,
    bracketed: bool,
) -> Result<()> {
    if track.gone {
        // git emits a literal "[gone]" (or bare "gone" with nobracket) when the
        // configured upstream no longer resolves.
        if bracketed {
            stdout.write_all(b"[gone]")?;
        } else {
            stdout.write_all(b"gone")?;
        }
        return Ok(());
    }
    if bracketed && (track.ahead > 0 || track.behind > 0) {
        stdout.write_all(b"[")?;
    }
    match (track.ahead, track.behind) {
        (0, _) => {}
        (ahead, 0) => write!(stdout, "ahead {ahead}")?,
        (ahead, behind) => write!(stdout, "ahead {ahead}, behind {behind}")?,
    }
    if track.ahead == 0 && track.behind > 0 {
        write!(stdout, "behind {}", track.behind)?;
    }
    if bracketed && (track.ahead > 0 || track.behind > 0) {
        stdout.write_all(b"]")?;
    }
    Ok(())
}

pub fn for_each_ref_track_short(track: ForEachRefTrack) -> &'static str {
    if track.gone {
        // git's trackshort is empty for a gone upstream.
        return "";
    }
    match (track.ahead, track.behind) {
        (0, 0) => "=",
        (_, 0) => ">",
        (0, _) => "<",
        (_, _) => "<>",
    }
}

pub fn write_for_each_ref_identity(stdout: &mut impl Write, identity: Option<&[u8]>) -> Result<()> {
    if let Some(identity) = identity {
        stdout.write_all(identity)?;
    }
    Ok(())
}

pub fn write_for_each_ref_identity_name(
    stdout: &mut impl Write,
    identity: Option<&[u8]>,
) -> Result<()> {
    if let Some(identity) = identity
        && let Some(name) = for_each_ref_identity_name(identity)
    {
        stdout.write_all(name)?;
    }
    Ok(())
}

pub fn write_for_each_ref_identity_email(
    stdout: &mut impl Write,
    identity: Option<&[u8]>,
) -> Result<()> {
    write_for_each_ref_identity_email_mode(stdout, identity, ForEachRefEmailMode::Bracketed)
}

pub fn write_for_each_ref_identity_email_mode(
    stdout: &mut impl Write,
    identity: Option<&[u8]>,
    mode: ForEachRefEmailMode,
) -> Result<()> {
    if let Some(identity) = identity
        && let Some(email) = for_each_ref_identity_email(identity, mode)
    {
        stdout.write_all(email)?;
    }
    Ok(())
}

pub fn write_for_each_ref_identity_date_raw(
    stdout: &mut impl Write,
    identity: Option<&[u8]>,
) -> Result<()> {
    if let Some(identity) = identity
        && let Some(date) = for_each_ref_identity_date_raw(identity)
    {
        stdout.write_all(date)?;
    }
    Ok(())
}

pub fn write_for_each_ref_identity_date(
    stdout: &mut impl Write,
    identity: Option<&[u8]>,
) -> Result<()> {
    write_for_each_ref_identity_date_mode(stdout, identity, &DateMode::Default)
}

pub fn write_for_each_ref_identity_date_mode(
    stdout: &mut impl Write,
    identity: Option<&[u8]>,
    mode: &DateMode,
) -> Result<()> {
    if let Some(identity) = identity
        && let Some(date) = for_each_ref_identity_date(identity, mode)
    {
        stdout.write_all(date.as_bytes())?;
    }
    Ok(())
}

pub fn for_each_ref_identity_name(identity: &[u8]) -> Option<&[u8]> {
    let marker = identity.windows(2).position(|window| window == b" <")?;
    Some(&identity[..marker])
}

pub fn for_each_ref_identity_email(identity: &[u8], mode: ForEachRefEmailMode) -> Option<&[u8]> {
    let start = identity.iter().position(|byte| *byte == b'<')?;
    let end = identity[start..].iter().position(|byte| *byte == b'>')?;
    let bracketed = &identity[start..=start + end];
    match mode {
        ForEachRefEmailMode::Bracketed => Some(bracketed),
        ForEachRefEmailMode::Trim => Some(&identity[start + 1..start + end]),
        ForEachRefEmailMode::LocalPart => {
            let trimmed = &identity[start + 1..start + end];
            let at = trimmed.iter().position(|byte| *byte == b'@')?;
            Some(&trimmed[..at])
        }
    }
}

pub fn for_each_ref_identity_date_raw(identity: &[u8]) -> Option<&[u8]> {
    let email_end = identity.iter().position(|byte| *byte == b'>')?;
    let rest = identity.get(email_end + 1..)?.strip_prefix(b" ")?;
    let timestamp_end = rest.iter().position(|byte| *byte == b' ')?;
    let timezone = rest.get(timestamp_end + 1..)?;
    if timezone.len() == 5
        && matches!(timezone[0], b'+' | b'-')
        && timezone[1..].iter().all(|byte| byte.is_ascii_digit())
    {
        Some(rest)
    } else {
        None
    }
}

pub fn for_each_ref_identity_date(identity: &[u8], mode: &DateMode) -> Option<String> {
    let raw = std::str::from_utf8(for_each_ref_identity_date_raw(identity)?).ok()?;
    let (timestamp, timezone) = raw.split_once(' ')?;
    let timestamp = timestamp.parse::<i64>().ok()?;
    mode.render(timestamp, timezone)
}

pub fn for_each_ref_identity_timestamp(identity: &[u8]) -> Option<i64> {
    let raw = std::str::from_utf8(for_each_ref_identity_date_raw(identity)?).ok()?;
    let (timestamp, _) = raw.split_once(' ')?;
    timestamp.parse::<i64>().ok()
}

/// The signature begin-markers git recognizes (`gpg-interface.c` format table).
/// A message line beginning with one of these starts the trailing signature.
const FOR_EACH_REF_SIGNATURE_MARKERS: [&[u8]; 4] = [
    b"-----BEGIN PGP SIGNATURE-----",
    b"-----BEGIN PGP MESSAGE-----",
    b"-----BEGIN SIGNED MESSAGE-----",
    b"-----BEGIN SSH SIGNATURE-----",
];

/// Offset into `message` where the trailing signature begins, or the message
/// length when unsigned. Mirrors gpg-interface.c `parse_signed_buffer`: the
/// LAST line that starts with a signature marker wins.
fn for_each_ref_signature_start(message: &[u8]) -> usize {
    let mut start = 0;
    let mut sig = message.len();
    while start < message.len() {
        let line = &message[start..];
        if FOR_EACH_REF_SIGNATURE_MARKERS
            .iter()
            .any(|marker| line.starts_with(marker))
        {
            sig = start;
        }
        match line.iter().position(|byte| *byte == b'\n') {
            Some(eol) => start += eol + 1,
            None => break,
        }
    }
    sig
}

/// The split of a commit/tag message into the regions git's for-each-ref atoms
/// expose, mirroring ref-filter.c `find_subpos`.
pub struct ForEachRefMessageParts<'a> {
    /// The subject line(s), with no trailing newline (raw bytes; callers run
    /// `for_each_ref_copy_subject` to collapse embedded newlines).
    pub subject: &'a [u8],
    /// `%(contents:body)` — body with the signature removed.
    pub body_without_sig: &'a [u8],
    /// `%(body)` (legacy) — body *including* the signature.
    pub body_with_sig: &'a [u8],
    /// `%(contents:signature)` — the trailing signature block (may be empty).
    pub signature: &'a [u8],
    /// `%(contents)` / `%(contents:size)` — the message from the subject start
    /// (after leading blank lines) to the end.
    pub bare: &'a [u8],
}

/// Split a commit/tag message into the for-each-ref content regions, mirroring
/// ref-filter.c `find_subpos`. `message` is the header-stripped message (sley
/// already strips object headers before this point).
pub fn for_each_ref_message_parts(message: &[u8]) -> ForEachRefMessageParts<'_> {
    // Skip any leading empty lines (the header/body separator is already gone).
    let mut start = 0;
    while message.get(start) == Some(&b'\n') {
        start += 1;
    }
    let buf = &message[start..];
    let bare = buf;
    let sigstart = for_each_ref_signature_start(buf);
    let signature = &buf[sigstart..];

    // Subject runs to the first blank line before the signature, else to the
    // signature start (treating the whole pre-sig message as subject).
    let subject_region = &buf[..sigstart];
    let subject_end = for_each_ref_blank_line(subject_region).unwrap_or(sigstart);
    let mut sublen = subject_end;
    while sublen > 0 && matches!(buf[sublen - 1], b'\n' | b'\r') {
        sublen -= 1;
    }
    let subject = &buf[..sublen];

    // Body begins after the subject's trailing blank lines.
    let mut body_start = subject_end;
    while body_start < buf.len() && matches!(buf[body_start], b'\n' | b'\r') {
        body_start += 1;
    }
    let body_with_sig = &buf[body_start..];
    let body_without_sig = &buf[body_start..sigstart.max(body_start)];
    ForEachRefMessageParts {
        subject,
        body_without_sig,
        body_with_sig,
        signature,
        bare,
    }
}

/// Find the byte offset of the first blank-line separator (`\n\n` or
/// `\r\n\r\n`) in `buf`, returning the offset of the first newline of the pair.
fn for_each_ref_blank_line(buf: &[u8]) -> Option<usize> {
    let lf = buf.windows(2).position(|window| window == b"\n\n");
    let crlf = buf.windows(4).position(|window| window == b"\r\n\r\n");
    match (lf, crlf) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

/// `copy_subject`: render the subject with embedded newlines turned into single
/// spaces (CRLF's CR is dropped), matching ref-filter.c.
pub fn for_each_ref_copy_subject(subject: &[u8]) -> String {
    let mut out = String::with_capacity(subject.len());
    let mut idx = 0;
    while idx < subject.len() {
        let byte = subject[idx];
        if byte == b'\r' && subject.get(idx + 1) == Some(&b'\n') {
            idx += 1;
            continue;
        }
        if byte == b'\n' {
            out.push(' ');
        } else {
            out.push(byte as char);
        }
        idx += 1;
    }
    out
}

/// `format_sanitized_subject`: replace non-title-character runs with a single
/// `-`, collapse consecutive `.`, and trim trailing `.`/`-` (pretty.c).
pub fn for_each_ref_sanitize_subject(subject: &str) -> String {
    let bytes = subject.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut space = 2u8; // git's initial `space = 2`
    let mut idx = 0;
    while idx < bytes.len() {
        let byte = bytes[idx];
        if for_each_ref_istitlechar(byte) {
            if space == 1 {
                out.push(b'-');
            }
            space = 0;
            out.push(byte);
            if byte == b'.' {
                while bytes.get(idx + 1) == Some(&b'.') {
                    idx += 1;
                }
            }
        } else {
            space |= 1;
        }
        idx += 1;
    }
    while matches!(out.last(), Some(b'.') | Some(b'-')) {
        out.pop();
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn for_each_ref_istitlechar(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'.' || byte == b'_'
}

pub fn for_each_ref_short_name(refname: &str) -> &str {
    if let Some(remote) = refname.strip_prefix("refs/remotes/")
        && let Some(remote_name) = remote.strip_suffix("/HEAD")
    {
        return remote_name;
    }
    refname
        .strip_prefix("refs/heads/")
        .or_else(|| refname.strip_prefix("refs/tags/"))
        .or_else(|| refname.strip_prefix("refs/remotes/"))
        .unwrap_or(refname)
}

/// git's `ref_rev_parse_rules`: the format patterns tried (shortest-name first)
/// when resolving an abbreviated ref, and in reverse when shortening one.
const REF_REV_PARSE_RULES: [&str; 6] = [
    "{}",
    "refs/{}",
    "refs/tags/{}",
    "refs/heads/{}",
    "refs/remotes/{}",
    "refs/remotes/{}/HEAD",
];

fn expand_ref_rule(rule: &str, short: &str) -> String {
    rule.replace("{}", short)
}

/// Strip the prefix/suffix of a rev-parse rule from `refname`, returning the
/// `%.*s` portion if the rule matches (git's `match_parse_rule`).
fn match_ref_parse_rule<'a>(refname: &'a str, rule: &str) -> Option<&'a str> {
    let (prefix, suffix) = rule.split_once("{}")?;
    refname
        .strip_prefix(prefix)
        .and_then(|rest| rest.strip_suffix(suffix))
}

/// git's `shorten_unambiguous_ref`: find the shortest abbreviation of `refname`
/// that, under the rev-parse rules, resolves back to exactly this ref.
/// `strict` (git's `core.warnambiguousrefs`, default true) requires *all* other
/// rules to fail; otherwise only rules that sort before the matched one matter.
/// `ref_exists` reports whether a fully-qualified refname is present.
pub fn shorten_unambiguous_ref(
    refname: &str,
    strict: bool,
    ref_exists: impl Fn(&str) -> bool,
) -> String {
    // Skip rule 0 ("{}"), which always matches.
    for matched in (1..REF_REV_PARSE_RULES.len()).rev() {
        let Some(short) = match_ref_parse_rule(refname, REF_REV_PARSE_RULES[matched]) else {
            continue;
        };
        let rules_to_fail = if strict {
            REF_REV_PARSE_RULES.len()
        } else {
            matched
        };
        let ambiguous = (0..rules_to_fail).any(|rule_idx| {
            rule_idx != matched
                && ref_exists(&expand_ref_rule(REF_REV_PARSE_RULES[rule_idx], short))
        });
        if !ambiguous {
            return short.to_string();
        }
    }
    refname.to_string()
}

pub fn parse_for_each_ref_strip_count(value: &str) -> Result<isize> {
    value
        .parse::<isize>()
        .map_err(|_| GitError::Command(format!("invalid refname strip count {value}")))
}

pub fn for_each_ref_lstrip_name(refname: &str, count: isize) -> String {
    let components = refname.split('/').collect::<Vec<_>>();
    if count == 0 {
        return refname.to_string();
    }
    let start = if count > 0 {
        (count as usize).min(components.len())
    } else {
        components.len().saturating_sub(count.unsigned_abs())
    };
    components[start..].join("/")
}

pub fn for_each_ref_rstrip_name(refname: &str, count: isize) -> String {
    let components = refname.split('/').collect::<Vec<_>>();
    if count == 0 {
        return refname.to_string();
    }
    let end = if count > 0 {
        components.len().saturating_sub(count as usize)
    } else {
        count.unsigned_abs().min(components.len())
    };
    components[..end].join("/")
}

pub fn for_each_ref_abbrev_oid(
    oid: &ObjectId,
    width: Option<usize>,
    candidates: &[ObjectId],
) -> String {
    let hex = oid.to_hex();
    let mut width = oid.abbrev_hex_len(width.unwrap_or(hex.len()));
    while width < hex.len() {
        let prefix = &hex.as_bytes()[..width];
        if !candidates
            .iter()
            .any(|candidate| candidate != oid && candidate.hex_prefix_matches(prefix))
        {
            break;
        }
        width += 1;
    }
    hex[..width].to_string()
}

pub fn parse_for_each_ref_abbrev_width(value: &str) -> Result<usize> {
    let width = value
        .parse::<usize>()
        .ok()
        .filter(|width| *width > 0)
        .ok_or_else(|| {
            GitError::Command(format!(
                "positive value expected in for-each-ref objectname:short format: {value}"
            ))
        })?;
    Ok(width.max(4))
}

pub fn commit_identity_date(raw: &[u8], mode: &DateMode) -> String {
    for_each_ref_identity_date(raw, mode).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sley_core::ObjectFormat;

    #[test]
    fn format_parser_decodes_literals_atoms_and_percent_escapes() {
        let format =
            ForEachRefFormat::parse("refs/%%/%(refname)%09%(objectname)%q").expect("valid format");
        assert_eq!(
            format.segments(),
            &[
                ForEachRefFormatSegment::Literal(b"refs/%/".to_vec()),
                ForEachRefFormatSegment::Atom(ForEachRefAtom::RefName {
                    source: ForEachRefNameSource::Ref,
                    format: ForEachRefNameFormat::Full
                }),
                ForEachRefFormatSegment::Literal(b"\t".to_vec()),
                ForEachRefFormatSegment::Atom(ForEachRefAtom::ObjectName {
                    peeled: false,
                    abbrev: None
                }),
                ForEachRefFormatSegment::Literal(b"%q".to_vec()),
            ]
        );
    }

    #[test]
    fn format_parser_decodes_typed_ref_filter_atoms() {
        let format = ForEachRefFormat::parse(
            "%(refname:short) %(upstream:lstrip=2) %(*objectname:short=7) %(authoremail:trim) %(authordate:iso8601-strict) %(*contents:lines=2)",
        )
        .expect("valid format");
        assert_eq!(
            format.segments(),
            &[
                ForEachRefFormatSegment::Atom(ForEachRefAtom::RefName {
                    source: ForEachRefNameSource::Ref,
                    format: ForEachRefNameFormat::Short,
                }),
                ForEachRefFormatSegment::Literal(b" ".to_vec()),
                ForEachRefFormatSegment::Atom(ForEachRefAtom::RefName {
                    source: ForEachRefNameSource::Upstream,
                    format: ForEachRefNameFormat::Strip(ForEachRefStrip {
                        direction: ForEachRefStripDirection::Left,
                        count: 2,
                    }),
                }),
                ForEachRefFormatSegment::Literal(b" ".to_vec()),
                ForEachRefFormatSegment::Atom(ForEachRefAtom::ObjectName {
                    peeled: true,
                    abbrev: Some(7),
                }),
                ForEachRefFormatSegment::Literal(b" ".to_vec()),
                // `name`/`email`/`date` atoms that carry a `:modifier` are now
                // kept as Raw placeholders; the CLI's string renderer owns the
                // full option grammar (mailmap, multi-option, all date modes)
                // and the byte-exact bad-argument errors.
                ForEachRefFormatSegment::Atom(ForEachRefAtom::Raw("authoremail:trim".to_string())),
                ForEachRefFormatSegment::Literal(b" ".to_vec()),
                ForEachRefFormatSegment::Atom(ForEachRefAtom::Raw(
                    "authordate:iso8601-strict".to_string(),
                )),
                ForEachRefFormatSegment::Literal(b" ".to_vec()),
                ForEachRefFormatSegment::Atom(ForEachRefAtom::ContentsLines {
                    peeled: true,
                    count: 2,
                }),
            ]
        );
    }

    #[test]
    fn format_parser_rejects_unterminated_atoms() {
        assert!(ForEachRefFormat::parse("%(refname").is_err());
    }

    #[test]
    fn format_parser_rejects_invalid_typed_atom_numbers() {
        assert!(ForEachRefFormat::parse("%(contents:lines=nope)").is_err());
        assert!(ForEachRefFormat::parse("%(objectname:short=0)").is_err());
        assert!(ForEachRefFormat::parse("%(refname:lstrip=nope)").is_err());
    }

    #[test]
    fn format_renderer_streams_literals_atoms_and_quotes() {
        let format = ForEachRefFormat::parse("branch=%(refname)").expect("valid format");
        let mut out = Vec::new();
        write_for_each_ref_format(
            &mut out,
            &format,
            ForEachRefQuoteMode::Shell,
            false,
            |atom, name| {
                assert_eq!(
                    name,
                    &ForEachRefAtom::RefName {
                        source: ForEachRefNameSource::Ref,
                        format: ForEachRefNameFormat::Full
                    }
                );
                atom.extend_from_slice(b"main's");
                Ok(())
            },
        )
        .expect("writes to in-memory buffer");
        assert_eq!(out, b"branch='main'\\''s'");
    }

    #[test]
    fn format_renderer_uses_shared_padding_and_magic() {
        let format =
            ForEachRefFormat::parse("x\n%-(*objectname)%>(6)%(refname)").expect("valid format");
        let mut out = Vec::new();
        write_for_each_ref_format(
            &mut out,
            &format,
            ForEachRefQuoteMode::None,
            false,
            |value, atom| {
                match atom {
                    ForEachRefAtom::ObjectName { peeled: true, .. } => {}
                    ForEachRefAtom::RefName { .. } => value.extend_from_slice(b"main"),
                    other => panic!("unexpected atom {other:?}"),
                }
                Ok(())
            },
        )
        .expect("writes to in-memory buffer");
        assert_eq!(out, b"x  main");
    }

    #[test]
    fn identity_parts_match_git_identity_layout() {
        let ident = b"Ada Lovelace <ada@example.com> 1717430401 -0530";
        assert_eq!(
            for_each_ref_identity_name(ident),
            Some(&b"Ada Lovelace"[..])
        );
        assert_eq!(
            for_each_ref_identity_email(ident, ForEachRefEmailMode::Bracketed),
            Some(&b"<ada@example.com>"[..])
        );
        assert_eq!(
            for_each_ref_identity_email(ident, ForEachRefEmailMode::Trim),
            Some(&b"ada@example.com"[..])
        );
        assert_eq!(
            for_each_ref_identity_email(ident, ForEachRefEmailMode::LocalPart),
            Some(&b"ada"[..])
        );
        assert_eq!(for_each_ref_identity_timestamp(ident), Some(1717430401));
        assert_eq!(
            for_each_ref_identity_date(ident, &DateMode::Raw).as_deref(),
            Some("1717430401 -0530")
        );
    }

    #[test]
    fn dates_use_identity_timezone() {
        let ident = b"Ada <ada@example.com> 1717430401 -0530";
        assert_eq!(
            for_each_ref_identity_date(ident, &DateMode::Short).as_deref(),
            Some("2024-06-03")
        );
        assert_eq!(
            for_each_ref_identity_date(ident, &DateMode::IsoStrict).as_deref(),
            Some("2024-06-03T10:30:01-05:30")
        );
    }

    #[test]
    fn tracking_formats_match_ref_filter_atoms() {
        assert_eq!(
            for_each_ref_track_short(ForEachRefTrack {
                ahead: 0,
                behind: 0,
                gone: false,
            }),
            "="
        );
        assert_eq!(
            for_each_ref_track_short(ForEachRefTrack {
                ahead: 1,
                behind: 0,
                gone: false,
            }),
            ">"
        );
        assert_eq!(
            for_each_ref_track_short(ForEachRefTrack {
                ahead: 0,
                behind: 1,
                gone: false,
            }),
            "<"
        );
        assert_eq!(
            for_each_ref_track_short(ForEachRefTrack {
                ahead: 1,
                behind: 1,
                gone: false,
            }),
            "<>"
        );

        let mut out = Vec::new();
        write_for_each_ref_track(
            &mut out,
            ForEachRefTrack {
                ahead: 2,
                behind: 3,
                gone: false,
            },
            true,
        )
        .expect("writes to in-memory buffer");
        assert_eq!(out, b"[ahead 2, behind 3]");
    }

    #[test]
    fn refname_shortening_and_stripping_match_ref_filter_rules() {
        assert_eq!(for_each_ref_short_name("refs/heads/main"), "main");
        assert_eq!(for_each_ref_short_name("refs/tags/v1"), "v1");
        assert_eq!(
            for_each_ref_short_name("refs/remotes/origin/HEAD"),
            "origin"
        );
        assert_eq!(for_each_ref_lstrip_name("refs/heads/main", 2), "main");
        assert_eq!(for_each_ref_lstrip_name("refs/heads/main", -1), "main");
        assert_eq!(for_each_ref_rstrip_name("refs/heads/main", 1), "refs/heads");
        assert_eq!(
            for_each_ref_rstrip_name("refs/heads/main", -2),
            "refs/heads"
        );
    }

    #[test]
    fn abbreviations_extend_to_avoid_ambiguity() {
        let one = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("valid object id");
        let two = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111122222222222222222222222222222222222",
        )
        .expect("valid object id");
        assert_eq!(
            parse_for_each_ref_abbrev_width("2").expect("valid abbrev width"),
            4
        );
        assert_eq!(
            for_each_ref_abbrev_oid(&one, Some(4), &[one.clone(), two]),
            "111111"
        );
    }
}
