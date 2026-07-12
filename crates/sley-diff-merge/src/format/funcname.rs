use sley_core::{GitError, Result};
use sley_grep::{Regex, RegexMode};

/// A compiled funcname spec: newline-separated POSIX regexes tried in order;
/// a leading `!` negates (a match rejects the line). Port of
/// `xdiff_set_find_func` + `ff_regexp` from upstream `xdiff-interface.c`.
pub struct CompiledFuncname {
    patterns: Vec<(bool, Regex)>,
}

/// The upstream hunk-header buffer is `char buf[80]` (`struct func_line`);
/// headings are truncated to it before the trailing-whitespace trim.
const FUNCNAME_BUFFER: usize = 80;

fn truncated_heading_len(heading: &[u8]) -> usize {
    let mut len = heading.len().min(FUNCNAME_BUFFER);
    // Git's UTF-8-aware hunk-header truncation never leaves a partial
    // multibyte character in the fixed 80-byte funcname buffer. Preserve raw
    // bytes for non-UTF-8 inputs, but retreat to a scalar boundary for valid
    // UTF-8 headings.
    if len < heading.len()
        && let Ok(text) = std::str::from_utf8(heading)
    {
        while !text.is_char_boundary(len) {
            len -= 1;
        }
    }
    len
}

impl CompiledFuncname {
    /// Compile a funcname spec. `extended` selects ERE (`xfuncname` /
    /// builtins) over BRE (`funcname` config). Errors mirror upstream's
    /// `die()` calls byte-for-byte (printed to stderr, exit 128).
    pub fn compile(spec: &[u8], extended: bool, icase: bool) -> Result<Self> {
        let lines: Vec<&[u8]> = spec.split(|&b| b == b'\n').collect();
        let mut patterns = Vec::new();
        for (idx, line) in lines.iter().enumerate() {
            let negate = line.first() == Some(&b'!');
            if negate && idx == lines.len() - 1 {
                // die("Last expression must not be negated: %s", value) —
                // `value` is the remaining suffix of the spec at that point.
                let suffix: Vec<u8> = lines[idx..].join(&b'\n');
                eprintln!(
                    "fatal: Last expression must not be negated: {}",
                    String::from_utf8_lossy(&suffix)
                );
                return Err(GitError::Exit(128));
            }
            let expression = if negate { &line[1..] } else { line };
            let mode = if extended {
                RegexMode::Ere
            } else {
                RegexMode::Bre
            };
            let regex = Regex::compile_bytes(expression, mode, icase, false).map_err(|_| {
                eprintln!(
                    "fatal: Invalid regexp to look for hunk header: {}",
                    String::from_utf8_lossy(expression)
                );
                GitError::Exit(128)
            })?;
            patterns.push((negate, regex));
        }
        Ok(Self { patterns })
    }

    /// Port of `ff_regexp`: match `line` (raw record bytes, trailing newline
    /// still attached) against the pattern list; return the heading bytes, or
    /// `None` when no pattern accepts the line.
    pub fn match_line(&self, line: &[u8]) -> Option<Vec<u8>> {
        // Exclude terminating newline (and cr) from matching.
        let mut len = line.len();
        if len > 0 && line[len - 1] == b'\n' {
            if len > 1 && line[len - 2] == b'\r' {
                len -= 2;
            } else {
                len -= 1;
            }
        }
        let line = &line[..len];
        let mut matched: Option<Vec<Option<(usize, usize)>>> = None;
        for (negate, regex) in &self.patterns {
            if let Some(captures) = regex.find_captures(line) {
                if *negate {
                    return None;
                }
                matched = Some(captures);
                break;
            }
        }
        let captures = matched?;
        let (start, end) = captures
            .get(1)
            .copied()
            .flatten()
            .unwrap_or_else(|| captures[0].expect("whole-match span present"));
        let heading = &line[start..end];
        let mut result = truncated_heading_len(heading);
        while result > 0 && heading[result - 1].is_ascii_whitespace() {
            result -= 1;
        }
        Some(heading[..result].to_vec())
    }
}

/// Port of `def_ff` (the default funcname heuristic): a line whose first byte
/// is a letter, `_`, or `$` is a section heading, truncated to the 80-byte
/// header buffer with trailing whitespace trimmed.
pub fn default_funcname_heading(line: &[u8]) -> Option<Vec<u8>> {
    let first = line.first()?;
    if !(first.is_ascii_alphabetic() || *first == b'_' || *first == b'$') {
        return None;
    }
    let mut len = truncated_heading_len(line);
    while len > 0 && line[len - 1].is_ascii_whitespace() {
        len -= 1;
    }
    Some(line[..len].to_vec())
}
