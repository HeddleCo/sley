//! Shared ref-filter formatting primitives.
//!
//! Git reuses the same identity/date/refname formatting language across
//! `for-each-ref`, `branch`, `tag`, `log`, `show`, `stash`, and status output.
//! This crate owns those semantic primitives so the CLI can remain an entry
//! point instead of a home for every command's formatting state.

use sley_core::{DateMode, GitError, ObjectId, Result};
use sley_strbuf_expand::{
    AtomTable, ExpandFormat, ExpandSegment, PaddingAlign, PaddingSpec, apply_magic,
};
use std::collections::HashMap;
use std::io::Write;

mod atoms;
mod contents;
mod context;
mod render;
mod repo;
mod sort;
mod tracking;
mod versioncmp;

pub use atoms::{
    ForEachRefEmailOptions, for_each_ref_color_escape, for_each_ref_message,
    for_each_ref_oid_atom_arg, for_each_ref_oid_atom_width, for_each_ref_push_color_code,
    for_each_ref_try_date_atom, for_each_ref_try_email_atom, for_each_ref_try_name_atom,
    for_each_ref_typed_identity, for_each_ref_typed_refname, for_each_ref_write_email,
    setup_for_each_ref_email_options, write_for_each_ref_signature, write_for_each_ref_typed_atom,
};
pub use contents::{
    ForEachRefContents, ForEachRefPeeledObject, for_each_ref_contents,
    for_each_ref_validate_tag_pointer, write_for_each_ref_contents_lines,
};
pub use context::{
    ForEachRefFormatContext, ForEachRefMailmapRewrite, ForEachRefSignatureVerification,
};
pub use render::{
    ForEachRefDescribeRenderer, ForEachRefRenderHooks, ForEachRefTrailersFormatter,
    print_for_each_ref_format, print_for_each_ref_format_with_is_bases,
};
pub use repo::{
    for_each_ref_loose_object_disk_size, for_each_ref_worktree_path, for_each_ref_worktree_paths,
};
pub use sort::{
    ForEachRefDateSortField, ForEachRefIdentityPart, ForEachRefIdentityRole,
    ForEachRefIdentitySortField, ForEachRefIdentitySource, for_each_ref_sort_date_key,
    for_each_ref_sort_identity_key, parse_for_each_ref_identity_sort,
};
pub use tracking::{
    ForEachRefPush, ForEachRefPushRemote, ForEachRefUpstream, expand_local_upstream_merge,
    for_each_ref_ahead_behind, for_each_ref_ahead_behind_with_diagnostic, for_each_ref_push,
    for_each_ref_push_remote, for_each_ref_upstream, for_each_ref_upstream_track,
    map_remote_fetch_refspec, map_remote_push_refspec, map_remote_tracking_ref,
    remote_display_name, resolve_for_each_ref_target,
};
pub use versioncmp::{
    VsSuffixMatch, version_sort_cmp, vs_digit_class, vs_find_better_matching_suffix,
    vs_swap_prereleases,
};

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

/// A date atom used as a `for-each-ref --sort` key.
///
/// Bare date atoms are sorted numerically by their timestamp. Once a date
/// format is supplied, Git sorts the rendered value bytewise instead. Keeping
/// the parsed mode here lets command frontends share that distinction without
/// reimplementing the ref-filter date grammar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForEachRefDateSort {
    pub peeled: bool,
    pub role: ForEachRefAtomIdentityRole,
    pub mode: DateMode,
    pub descending: bool,
}

/// Parse a date sort atom, returning `None` when `value` names another atom.
pub fn parse_for_each_ref_date_sort(value: &str) -> Result<Option<ForEachRefDateSort>> {
    let (value, descending) = value
        .strip_prefix('-')
        .map(|value| (value, true))
        .unwrap_or((value, false));
    let (value, peeled) = value
        .strip_prefix('*')
        .map(|value| (value, true))
        .unwrap_or((value, false));
    let (atom, modifier) = value
        .split_once(':')
        .map(|(atom, modifier)| (atom, Some(modifier)))
        .unwrap_or((value, None));
    let role = match atom {
        "authordate" => ForEachRefAtomIdentityRole::Author,
        "committerdate" => ForEachRefAtomIdentityRole::Committer,
        "taggerdate" => ForEachRefAtomIdentityRole::Tagger,
        "creatordate" => ForEachRefAtomIdentityRole::Creator,
        _ => return Ok(None),
    };
    let mode = DateMode::parse_atom_modifier(modifier).ok_or_else(|| {
        GitError::Command(format!(
            "unrecognized %({atom}) argument: {}",
            modifier.unwrap_or("")
        ))
    })?;
    Ok(Some(ForEachRefDateSort {
        peeled,
        role,
        mode,
        descending,
    }))
}

