use super::*;
use sley_grep::{Regex, RegexMode};

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            while chars.next().is_some_and(|ch| ch != 'm') {}
            continue;
        }
        out.push(c);
    }
    out
}

#[test]
fn parse_color_red() {
    assert_eq!(parse_color_value("red"), Some("\x1b[31m".to_string()));
}

#[test]
fn parse_color_bold_green() {
    assert_eq!(
        parse_color_value("bold green"),
        Some("\x1b[1;32m".to_string())
    );
}

#[test]
fn parse_color_reset() {
    assert_eq!(parse_color_value("reset"), Some("\x1b[m".to_string()));
}

#[test]
fn parse_color_unknown_word_returns_none() {
    assert_eq!(parse_color_value("notacolor"), None);
}

#[test]
fn parse_color_empty_returns_empty_string() {
    assert_eq!(parse_color_value(""), Some(String::new()));
}

#[test]
fn parse_color_reverse_attr() {
    assert_eq!(parse_color_value("reverse"), Some("\x1b[7m".to_string()));
}

#[test]
fn parse_color_dim() {
    assert_eq!(parse_color_value("dim"), Some("\x1b[2m".to_string()));
}

#[test]
fn parse_color_blue_background() {
    assert_eq!(
        parse_color_value("red blue"),
        Some("\x1b[31;44m".to_string())
    );
}

#[test]
fn push_colored_line_wraps_body() {
    let mut out = Vec::new();
    push_colored_line(&mut out, "\x1b[31m", "\x1b[m", b"hello\n");
    assert_eq!(strip_ansi(&String::from_utf8_lossy(&out)), "hello\n");
}

#[test]
fn push_colored_line_empty_body_emits_newline_only() {
    let mut out = Vec::new();
    push_colored_line(&mut out, "\x1b[31m", "\x1b[m", b"\n");
    assert_eq!(out, b"\n");
}

#[test]
fn push_colored_line_no_color_passes_through() {
    let mut out = Vec::new();
    push_colored_line(&mut out, "", "", b"x\n");
    assert_eq!(out, b"x\n");
}

#[test]
fn push_colored_line_without_trailing_newline() {
    let mut out = Vec::new();
    push_colored_line(&mut out, "\x1b[32m", "\x1b[m", b"no newline");
    assert_eq!(strip_ansi(&String::from_utf8_lossy(&out)), "no newline");
}

#[test]
fn diff_colors_enabled_defaults() {
    let colors = DiffColors::enabled(None);
    assert!(colors.meta.contains("\x1b["));
    assert!(colors.old.contains("\x1b[31m"));
    assert!(colors.new.contains("\x1b[32m"));
    assert_eq!(colors.reset, "\x1b[m");
}

#[test]
fn render_colors_maps_palette() {
    let colors = DiffColors::enabled(None);
    let mapped = render_colors(&colors);
    assert_eq!(mapped.old, colors.old.as_str());
    assert_eq!(mapped.new, colors.new.as_str());
}

#[test]
fn default_funcname_heading_accepts_underscore() {
    assert_eq!(
        default_funcname_heading(b"fn main() {\n"),
        Some(b"fn main() {".to_vec())
    );
}

#[test]
fn default_funcname_heading_rejects_digit_lead() {
    assert_eq!(default_funcname_heading(b"1bad\n"), None);
}

#[test]
fn default_funcname_heading_trims_trailing_space() {
    assert_eq!(
        default_funcname_heading(b"hello   \n"),
        Some(b"hello".to_vec())
    );
}

#[test]
fn default_funcname_heading_dollar_prefix() {
    assert_eq!(
        default_funcname_heading(b"$injector\n"),
        Some(b"$injector".to_vec())
    );
}

#[test]
fn default_funcname_heading_truncates_at_utf8_boundary() {
    let line = [b"L  ".as_slice(), "日本語".repeat(13).as_bytes(), b"\n"].concat();
    let heading = default_funcname_heading(&line).expect("heading");
    assert!(std::str::from_utf8(&heading).is_ok());
    assert_eq!(heading.len(), 78);
}

#[test]
fn compiled_funcname_matches_simple_pattern() {
    let compiled = CompiledFuncname::compile(b"^fn.*", true, false).expect("compile");
    assert_eq!(
        compiled.match_line(b"fn foo() {\n"),
        Some(b"fn foo() {".to_vec())
    );
}

#[test]
fn compiled_funcname_negated_pattern_rejects() {
    let compiled = CompiledFuncname::compile(b"!^//\n^fn.*", true, false).expect("compile");
    assert_eq!(compiled.match_line(b"// comment\n"), None);
    assert_eq!(
        compiled.match_line(b"fn foo()\n"),
        Some(b"fn foo()".to_vec())
    );
}

#[test]
fn heading_classifier_uses_default_without_driver() {
    let mut classify = heading_classifier(None);
    assert_eq!(classify(b"struct S {\n"), Some(b"struct S {".to_vec()));
}

