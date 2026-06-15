//! Unified-diff / patch RENDERER: turn a computed file diff (the old/new
//! blob contents) into the textual unified-diff hunk body git's `diff.c`
//! emit path produces (`emit_diff_symbol` / `fn_out_consume`).
//!
//! This is the byte-for-byte port of git's hunk emitter: `@@ -os,oc +ns,nc @@
//! <heading>` hunk headers, the `+`/`-`/context lines, and the
//! `\ No newline at end of file` marker. It owns hunk *grouping* (combining
//! changes whose context windows overlap, `xdl_get_hunk`'s `distance >
//! max_common` break) and hunk *range* computation, then emits each hunk.
//!
//! What this module deliberately does NOT own (those stay with the caller,
//! which has the repository/userdiff/config context):
//!
//! * **The per-file metainfo header** (`diff --git`, `index`, `---`/`+++`,
//!   mode/similarity lines). That is repository- and option-shaped; the
//!   renderer only produces the hunk body that follows it.
//! * **Funcname section-heading resolution.** The caller supplies a
//!   [`HeadingFn`] closure that, given a candidate line, returns its section
//!   heading (git's `def_ff` default heuristic or a userdiff `xfuncname`
//!   pattern). The renderer does the *scan upward* for the nearest heading
//!   line; the caller only classifies a single line.
//! * **Word-diff body rendering.** When [`HunkRenderOptions::word_diff`] is
//!   set, the renderer delegates each hunk's body to a [`HunkWordDiff`] hook,
//!   which the caller implements over its own word-diff machinery.
//!
//! The seams keep the byte-shaping (ranges, headers, prefixes, no-newline
//! markers, color spans) here — the part every diff-emitting command used to
//! re-derive — while leaving the repository-coupled concerns in the consumer.

use crate::{DiffLine, DiffOp, myers_diff_lines, split_lines};

/// git's default hunk context (`-U3`).
pub const DEFAULT_CONTEXT: usize = 3;

/// The per-line origin marker for an emitted diff line.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LineKind {
    /// An unchanged (` `) line, present on both sides.
    Context,
    /// A removed (`-`) line, present only on the old side.
    Delete,
    /// An added (`+`) line, present only on the new side.
    Insert,
}

/// One line of the unified diff, with its origin and 0-based positions in the
/// old/new files (used to compute hunk ranges and feed the word-diff hook).
#[derive(Clone, Copy)]
pub struct TaggedLine<'a> {
    /// Whether the line is context / a deletion / an insertion.
    pub kind: LineKind,
    /// The raw line bytes, including the trailing `\n` when present.
    pub content: &'a [u8],
    /// 0-based index of this line on the old side.
    pub old_index: usize,
    /// 0-based index of this line on the new side.
    pub new_index: usize,
}

/// ANSI color palette for a unified diff, mirroring git's `diff_get_color`
/// slots. Each field is the raw escape sequence (empty string = no color).
///
/// The renderer only consults the slots it paints in the hunk body; the
/// per-file metainfo slot (`meta`) lives with the caller's header emitter and
/// is intentionally absent here.
#[derive(Clone, Copy)]
pub struct RenderColors<'a> {
    /// `color.diff.frag` — the `@@ .. @@` span.
    pub frag: &'a str,
    /// `color.diff.func` — the section heading after the frag.
    pub func: &'a str,
    /// `color.diff.old` — removed (`-`) lines.
    pub old: &'a str,
    /// `color.diff.new` — added (`+`) lines.
    pub new: &'a str,
    /// `color.diff.context` — context (` `) lines and the no-newline marker.
    pub context: &'a str,
    /// The reset sequence terminating each colored span.
    pub reset: &'a str,
    /// `color.diff.whitespace` — the highlight for whitespace errors
    /// (`--ws-error-highlight`).
    pub whitespace: &'a str,
}

/// Resolve the section heading for one candidate line.
///
/// Returns `Some(heading)` when `line` is a heading line (git's `def_ff`
/// default heuristic or a userdiff `xfuncname` match) and `None` otherwise.
/// The renderer scans upward from each hunk's first line and uses the first
/// `Some` it finds — the caller only has to classify a single line, so it can
/// keep its userdiff-driver / config resolution out of this crate.
pub type HeadingFn<'a> = dyn FnMut(&[u8]) -> Option<Vec<u8>> + 'a;