/// Select the ref that Git's `%(is-base:<tip>)` heuristic marks.
///
/// Histories are ordered from each commit towards its first parent. The best
/// candidate is the one whose first-parent history intersects the tip history
/// closest to the tip; candidate order breaks ties, matching ref-array order.
pub fn select_for_each_ref_is_base_candidate(
    tip_first_parent_history: &[ObjectId],
    candidate_first_parent_histories: &[Vec<ObjectId>],
) -> Option<usize> {
    let tip_positions = tip_first_parent_history
        .iter()
        .enumerate()
        .map(|(position, oid)| (*oid, position))
        .collect::<HashMap<_, _>>();

    candidate_first_parent_histories
        .iter()
        .enumerate()
        .filter_map(|(candidate, history)| {
            history
                .iter()
                .filter_map(|oid| tip_positions.get(oid).copied())
                .min()
                .map(|tip_distance| (tip_distance, candidate))
        })
        .min()
        .map(|(_, candidate)| candidate)
}

/// Whether `name` is one of Git's enumerable root refs.
///
/// Root-ref syntax alone is broader than the ref-filter surface: `FETCH_HEAD`
/// and `MERGE_HEAD` are pseudorefs and are deliberately excluded, while HEAD,
/// `*_HEAD`, and Git's named root refs are included when they resolve.
pub fn is_for_each_ref_root_ref(name: &str) -> bool {
    let root_syntax = !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte == b'-' || byte == b'_');
    if !root_syntax || matches!(name, "FETCH_HEAD" | "MERGE_HEAD") {
        return false;
    }
    name.ends_with("_HEAD")
        || matches!(
            name,
            "HEAD"
                | "AUTO_MERGE"
                | "BISECT_EXPECTED_REV"
                | "NOTES_MERGE_PARTIAL"
                | "NOTES_MERGE_REF"
                | "MERGE_AUTOSTASH"
        )
}

/// Parse Git's `#rrggbb` color spelling used by `%(color:<value>)` atoms.
pub fn parse_for_each_ref_hex_color(value: &str) -> Option<(u8, u8, u8)> {
    let hex = value.strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    Some((
        u8::from_str_radix(&hex[0..2], 16).ok()?,
        u8::from_str_radix(&hex[2..4], 16).ok()?,
        u8::from_str_radix(&hex[4..6], 16).ok()?,
    ))
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
                            apply_magic(out, atom.magic, &value);
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
                            apply_magic(out, atom.magic, &value);
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
                apply_magic(out, atom.magic, &value);
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
        if let Some(len) = csi_escape_sequence_len(value, idx) {
            idx += len;
            continue;
        }
        // Measure the text run up to the next escape by display columns, not
        // bytes, so multibyte characters (CJK, accents, emoji) pad like git.
        let mut run = idx + 1;
        while run < value.len() && csi_escape_sequence_len(value, run).is_none() {
            run += 1;
        }
        width += sley_strbuf_expand::strwidth(&value[idx..run]);
        idx = run;
    }
    width
}

/// Length of the CSI escape sequence starting at `idx`, if any
/// (`ESC [ ... final-byte`, final byte in `0x40..=0x7e`).
fn csi_escape_sequence_len(value: &[u8], idx: usize) -> Option<usize> {
    if value[idx] != 0x1b || value.get(idx + 1) != Some(&b'[') {
        return None;
    }
    value[idx + 2..]
        .iter()
        .position(|byte| (0x40..=0x7e).contains(byte))
        .map(|end| end + 3)
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
    // Locate the timestamp+timezone tail git's way (scanning back from the end
    // for the last '>'), then return the contiguous `<digits> <tz>` slice.
    let fields = sley_core::split_ident_line(identity)?;
    let date = fields.date?;
    let tz = fields.tz?;
    let base = identity.as_ptr() as usize;
    let start = date.as_ptr() as usize - base;
    let end = (tz.as_ptr() as usize - base) + tz.len();
    Some(&identity[start..end])
}

pub fn for_each_ref_identity_date(identity: &[u8], mode: &DateMode) -> Option<String> {
    // git's show_ident_date semantics: an out-of-range timestamp renders the
    // epoch sentinel rather than dropping the field; a missing date renders
    // nothing (None).
    let fields = sley_core::split_ident_line(identity)?;
    let date = fields.date?;
    let tz = fields.tz.unwrap_or(b"+0000");
    Some(sley_core::ident_render_date(date, tz, mode))
}

