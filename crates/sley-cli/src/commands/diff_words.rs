//! Word-diff rendering (`--word-diff=plain|porcelain|color`, `--color-words`)
//! and the diff color palette, ported from upstream `diff.c`
//! (`diff_words_styles`, `fn_out_diff_words_aux`, `find_word_boundaries`,
//! `diff_words_fill`, `emit_hunk_header`).

use crate::*;
use commands::grep::Regex;

/// ANSI palette for colored diff output. Each slot holds the escape sequence
/// (empty when color is disabled), mirroring `diff_get_color`.
#[derive(Clone, Default)]
pub(crate) struct DiffColors {
    pub(crate) meta: String,
    pub(crate) frag: String,
    pub(crate) func: String,
    pub(crate) old: String,
    pub(crate) new: String,
    pub(crate) context: String,
    pub(crate) reset: String,
}

impl DiffColors {
    /// The default enabled palette: meta=bold, frag=cyan, old=red, new=green,
    /// func/context unset, overridden by `color.diff.<slot>` with the legacy
    /// `diff.color.<slot>` spelling as a fallback.
    pub(crate) fn enabled(config: Option<&GitConfig>) -> Self {
        let lookup = |slot: &str, default: &str| -> String {
            let value = config.and_then(|config| {
                config
                    .get("color", Some("diff"), slot)
                    .or_else(|| config.get("diff", Some("color"), slot))
            });
            match value {
                Some(name) => parse_color_value(name).unwrap_or_else(|| default.to_string()),
                None => default.to_string(),
            }
        };
        Self {
            meta: lookup("meta", "\x1b[1m"),
            frag: lookup("frag", "\x1b[36m"),
            func: lookup("func", ""),
            old: lookup("old", "\x1b[31m"),
            new: lookup("new", "\x1b[32m"),
            context: lookup("context", ""),
            reset: "\x1b[m".to_string(),
        }
    }
}

/// Parse a git color word ("red", "bold", "green dim", ...) into an ANSI
/// sequence. Only the simple forms the diff palette uses are supported;
/// unknown words yield `None` (caller keeps the default).
fn parse_color_value(value: &str) -> Option<String> {
    let mut fg: Option<u8> = None;
    let mut bg: Option<u8> = None;
    let mut attrs: Vec<u8> = Vec::new();
    for word in value.split_ascii_whitespace() {
        let code = |name: &str| -> Option<u8> {
            Some(match name {
                "black" => 0,
                "red" => 1,
                "green" => 2,
                "yellow" => 3,
                "blue" => 4,
                "magenta" => 5,
                "cyan" => 6,
                "white" => 7,
                _ => return None,
            })
        };
        match word {
            "bold" => attrs.push(1),
            "dim" => attrs.push(2),
            "ul" => attrs.push(4),
            "blink" => attrs.push(5),
            "reverse" => attrs.push(7),
            "normal" => {}
            "reset" => return Some("\x1b[m".to_string()),
            _ => {
                if let Some(code) = code(word) {
                    if fg.is_none() {
                        fg = Some(code);
                    } else {
                        bg = Some(code);
                    }
                } else {
                    return None;
                }
            }
        }
    }
    let mut parts: Vec<String> = attrs.iter().map(u8::to_string).collect();
    if let Some(fg) = fg {
        parts.push((30 + fg).to_string());
    }
    if let Some(bg) = bg {
        parts.push((40 + bg).to_string());
    }
    if parts.is_empty() {
        return Some(String::new());
    }
    Some(format!("\x1b[{}m", parts.join(";")))
}

/// Wrap one already-newline-terminated line in a color, mirroring
/// `emit_line_0`: the reset lands before the trailing newline, and a line
/// that is empty (ignoring its newline) is passed through uncolored.
pub(crate) fn push_colored_line(out: &mut Vec<u8>, color: &str, reset: &str, line: &[u8]) {
    let (body, newline): (&[u8], &[u8]) = match line.split_last() {
        Some((b'\n', body)) => (body, b"\n"),
        _ => (line, b""),
    };
    if body.is_empty() {
        out.extend_from_slice(newline);
        return;
    }
    if color.is_empty() && reset.is_empty() {
        out.extend_from_slice(body);
        out.extend_from_slice(newline);
        return;
    }
    out.extend_from_slice(color.as_bytes());
    out.extend_from_slice(body);
    out.extend_from_slice(reset.as_bytes());
    out.extend_from_slice(newline);
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum WordDiffMode {
    Plain,
    Porcelain,
    Color,
}

/// Word-diff configuration for one file pair: the rendering mode, the
/// compiled word regex (None = whitespace tokenization), and the palette.
pub(crate) struct WordDiffConfig<'a> {
    pub(crate) mode: WordDiffMode,
    pub(crate) regex: Option<&'a Regex>,
    pub(crate) colors: &'a DiffColors,
}

struct StyleElem<'a> {
    prefix: &'a str,
    suffix: &'a str,
    color: &'a str,
}