/// A hook that renders a single hunk's body when `--word-diff` is active.
///
/// The renderer feeds the hunk's tagged lines through this in order
/// (`fn_out_consume`'s `diff_words` branch): each removed line is pushed to
/// the minus buffer, each added line to the plus buffer, and a context line
/// flushes the accumulated word diff before emitting the context line itself.
/// The implementor owns the actual word-level rendering and color spans; this
/// keeps the word-diff machinery in the consumer.
pub trait HunkWordDiff {
    /// Buffer one removed line's content for the next word-diff flush.
    fn push_minus(&mut self, content: &[u8]);
    /// Buffer one added line's content for the next word-diff flush.
    fn push_plus(&mut self, content: &[u8]);
    /// Word-diff the accumulated minus/plus buffers into `out` and reset them.
    fn flush(&mut self, out: &mut Vec<u8>);
    /// Emit one context line (the `--word-diff` context style).
    fn emit_context_line(&mut self, out: &mut Vec<u8>, content: &[u8]);
}

/// Hunk-shaping and styling options for [`render_hunks`].
///
/// Lifetimes are split so the funcname / word-diff hooks can be borrowed
/// mutably while `colors` is borrowed shared.
pub struct HunkRenderOptions<'a, 'h> {
    /// Lines of context around each change (`-U<n>`, default
    /// [`DEFAULT_CONTEXT`]).
    pub context: usize,
    /// Extra inter-hunk merging distance (`--inter-hunk-context`).
    pub interhunk: usize,
    /// Per-line section-heading classifier; `None` emits headerless hunks.
    pub heading: Option<&'a mut HeadingFn<'h>>,
    /// ANSI palette when color output is enabled.
    pub colors: Option<RenderColors<'a>>,
    /// Word-diff body hook (replaces the `+`/`-` line bodies of each hunk).
    pub word_diff: Option<&'a mut dyn HunkWordDiff>,
    /// `--ws-error-highlight` configuration: when set and colors are on, the
    /// renderer paints whitespace errors on the selected line kinds with
    /// `colors.whitespace` (git's `emit_line_ws_markup`). `None` disables it.
    pub ws_error: Option<WsErrorHighlight>,
}

/// Which line kinds get whitespace-error highlighting, plus the rule to check
/// against. git's `--ws-error-highlight` defaults to highlighting only new
/// (`+`) lines.
#[derive(Clone, Copy)]
pub struct WsErrorHighlight {
    /// The resolved whitespace rule to check each line against.
    pub rule: crate::ws::WsRule,
    /// Highlight errors on removed (`-`) lines.
    pub old: bool,
    /// Highlight errors on added (`+`) lines.
    pub new: bool,
    /// Highlight errors on context (` `) lines.
    pub context: bool,
}

impl Default for HunkRenderOptions<'_, '_> {
    fn default() -> Self {
        Self {
            context: DEFAULT_CONTEXT,
            interhunk: 0,
            heading: None,
            colors: None,
            word_diff: None,
            ws_error: None,
        }
    }
}

/// Render the unified-diff hunk body for a single file change into `out`.
///
/// `old_content` / `new_content` are the full blob contents (`None` for an
/// absent side — a created or deleted file). The function computes the
/// line-level Myers diff, groups changes into hunks with `options.context`
/// lines of surrounding context (merging nearby groups per
/// `options.interhunk`), and emits each hunk: the `@@` header (with git's
/// section heading), then the context / `-` / `+` lines including
/// `\ No newline at end of file` markers.
///
/// Nothing is written when the contents are identical (no changed lines).
/// This is the body *after* the per-file metainfo header the caller emits.
pub fn render_hunks(
    out: &mut Vec<u8>,
    old_content: Option<&[u8]>,
    new_content: Option<&[u8]>,
    options: &mut HunkRenderOptions<'_, '_>,
) {
    let old = split_lines(old_content.unwrap_or_default());
    let new = split_lines(new_content.unwrap_or_default());
    let ops = myers_diff_lines(&old, &new);

    // Flatten the edit script into a tagged line stream carrying old/new
    // positions.
    let mut tagged: Vec<TaggedLine<'_>> = Vec::new();
    let mut old_idx = 0usize;
    let mut new_idx = 0usize;
    for op in ops {
        match op {
            DiffOp::Equal(n) => {
                for _ in 0..n {
                    tagged.push(TaggedLine {
                        kind: LineKind::Context,
                        content: old[old_idx].content,
                        old_index: old_idx,
                        new_index: new_idx,
                    });
                    old_idx += 1;
                    new_idx += 1;
                }
            }
            DiffOp::Delete(n) => {
                for _ in 0..n {
                    tagged.push(TaggedLine {
                        kind: LineKind::Delete,
                        content: old[old_idx].content,
                        old_index: old_idx,
                        new_index: new_idx,
                    });
                    old_idx += 1;
                }
            }
            DiffOp::Insert(n) => {
                for _ in 0..n {
                    tagged.push(TaggedLine {
                        kind: LineKind::Insert,
                        content: new[new_idx].content,
                        old_index: old_idx,
                        new_index: new_idx,
                    });
                    new_idx += 1;
                }
            }
        }
    }

    // Indices of changed (non-context) lines.
    let change_positions: Vec<usize> = tagged
        .iter()
        .enumerate()
        .filter(|(_, line)| line.kind != LineKind::Context)
        .map(|(idx, _)| idx)
        .collect();
    if change_positions.is_empty() {
        return;
    }

    // Group changes whose context windows overlap into single hunks.
    let mut groups: Vec<(usize, usize)> = Vec::new();
    let mut group_start = change_positions[0];
    let mut group_end = change_positions[0];
    for &pos in &change_positions[1..] {
        // Two change runs merge when at most 2*context (+ interhunk) equal
        // lines separate them, mirroring xdl_get_hunk's `distance >
        // max_common` break (the position gap counts the separating equal
        // lines plus one, so adjacent delete/insert runs always merge).
        if pos - group_end <= 2 * options.context + options.interhunk + 1 {
            group_end = pos;
        } else {
            groups.push((group_start, group_end));
            group_start = pos;
            group_end = pos;
        }
    }
    groups.push((group_start, group_end));

    for (first_change, last_change) in groups {
        let hunk_start = first_change.saturating_sub(options.context);
        let hunk_end = (last_change + options.context + 1).min(tagged.len());
        render_one_hunk(out, &tagged, &old, hunk_start, hunk_end, options);
    }
}

