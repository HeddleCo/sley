//! Word-diff rendering (`--word-diff=plain|porcelain|color`, `--color-words`)
//! and the diff color palette, ported from upstream `diff.c`
//! (`diff_words_styles`, `fn_out_diff_words_aux`, `find_word_boundaries`,
//! `diff_words_fill`, `emit_hunk_header`).

use sley_config::GitConfig;
use sley_grep::Regex;

/// ANSI palette for colored diff output. Each slot holds the escape sequence
/// (empty when color is disabled), mirroring `diff_get_color`.
#[derive(Clone, Default)]
pub struct DiffColors {
    pub meta: String,
    pub frag: String,
    pub func: String,
    pub old: String,
    pub new: String,
    pub context: String,
    pub reset: String,
    /// `color.diff.whitespace` — the highlight for whitespace errors
    /// (`--ws-error-highlight`). Default `[7m` (reverse), matching git.
    pub whitespace: String,
    pub old_moved: String,
    pub old_moved_alt: String,
    pub old_moved_dim: String,
    pub old_moved_alt_dim: String,
    pub new_moved: String,
    pub new_moved_alt: String,
    pub new_moved_dim: String,
    pub new_moved_alt_dim: String,
}

impl DiffColors {
    /// The default enabled palette: meta=bold, frag=cyan, old=red, new=green,
    /// func/context unset, overridden by `color.diff.<slot>` with the legacy
    /// `diff.color.<slot>` spelling as a fallback.
    pub fn enabled(config: Option<&GitConfig>) -> Self {
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
            // git's GIT_COLOR_REVERSE for whitespace by default; the test
            // decoder names this red-background span `<BRED>`.
            whitespace: lookup("whitespace", "\x1b[41m"),
            old_moved: lookup("oldMoved", "\x1b[1;35m"),
            old_moved_alt: lookup("oldMovedAlternative", "\x1b[1;34m"),
            old_moved_dim: lookup("oldMovedDimmed", "\x1b[2m"),
            old_moved_alt_dim: lookup("oldMovedAlternativeDimmed", "\x1b[2;3m"),
            new_moved: lookup("newMoved", "\x1b[1;36m"),
            new_moved_alt: lookup("newMovedAlternative", "\x1b[1;33m"),
            new_moved_dim: lookup("newMovedDimmed", "\x1b[2m"),
            new_moved_alt_dim: lookup("newMovedAlternativeDimmed", "\x1b[2;3m"),
        }
    }
}

/// Parse a git color word ("red", "bold", "green dim", ...) into an ANSI
/// sequence. Only the simple forms the diff palette uses are supported;
/// unknown words yield `None` (caller keeps the default).
pub fn parse_color_value(value: &str) -> Option<String> {
    let mut fg: Option<u8> = None;
    let mut fg_seen = false;
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
            "italic" => attrs.push(3),
            "ul" => attrs.push(4),
            "blink" => attrs.push(5),
            "reverse" => attrs.push(7),
            "normal" => fg_seen = true,
            "reset" => return Some("\x1b[m".to_string()),
            _ => {
                if let Some(code) = code(word) {
                    if !fg_seen {
                        fg = Some(code);
                        fg_seen = true;
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
pub fn push_colored_line(out: &mut Vec<u8>, color: &str, reset: &str, line: &[u8]) {
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
pub enum WordDiffMode {
    Plain,
    Porcelain,
    Color,
}

/// Word-diff configuration for one file pair: the rendering mode, the
/// compiled word regex (None = whitespace tokenization), and the palette.
pub struct WordDiffConfig<'a> {
    pub mode: WordDiffMode,
    pub regex: Option<&'a Regex>,
    pub colors: &'a DiffColors,
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
                new_word: StyleElem {
                    prefix: "+",
                    suffix: "\n",
                    color: &colors.new,
                },
                old_word: StyleElem {
                    prefix: "-",
                    suffix: "\n",
                    color: &colors.old,
                },
                ctx: StyleElem {
                    prefix: " ",
                    suffix: "\n",
                    color: &colors.context,
                },
                newline: "~\n",
            },
            WordDiffMode::Plain => WordStyle {
                new_word: StyleElem {
                    prefix: "{+",
                    suffix: "+}",
                    color: &colors.new,
                },
                old_word: StyleElem {
                    prefix: "[-",
                    suffix: "-]",
                    color: &colors.old,
                },
                ctx: StyleElem {
                    prefix: "",
                    suffix: "",
                    color: &colors.context,
                },
                newline: "\n",
            },
            WordDiffMode::Color => WordStyle {
                new_word: StyleElem {
                    prefix: "",
                    suffix: "",
                    color: &colors.new,
                },
                old_word: StyleElem {
                    prefix: "",
                    suffix: "",
                    color: &colors.old,
                },
                ctx: StyleElem {
                    prefix: "",
                    suffix: "",
                    color: &colors.context,
                },
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
pub struct WordDiffBuffers {
    minus: Vec<u8>,
    plus: Vec<u8>,
}

impl Default for WordDiffBuffers {
    fn default() -> Self {
        Self::new()
    }
}

impl WordDiffBuffers {
    pub fn new() -> Self {
        Self {
            minus: Vec::new(),
            plus: Vec::new(),
        }
    }

    /// Append one removed line's content (prefix already stripped).
    pub fn push_minus(&mut self, content: &[u8]) {
        self.minus.extend_from_slice(content);
    }

    /// Append one added line's content (prefix already stripped).
    pub fn push_plus(&mut self, content: &[u8]) {
        self.plus.extend_from_slice(content);
    }

    /// Port of `diff_words_show`: word-diff the accumulated buffers into
    /// `out` and reset them.
    pub fn flush(&mut self, out: &mut Vec<u8>, config: &WordDiffConfig<'_>) {
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
        let minus_lines: Vec<crate::DiffLine<'_>> = minus_words
            .iter()
            .map(|span| crate::DiffLine {
                content: &self.minus[span.begin..span.end],
                has_newline: true,
            })
            .collect();
        let plus_lines: Vec<crate::DiffLine<'_>> = plus_words
            .iter()
            .map(|span| crate::DiffLine {
                content: &self.plus[span.begin..span.end],
                has_newline: true,
            })
            .collect();
        let ops = crate::myers_diff_lines(&minus_lines, &plus_lines);

        // Walk the edit script as (minus_first, minus_len, plus_first,
        // plus_len) changes, mirroring fn_out_diff_words_aux.
        let mut current_plus = 0usize; // byte offset into self.plus
        let mut minus_idx = 0usize;
        let mut plus_idx = 0usize;
        let mut pending_del = 0usize;
        let mut pending_ins = 0usize;
        let emit_change = |out: &mut Vec<u8>,
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
                crate::DiffOp::Delete(n) => pending_del += n,
                crate::DiffOp::Insert(n) => pending_ins += n,
                crate::DiffOp::Equal(n) => {
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
            write_word_helper(out, &style.ctx, style.newline, &self.plus[current_plus..]);
        }
        self.minus.clear();
        self.plus.clear();
    }

    /// Emit a context line in word-diff mode (after flushing): porcelain
    /// keeps the ` ` prefix and appends `~`; plain/color drop the prefix.
    pub fn emit_context_line(out: &mut Vec<u8>, config: &WordDiffConfig<'_>, content: &[u8]) {
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