struct WordStyle<'a> {
    new_word: StyleElem<'a>,
    old_word: StyleElem<'a>,
    ctx: StyleElem<'a>,
    newline: &'a str,
}

impl<'a> WordDiffConfig<'a> {
    fn style(&self) -> WordStyle<'a> {
        let colors = self.colors;
        match self.mode {
            WordDiffMode::Porcelain => WordStyle {
                new_word: StyleElem { prefix: "+", suffix: "\n", color: &colors.new },
                old_word: StyleElem { prefix: "-", suffix: "\n", color: &colors.old },
                ctx: StyleElem { prefix: " ", suffix: "\n", color: &colors.context },
                newline: "~\n",
            },
            WordDiffMode::Plain => WordStyle {
                new_word: StyleElem { prefix: "{+", suffix: "+}", color: &colors.new },
                old_word: StyleElem { prefix: "[-", suffix: "-]", color: &colors.old },
                ctx: StyleElem { prefix: "", suffix: "", color: &colors.context },
                newline: "\n",
            },
            WordDiffMode::Color => WordStyle {
                new_word: StyleElem { prefix: "", suffix: "", color: &colors.new },
                old_word: StyleElem { prefix: "", suffix: "", color: &colors.old },
                ctx: StyleElem { prefix: "", suffix: "", color: &colors.context },
                newline: "\n",
            },
        }
    }
}

/// Port of `fn_out_diff_words_write_helper`: emit `buf` (a byte range of the
/// original minus/plus text, possibly spanning newlines) one line segment at
/// a time, wrapping each non-empty segment in the style element and emitting
/// the style's newline string between segments.
fn write_word_helper(out: &mut Vec<u8>, elem: &StyleElem<'_>, newline: &str, buf: &[u8]) {
    let mut rest = buf;
    loop {
        let split = rest.iter().position(|&b| b == b'\n');
        let segment = match split {
            Some(at) => &rest[..at],
            None => rest,
        };
        if !segment.is_empty() {
            let colored = !elem.color.is_empty();
            if colored {
                out.extend_from_slice(elem.color.as_bytes());
            }
            out.extend_from_slice(elem.prefix.as_bytes());
            out.extend_from_slice(segment);
            out.extend_from_slice(elem.suffix.as_bytes());
            if colored {
                out.extend_from_slice(b"\x1b[m");
            }
        }
        let Some(at) = split else { break };
        out.extend_from_slice(newline.as_bytes());
        rest = &rest[at + 1..];
        if rest.is_empty() {
            break;
        }
    }
}

/// One tokenized word: its byte span in the original buffer.
struct WordSpan {
    begin: usize,
    end: usize,
}

/// Port of `find_word_boundaries` + `diff_words_fill`: split `text` into
/// words per `regex` (None = whitespace-separated runs), returning the
/// original spans. Zero-length regex matches skip one byte, exactly like the
/// `(*begin)++` upstream.
fn split_words(text: &[u8], regex: Option<&Regex>) -> Vec<WordSpan> {
    let mut words = Vec::new();
    let mut begin = 0usize;
    while begin < text.len() {
        match regex {
            Some(regex) => {
                let Some((so, eo)) = regex.find_longest_alternative(&text[begin..]) else {
                    break;
                };
                let match_bytes = &text[begin + so..begin + eo];
                let end = match match_bytes.iter().position(|&b| b == b'\n') {
                    Some(at) => begin + so + at,
                    None => begin + eo,
                };
                let start = begin + so;
                if start == end {
                    begin = start + 1;
                    continue;
                }
                words.push(WordSpan { begin: start, end });
                begin = end;
            }
            None => {
                while begin < text.len() && is_xdl_space(text[begin]) {
                    begin += 1;
                }
                if begin >= text.len() {
                    break;
                }
                let mut end = begin + 1;
                while end < text.len() && !is_xdl_space(text[end]) {
                    end += 1;
                }
                words.push(WordSpan { begin, end });
                begin = end;
            }
        }
    }
    words
}

fn is_xdl_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

/// The per-hunk word-diff renderer state: accumulated minus/plus text.
pub(crate) struct WordDiffBuffers {
    minus: Vec<u8>,
    plus: Vec<u8>,
}

impl WordDiffBuffers {
    pub(crate) fn new() -> Self {
        Self {
            minus: Vec::new(),
            plus: Vec::new(),
        }
    }

    /// Append one removed line's content (prefix already stripped).
    pub(crate) fn push_minus(&mut self, content: &[u8]) {
        self.minus.extend_from_slice(content);
    }

    /// Append one added line's content (prefix already stripped).
    pub(crate) fn push_plus(&mut self, content: &[u8]) {
        self.plus.extend_from_slice(content);
    }