/// Emit a single hunk covering `tagged[start..end]`: the `@@ -os,oc +ns,nc @@
/// <heading>` header followed by the context/`-`/`+` lines, including the
/// `\ No newline at end of file` markers.
fn render_one_hunk(
    out: &mut Vec<u8>,
    tagged: &[TaggedLine<'_>],
    old_lines: &[DiffLine<'_>],
    start: usize,
    end: usize,
    options: &mut HunkRenderOptions<'_, '_>,
) {
    let slice = &tagged[start..end];
    let mut old_count = 0usize;
    let mut new_count = 0usize;
    for line in slice {
        match line.kind {
            LineKind::Context => {
                old_count += 1;
                new_count += 1;
            }
            LineKind::Delete => old_count += 1,
            LineKind::Insert => new_count += 1,
        }
    }
    // 1-based starting line numbers; an empty side starts at 0.
    let old_start = if old_count == 0 {
        slice.first().map(|line| line.old_index).unwrap_or(0)
    } else {
        slice
            .iter()
            .find(|line| line.kind != LineKind::Insert)
            .map(|line| line.old_index + 1)
            .unwrap_or(1)
    };
    let new_start = if new_count == 0 {
        slice.first().map(|line| line.new_index).unwrap_or(0)
    } else {
        slice
            .iter()
            .find(|line| line.kind != LineKind::Delete)
            .map(|line| line.new_index + 1)
            .unwrap_or(1)
    };

    let heading = hunk_section_heading(
        old_lines,
        slice.first().map(|line| line.old_index),
        options.heading.as_deref_mut(),
    );
    let frag = format!(
        "@@ -{} +{} @@",
        format_hunk_range(old_start, old_count),
        format_hunk_range(new_start, new_count)
    );
    match options.colors {
        // Port of emit_hunk_header: the "@@ .. @@" span in the frag color,
        // the separating blank in the context color, the heading in the func
        // color (each reset-terminated).
        Some(colors) => {
            out.extend_from_slice(colors.frag.as_bytes());
            out.extend_from_slice(frag.as_bytes());
            out.extend_from_slice(colors.reset.as_bytes());
            if let Some(heading) = &heading {
                out.extend_from_slice(colors.context.as_bytes());
                out.push(b' ');
                out.extend_from_slice(colors.reset.as_bytes());
                out.extend_from_slice(colors.func.as_bytes());
                out.extend_from_slice(heading);
                out.extend_from_slice(colors.reset.as_bytes());
            }
            out.push(b'\n');
        }
        None => {
            out.extend_from_slice(frag.as_bytes());
            if let Some(heading) = &heading {
                out.push(b' ');
                out.extend_from_slice(heading);
            }
            out.push(b'\n');
        }
    }

    if let Some(word_diff) = options.word_diff.as_deref_mut() {
        // Word-diff rendering: minus/plus runs accumulate and flush at
        // context lines (fn_out_consume's diff_words branch); the
        // "\ No newline" markers are eaten.
        for line in slice {
            match line.kind {
                LineKind::Delete => word_diff.push_minus(line.content),
                LineKind::Insert => word_diff.push_plus(line.content),
                LineKind::Context => {
                    word_diff.flush(out);
                    word_diff.emit_context_line(out, line.content);
                }
            }
        }
        word_diff.flush(out);
        return;
    }

    for line in slice {
        let prefix = match line.kind {
            LineKind::Context => b' ',
            LineKind::Delete => b'-',
            LineKind::Insert => b'+',
        };
        match options.colors {
            Some(colors) => {
                // Whitespace-error highlighting applies to the selected line
                // kinds (default: new lines only).
                let ws_rule = options.ws_error.and_then(|ws| {
                    let enabled = match line.kind {
                        LineKind::Context => ws.context,
                        LineKind::Delete => ws.old,
                        LineKind::Insert => ws.new,
                    };
                    enabled.then_some(ws.rule)
                });
                write_patch_line_colored(out, prefix, line.content, colors, ws_rule);
            }
            None => write_patch_line(out, prefix, line.content),
        }
    }
}

/// Format one `start,count` side of an `@@` header. git omits the count when
/// it is exactly 1 (e.g. `+5` rather than `+5,1`).
fn format_hunk_range(start: usize, count: usize) -> String {
    if count == 1 {
        start.to_string()
    } else {
        format!("{start},{count}")
    }
}

/// git's section heading for a hunk: the nearest line *before* the hunk's
/// first line accepted by the caller's `heading` classifier. Headings are
/// produced by the classifier (already capped/trimmed by the caller's
/// userdiff machinery). Returns `None` when no such line precedes the hunk or
/// no classifier was supplied.
fn hunk_section_heading(
    old_lines: &[DiffLine<'_>],
    first_old_index: Option<usize>,
    mut heading: Option<&mut HeadingFn<'_>>,
) -> Option<Vec<u8>> {
    let first = first_old_index?;
    let classifier = heading.as_mut()?;
    // Scan upward from the line just above the hunk.
    for idx in (0..first).rev() {
        if let Some(found) = classifier(old_lines[idx].content) {
            return Some(found);
        }
    }
    None
}

/// Write a single diff line with its `prefix` marker, appending the
/// `\ No newline at end of file` note when the source line lacks a trailing
/// LF.
fn write_patch_line(out: &mut Vec<u8>, prefix: u8, line: &[u8]) {
    out.push(prefix);
    out.extend_from_slice(line);
    if !line.ends_with(b"\n") {
        out.extend_from_slice(b"\n\\ No newline at end of file\n");
    }
}

/// [`write_patch_line`] in color, optionally painting whitespace errors.
///
/// When `ws_rule` is `Some`, the line body is emitted through
/// [`crate::ws::ws_check_emit`] (git's `emit_line_ws_markup` highlighted
/// branch): the sign is painted in the line color, then the body's non-error
/// segments in the line color and its whitespace-error segments in
/// `colors.whitespace`. A clean line produces no whitespace spans, so it stays
/// visually plain.
///
/// When `ws_rule` is `None`, context/old lines paint the sign and body in one
/// span; new lines paint the sign and body as separate spans (the default
/// `ws-error-highlight` path with no rule).
fn write_patch_line_colored(
    out: &mut Vec<u8>,
    prefix: u8,
    line: &[u8],
    colors: RenderColors<'_>,
    ws_rule: Option<crate::ws::WsRule>,
) {
    let (body, terminated) = match line.split_last() {
        Some((b'\n', body)) => (body, true),
        _ => (line, false),
    };
    let color = match prefix {
        b'-' => colors.old,
        b'+' => colors.new,
        _ => colors.context,
    };

    if let Some(rule) = ws_rule {
        // Sign in the line color, then the body through ws_check_emit (no
        // trailing newline in `body`, so the emit's own LF handling is inert).
        out.extend_from_slice(color.as_bytes());
        out.push(prefix);
        out.extend_from_slice(colors.reset.as_bytes());
        let emit_colors = crate::ws::WsEmitColors {
            set: color,
            reset: colors.reset,
            ws: colors.whitespace,
        };
        crate::ws::ws_check_emit(body, rule, out, &emit_colors);
        out.push(b'\n');
        if !terminated {
            out.extend_from_slice(colors.context.as_bytes());
            out.extend_from_slice(b"\\ No newline at end of file");
            out.extend_from_slice(colors.reset.as_bytes());
            out.push(b'\n');
        }
        return;
    }

    if prefix == b'+' {
        out.extend_from_slice(color.as_bytes());
        out.push(prefix);
        out.extend_from_slice(colors.reset.as_bytes());
        if !body.is_empty() {
            out.extend_from_slice(color.as_bytes());
            out.extend_from_slice(body);
            out.extend_from_slice(colors.reset.as_bytes());
        }
    } else {
        out.extend_from_slice(color.as_bytes());
        out.push(prefix);
        out.extend_from_slice(body);
        out.extend_from_slice(colors.reset.as_bytes());
    }
    out.push(b'\n');
    if !terminated {
        out.extend_from_slice(colors.context.as_bytes());
        out.extend_from_slice(b"\\ No newline at end of file");
        out.extend_from_slice(colors.reset.as_bytes());
        out.push(b'\n');
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render_plain(old: Option<&[u8]>, new: Option<&[u8]>) -> Vec<u8> {
        let mut out = Vec::new();
        let mut options = HunkRenderOptions::default();
        render_hunks(&mut out, old, new, &mut options);
        out
    }

    #[test]
    fn identical_content_renders_nothing() {
        assert!(render_plain(Some(b"a\nb\n"), Some(b"a\nb\n")).is_empty());
    }

    #[test]
    fn single_line_change_basic_hunk() {
        let out = render_plain(Some(b"alpha\nbeta\ngamma\n"), Some(b"alpha\nBETA\ngamma\n"));
        assert_eq!(
            out,
            b"@@ -1,3 +1,3 @@\n alpha\n-beta\n+BETA\n gamma\n".to_vec(),
        );
    }

    #[test]
    fn count_omitted_when_one() {
        // A single-line file changed in place yields `-1 +1` (no `,1`).
        let out = render_plain(Some(b"old\n"), Some(b"new\n"));
        assert_eq!(out, b"@@ -1 +1 @@\n-old\n+new\n".to_vec());
    }

    #[test]
    fn no_newline_marker_on_old_side() {
        let out = render_plain(Some(b"only line no newline"), None);
        assert_eq!(
            out,
            b"@@ -1 +0,0 @@\n-only line no newline\n\\ No newline at end of file\n".to_vec(),
        );
    }

    #[test]
    fn no_newline_marker_on_new_side() {
        let out = render_plain(Some(b"beta\n"), Some(b"beta-notail"));
        assert_eq!(
            out,
            b"@@ -1 +1 @@\n-beta\n+beta-notail\n\\ No newline at end of file\n".to_vec(),
        );
    }

    #[test]
    fn pure_insertion_into_empty() {
        let out = render_plain(None, Some(b"x\ny\n"));
        assert_eq!(out, b"@@ -0,0 +1,2 @@\n+x\n+y\n".to_vec());
    }

    #[test]
    fn distant_changes_split_into_two_hunks() {
        let old: &[u8] = b"a\nb\nc\nd\ne\nf\ng\nh\ni\nj\n";
        let new: &[u8] = b"A\nb\nc\nd\ne\nf\ng\nh\ni\nJ\n";
        let out = render_plain(Some(old), Some(new));
        // Two changes 9 lines apart (> 2*3+1) produce two separate hunks.
        let text = String::from_utf8(out).expect("rendered output is valid UTF-8");
        assert_eq!(text.matches("@@ ").count(), 2, "expected two hunks: {text}");
    }

    #[test]
    fn heading_callback_supplies_section() {
        // The change is far enough below `fn foo()` that the funcname line
        // precedes the hunk (the heading scan looks *above* the hunk's first
        // line, so a change touching line 1 would correctly find no heading).
        let old: &[u8] =
            b"fn foo() {\n    a\n    b\n    c\n    d\n    e\n    f\n    g\n}\n";
        let new: &[u8] =
            b"fn foo() {\n    a\n    b\n    c\n    d\n    CHANGED\n    f\n    g\n}\n";
        let mut out = Vec::new();
        // Classifier accepts any line whose first byte is an ASCII letter
        // (a crude def_ff stand-in for the test).
        let mut heading_fn = |line: &[u8]| -> Option<Vec<u8>> {
            if line.first().is_some_and(u8::is_ascii_alphabetic) {
                Some(line.strip_suffix(b"\n").unwrap_or(line).to_vec())
            } else {
                None
            }
        };
        let mut options = HunkRenderOptions {
            heading: Some(&mut heading_fn),
            ..Default::default()
        };
        render_hunks(&mut out, Some(old), Some(new), &mut options);
        let text = String::from_utf8(out).expect("rendered output is valid UTF-8");
        assert!(
            text.starts_with("@@ -3,7 +3,7 @@ fn foo() {\n"),
            "expected funcname heading: {text}",
        );
    }
}