pub fn for_each_ref_identity_timestamp(identity: &[u8]) -> Option<i64> {
    let fields = sley_core::split_ident_line(identity)?;
    let date = fields.date?;
    std::str::from_utf8(date).ok()?.parse::<i64>().ok()
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
/// spaces (CRLF's CR is dropped), matching ref-filter.c. Multibyte UTF-8
/// content passes through byte-exactly; invalid UTF-8 degrades lossily.
pub fn for_each_ref_copy_subject(subject: &[u8]) -> String {
    let decoded = String::from_utf8_lossy(subject);
    let mut out = String::with_capacity(decoded.len());
    let mut chars = decoded.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\r' && chars.peek() == Some(&'\n') {
            continue;
        }
        out.push(if ch == '\n' { ' ' } else { ch });
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

/// Render an ident's date for the structured header lines (`Date:`/`AuthorDate:`/
/// `CommitDate:`), mirroring pretty.c's `pp_user_info`, which calls
/// `show_ident_date` directly: a missing or unparsable date still prints the
/// epoch sentinel (`Thu Jan 1 00:00:00 1970 +0000`) rather than an empty string.
/// Use this for the medium/full/fuller layouts; use [`commit_identity_date`] for
/// the `%ad`/`%cd` placeholders, which suppress a missing date entirely.
pub fn commit_identity_date_or_sentinel(raw: &[u8], mode: &DateMode) -> String {
    match sley_core::split_ident_line(raw) {
        Some(fields) => {
            let date = fields.date.unwrap_or(b"0");
            let tz = fields.tz.unwrap_or(b"+0000");
            sley_core::ident_render_date(date, tz, mode)
        }
        // No `<…>` pair at all: pp_user_info would skip the whole block, so the
        // caller shouldn't reach here for a well-formed commit; fall back to the
        // epoch sentinel to stay non-panicking.
        None => sley_core::ident_render_date(b"0", b"+0000", mode),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sley_core::ObjectFormat;

    #[test]
    fn ref_filter_root_refs_exclude_pseudorefs() {
        for name in ["HEAD", "ORIG_HEAD", "AUTO_MERGE", "BISECT_EXPECTED_REV"] {
            assert!(is_for_each_ref_root_ref(name), "{name}");
        }
        for name in ["FETCH_HEAD", "MERGE_HEAD", "DANGLING", "refs/heads/main"] {
            assert!(!is_for_each_ref_root_ref(name), "{name}");
        }
    }

    #[test]
    fn ref_filter_hex_color_requires_six_hex_digits() {
        assert_eq!(
            parse_for_each_ref_hex_color("#aa22ac"),
            Some((0xaa, 0x22, 0xac))
        );
        assert_eq!(
            parse_for_each_ref_hex_color("#AA22AC"),
            Some((0xaa, 0x22, 0xac))
        );
        assert_eq!(parse_for_each_ref_hex_color("#abc"), None);
        assert_eq!(parse_for_each_ref_hex_color("#gg22ac"), None);
    }

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
    fn align_and_padding_measure_multibyte_subjects_by_display_columns() {
        use sley_strbuf_expand::TruncateMode;

        // copy_subject must pass multibyte content through byte-exactly
        // (regression: bytes were re-encoded one Latin-1 char at a time).
        assert_eq!(
            for_each_ref_copy_subject("日本語テスト".as_bytes()),
            "日本語テスト"
        );
        assert_eq!(
            for_each_ref_copy_subject("héllo\r\nwörld\né".as_bytes()),
            "héllo wörld é"
        );

        // "日本語テスト" renders in 12 terminal columns but occupies 18 bytes;
        // git's strbuf_utf8_align pads to the column width (oracle:
        // `git for-each-ref --format='[%(align:20,left)%(subject)%(end)]'`
        // on a commit with this subject emits 18 bytes + 8 spaces).
        let cjk = "日本語テスト".as_bytes();
        assert_eq!(cjk.len(), 18);
        assert_eq!(sley_strbuf_expand::strwidth(cjk), 12);

        assert_eq!(for_each_ref_display_width(cjk), 12);
        assert_eq!(for_each_ref_display_width("héllo".as_bytes()), 5);
        assert_eq!(for_each_ref_display_width(b"\x1b[31mabc\x1b[m"), 3);
        assert_eq!(for_each_ref_display_width(b"\x1b[31m\xe6\x97\xa5\x1b[m"), 2);

        let mut aligned = cjk.to_vec();
        apply_for_each_ref_align(
            &mut aligned,
            &ForEachRefAlignOptions {
                width: 20,
                position: ForEachRefAlignPosition::Left,
            },
        );
        assert_eq!(
            String::from_utf8(aligned).unwrap_or_default(),
            format!("日本語テスト{}", " ".repeat(8))
        );

        let mut middle = cjk.to_vec();
        apply_for_each_ref_align(
            &mut middle,
            &ForEachRefAlignOptions {
                width: 20,
                position: ForEachRefAlignPosition::Middle,
            },
        );
        assert_eq!(
            String::from_utf8(middle).unwrap_or_default(),
            format!("{}日本語テスト{}", " ".repeat(4), " ".repeat(4))
        );

        let mut padded = cjk.to_vec();
        apply_for_each_ref_padding(
            &mut padded,
            Some(PaddingSpec {
                width: 20,
                align: PaddingAlign::Right,
                truncate: TruncateMode::None,
                to_column: false,
            }),
        );
        assert_eq!(
            String::from_utf8(padded).unwrap_or_default(),
            format!("{}日本語テスト", " ".repeat(8))
        );
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
    fn date_sort_parser_preserves_custom_format_semantics() {
        let sort = parse_for_each_ref_date_sort("-*creatordate:format:%H:%M:%S")
            .expect("valid date sort")
            .expect("recognized date atom");
        assert!(sort.peeled);
        assert!(sort.descending);
        assert_eq!(sort.role, ForEachRefAtomIdentityRole::Creator);
        assert_eq!(
            sort.mode,
            DateMode::Strftime {
                template: "%H:%M:%S".to_string(),
                local: false,
            }
        );
        // Bare `creatordate:format:...` (no peel/desc flags) is the atom used by
        // t6300/t1461 "sort by custom date format".
        let plain = parse_for_each_ref_date_sort("creatordate:format:%H:%M:%S")
            .expect("valid date sort")
            .expect("recognized date atom");
        assert!(!plain.peeled);
        assert!(!plain.descending);
        assert_eq!(plain.role, ForEachRefAtomIdentityRole::Creator);
        assert_eq!(
            plain.mode,
            DateMode::Strftime {
                template: "%H:%M:%S".to_string(),
                local: false,
            }
        );
        assert!(
            parse_for_each_ref_date_sort("refname")
                .expect("non-date sort is not an error")
                .is_none()
        );
    }

    /// Git sorts bare date atoms by raw timestamp, but once a `format:` (or
    /// other) modifier is present the *rendered* string is compared bytewise.
    /// The t6300 fixture dates reverse order under those two keys; pin that.
    #[test]
    fn custom_date_format_sort_keys_differ_from_raw_timestamps() {
        // Same instants as t/for-each-ref-tests.sh "set up custom date sorting".
        let idents = [
            b"user <user@example.com> 1707341660 +0000".as_slice(), // 21:34:20
            b"user <user@example.com> 945129922 +0000".as_slice(),  // 00:05:22
            b"user <user@example.com> 1622806011 +0000".as_slice(), // 11:26:51
            b"user <user@example.com> 1169484241 +0000".as_slice(), // 16:44:01
        ];
        let mode = DateMode::Strftime {
            template: "%H:%M:%S".to_string(),
            local: false,
        };
        let mut by_format: Vec<_> = idents
            .iter()
            .map(|ident| for_each_ref_identity_date(ident, &mode).expect("date"))
            .collect();
        let mut by_unix: Vec<_> = idents
            .iter()
            .map(|ident| for_each_ref_identity_timestamp(ident).expect("ts"))
            .collect();
        by_format.sort();
        by_unix.sort();
        assert_eq!(
            by_format,
            vec![
                "00:05:22".to_string(),
                "11:26:51".to_string(),
                "16:44:01".to_string(),
                "21:34:20".to_string(),
            ]
        );
        assert_eq!(by_unix, vec![945129922, 1169484241, 1622806011, 1707341660]);
        // Timestamp order of the *labels* is not the same as time-of-day order.
        let labels_by_unix: Vec<_> = by_unix
            .iter()
            .map(|ts| {
                let ident = format!("user <user@example.com> {ts} +0000");
                for_each_ref_identity_date(ident.as_bytes(), &mode).expect("date")
            })
            .collect();
        assert_ne!(
            labels_by_unix, by_format,
            "format:%H:%M:%S order must not collapse to creatordate order"
        );
    }

    #[test]
    fn is_base_selection_minimizes_tip_first_parent_distance_and_keeps_ref_order() {
        let oid =
            |hex: &str| ObjectId::from_hex(ObjectFormat::Sha1, hex).expect("valid test object id");
        let root = oid("0000000000000000000000000000000000000001");
        let near = oid("0000000000000000000000000000000000000002");
        let tip = oid("0000000000000000000000000000000000000003");
        let left = oid("0000000000000000000000000000000000000004");
        let right = oid("0000000000000000000000000000000000000005");
        let histories = vec![vec![left, near, root], vec![right, near, root], vec![root]];
        assert_eq!(
            select_for_each_ref_is_base_candidate(&[tip, near, root], &histories),
            Some(0),
            "nearest intersection wins and the first candidate breaks a tie"
        );
        assert_eq!(
            select_for_each_ref_is_base_candidate(&[tip], &histories),
            None
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