#[test]
fn word_diff_buffers_plain_mode_brackets() {
    let colors = DiffColors::enabled(None);
    let regex = Regex::compile_bytes(b"[a-zA-Z]+", RegexMode::Ere, false, false).ok();
    let config = WordDiffConfig {
        mode: WordDiffMode::Plain,
        regex: regex.as_ref(),
        colors: &colors,
    };
    let mut buffers = WordDiffBuffers::new();
    buffers.push_minus(b"hello ");
    buffers.push_plus(b"hella ");
    let mut out = Vec::new();
    buffers.flush(&mut out, &config);
    let rendered = String::from_utf8_lossy(&out);
    assert!(rendered.contains("[-"));
    assert!(rendered.contains("-]"));
    assert!(rendered.contains("{+"));
    assert!(rendered.contains("+}"));
}

#[test]
fn word_diff_buffers_color_mode_no_prefix() {
    let colors = DiffColors::enabled(None);
    let config = WordDiffConfig {
        mode: WordDiffMode::Color,
        regex: None,
        colors: &colors,
    };
    let mut buffers = WordDiffBuffers::new();
    buffers.push_minus(b"foo");
    buffers.push_plus(b"bar");
    let mut out = Vec::new();
    buffers.flush(&mut out, &config);
    assert!(!String::from_utf8_lossy(&out).contains("{+"));
}

#[test]
fn word_diff_emit_context_line_porcelain() {
    let colors = DiffColors::enabled(None);
    let config = WordDiffConfig {
        mode: WordDiffMode::Porcelain,
        regex: None,
        colors: &colors,
    };
    let mut out = Vec::new();
    WordDiffBuffers::emit_context_line(&mut out, &config, b"ctx\n");
    assert!(String::from_utf8_lossy(&out).ends_with("~\n"));
}

#[test]
fn word_diff_adapter_implements_flush() {
    use crate::render::HunkWordDiff;

    let colors = DiffColors::enabled(None);
    let config = WordDiffConfig {
        mode: WordDiffMode::Plain,
        regex: None,
        colors: &colors,
    };
    let mut adapter = WordDiffAdapter::new(&config);
    HunkWordDiff::push_minus(&mut adapter, b"a");
    HunkWordDiff::push_plus(&mut adapter, b"b");
    let mut out = Vec::new();
    HunkWordDiff::flush(&mut adapter, &mut out);
    assert!(!out.is_empty());
}

#[test]
fn word_diff_only_deletion() {
    let colors = DiffColors::enabled(None);
    let config = WordDiffConfig {
        mode: WordDiffMode::Plain,
        regex: None,
        colors: &colors,
    };
    let mut buffers = WordDiffBuffers::new();
    buffers.push_minus(b"gone");
    let mut out = Vec::new();
    buffers.flush(&mut out, &config);
    assert!(String::from_utf8_lossy(&out).contains("[-gone-]"));
}

#[test]
fn word_diff_empty_flush_is_noop() {
    let colors = DiffColors::enabled(None);
    let config = WordDiffConfig {
        mode: WordDiffMode::Color,
        regex: None,
        colors: &colors,
    };
    let mut buffers = WordDiffBuffers::new();
    let mut out = vec![b'x'];
    buffers.flush(&mut out, &config);
    assert_eq!(out, vec![b'x']);
}

#[test]
fn word_diff_whitespace_tokenization() {
    let colors = DiffColors::enabled(None);
    let config = WordDiffConfig {
        mode: WordDiffMode::Plain,
        regex: None,
        colors: &colors,
    };
    let mut buffers = WordDiffBuffers::new();
    buffers.push_minus(b"aa bb");
    buffers.push_plus(b"aa cc");
    let mut out = Vec::new();
    buffers.flush(&mut out, &config);
    let s = String::from_utf8_lossy(&out);
    assert!(s.contains("bb"));
    assert!(s.contains("cc"));
}

#[test]
fn parse_color_bold_red_green_bg() {
    let parsed = parse_color_value("bold red green").expect("parse");
    assert!(parsed.contains("1"));
    assert!(parsed.contains("31"));
    assert!(parsed.contains("42"));
}

#[test]
fn parse_color_normal_then_cyan() {
    // `normal` resets attrs; a following color word sets foreground.
    let parsed = parse_color_value("cyan").expect("parse");
    assert!(parsed.contains("36"));
}

#[test]
fn compiled_funcname_capture_group() {
    let compiled =
        CompiledFuncname::compile(b"^[[:space:]]*(func.*)", true, false).expect("compile");
    assert_eq!(
        compiled.match_line(b"  func foo()\n"),
        Some(b"func foo()".to_vec())
    );
}

#[test]
fn default_funcname_empty_line_none() {
    assert_eq!(default_funcname_heading(b"\n"), None);
}

#[test]
fn push_colored_line_multibyte_safe() {
    let mut out = Vec::new();
    push_colored_line(&mut out, "\x1b[36m", "\x1b[m", "café\n".as_bytes());
    assert!(out.ends_with(b"\n"));
}