    /// Port of `diff_words_show`: word-diff the accumulated buffers into
    /// `out` and reset them.
    pub(crate) fn flush(&mut self, out: &mut Vec<u8>, config: &WordDiffConfig<'_>) {
        if self.minus.is_empty() && self.plus.is_empty() {
            return;
        }
        let style = config.style();
        // Special case: only removal.
        if self.plus.is_empty() {
            write_word_helper(out, &style.old_word, style.newline, &self.minus);
            self.minus.clear();
            return;
        }
        let minus_words = split_words(&self.minus, config.regex);
        let plus_words = split_words(&self.plus, config.regex);
        // Word-level diff: each word becomes one "line" for the line differ.
        let minus_lines: Vec<sley_diff_merge::DiffLine<'_>> = minus_words
            .iter()
            .map(|span| sley_diff_merge::DiffLine {
                content: &self.minus[span.begin..span.end],
                has_newline: true,
            })
            .collect();
        let plus_lines: Vec<sley_diff_merge::DiffLine<'_>> = plus_words
            .iter()
            .map(|span| sley_diff_merge::DiffLine {
                content: &self.plus[span.begin..span.end],
                has_newline: true,
            })
            .collect();
        let ops = sley_diff_merge::myers_diff_lines(&minus_lines, &plus_lines);

        // Walk the edit script as (minus_first, minus_len, plus_first,
        // plus_len) changes, mirroring fn_out_diff_words_aux.
        let mut current_plus = 0usize; // byte offset into self.plus
        let mut minus_idx = 0usize;
        let mut plus_idx = 0usize;
        let mut pending_del = 0usize;
        let mut pending_ins = 0usize;
        let mut emit_change = |out: &mut Vec<u8>,
                               minus_first: usize,
                               minus_len: usize,
                               plus_first: usize,
                               plus_len: usize,
                               current_plus: &mut usize| {
            let (minus_begin, minus_end) = if minus_len > 0 {
                (
                    minus_words[minus_first].begin,
                    minus_words[minus_first + minus_len - 1].end,
                )
            } else {
                let anchor = if minus_first == 0 {
                    0
                } else {
                    minus_words[minus_first - 1].end
                };
                (anchor, anchor)
            };
            let (plus_begin, plus_end) = if plus_len > 0 {
                (
                    plus_words[plus_first].begin,
                    plus_words[plus_first + plus_len - 1].end,
                )
            } else {
                let anchor = if plus_first == 0 {
                    0
                } else {
                    plus_words[plus_first - 1].end
                };
                (anchor, anchor)
            };
            if *current_plus != plus_begin {
                write_word_helper(
                    out,
                    &style.ctx,
                    style.newline,
                    &self.plus[*current_plus..plus_begin],
                );
            }
            if minus_begin != minus_end {
                write_word_helper(
                    out,
                    &style.old_word,
                    style.newline,
                    &self.minus[minus_begin..minus_end],
                );
            }
            if plus_begin != plus_end {
                write_word_helper(
                    out,
                    &style.new_word,
                    style.newline,
                    &self.plus[plus_begin..plus_end],
                );
            }
            *current_plus = plus_end;
        };
        for op in ops {
            match op {
                sley_diff_merge::DiffOp::Delete(n) => pending_del += n,
                sley_diff_merge::DiffOp::Insert(n) => pending_ins += n,
                sley_diff_merge::DiffOp::Equal(n) => {
                    if pending_del > 0 || pending_ins > 0 {
                        emit_change(
                            out,
                            minus_idx,
                            pending_del,
                            plus_idx,
                            pending_ins,
                            &mut current_plus,
                        );
                        minus_idx += pending_del;
                        plus_idx += pending_ins;
                        pending_del = 0;
                        pending_ins = 0;
                    }
                    minus_idx += n;
                    plus_idx += n;
                }
            }
        }
        if pending_del > 0 || pending_ins > 0 {
            emit_change(
                out,
                minus_idx,
                pending_del,
                plus_idx,
                pending_ins,
                &mut current_plus,
            );
        }
        if current_plus != self.plus.len() {
            write_word_helper(
                out,
                &style.ctx,
                style.newline,
                &self.plus[current_plus..],
            );
        }
        self.minus.clear();
        self.plus.clear();
    }

    /// Emit a context line in word-diff mode (after flushing): porcelain
    /// keeps the ` ` prefix and appends `~`; plain/color drop the prefix.
    pub(crate) fn emit_context_line(
        out: &mut Vec<u8>,
        config: &WordDiffConfig<'_>,
        content: &[u8],
    ) {
        let colors = config.colors;
        match config.mode {
            WordDiffMode::Porcelain => {
                let mut line = Vec::with_capacity(content.len() + 1);
                line.push(b' ');
                line.extend_from_slice(content);
                if !line.ends_with(b"\n") {
                    line.push(b'\n');
                }
                push_colored_line(out, &colors.context, &colors.reset, &line);
                out.extend_from_slice(b"~\n");
            }
            WordDiffMode::Plain | WordDiffMode::Color => {
                let mut line = content.to_vec();
                if !line.ends_with(b"\n") {
                    line.push(b'\n');
                }
                push_colored_line(out, &colors.context, &colors.reset, &line);
            }
        }
    }
}
