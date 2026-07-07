//! Shared grep-source matching engine.

use sley_core::{GitError, Result};

pub const INVALID_REGEX_MESSAGE: &str = "Invalid regular expression";
pub const UNBALANCED_BRACKETS_MESSAGE: &str = "brackets ([ ]) not balanced";

/// How the regular expression text is interpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatternKind {
    /// POSIX basic regular expressions (Git's default).
    Basic,
    /// POSIX extended regular expressions (`-E` / `--extended-regexp`).
    Extended,
    /// Fixed strings (`-F` / `--fixed-strings`).
    Fixed,
    /// Perl-compatible regular expressions (`-P` / `--perl-regexp`).
    Perl,
}

/// Mirror of git's `GREP_PATTERN_TYPE_*`. `Unspecified` means "fall back to
/// `extended_regexp_option`".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatternTypeOption {
    Unspecified,
    Bre,
    Ere,
    Fixed,
    Pcre,
}

/// A token in the boolean grep expression (parsed from the argv stream).
#[derive(Clone)]
pub enum ExprToken {
    Pattern(usize), // index into the compiled pattern list
    And,
    Or,
    Not,
    Open,
    Close,
}

/// The parsed boolean expression tree (`-e A --and ( -e B --or --not -e C )`).
#[derive(Clone)]
pub enum Expr {
    /// Leaf: index into the compiled pattern list.
    Atom(usize),
    Not(Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegexDiagnosticVerbosity {
    Default,
    Verbose,
}

impl RegexDiagnosticVerbosity {
    pub fn from_env() -> Self {
        match std::env::var_os("SLEY_REGEX_VERBOSE").and_then(|value| value.into_string().ok()) {
            Some(value) if !value.is_empty() && value != "0" && value != "false" => Self::Verbose,
            _ => Self::Default,
        }
    }

    /// The verbosity that matches the platform's libc `regerror` strings, so
    /// `*_matches_upstream_git` parity holds against a git built on the same
    /// platform. git prints the C library's `regerror` text for a bad pattern:
    /// BSD libc (macOS / *BSD) yields the detailed `brackets ([ ]) not
    /// balanced` (Verbose); glibc (Linux, what CI builds) yields the generic
    /// `Invalid regular expression` (Default).
    pub const fn platform_default() -> Self {
        if platform_uses_bsd_libc_diagnostics() {
            Self::Verbose
        } else {
            Self::Default
        }
    }
}

/// Whether the target platform's C library emits the detailed BSD-style regex
/// and option diagnostics (macOS and the BSDs) rather than glibc's terser forms.
pub const fn platform_uses_bsd_libc_diagnostics() -> bool {
    cfg!(any(
        target_vendor = "apple",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly",
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegexDiagnosticDetail {
    Generic,
    UnbalancedBrackets,
}

impl RegexDiagnosticDetail {
    fn from_error(err: &GitError) -> Self {
        match err {
            GitError::Command(message)
                if message.contains("bracket")
                    || message.contains("class")
                    || message.contains("unbalanced [") =>
            {
                Self::UnbalancedBrackets
            }
            _ => Self::Generic,
        }
    }
}

pub fn regex_diagnostic_message(
    detail: RegexDiagnosticDetail,
    verbosity: RegexDiagnosticVerbosity,
) -> &'static str {
    match (detail, verbosity) {
        (RegexDiagnosticDetail::UnbalancedBrackets, RegexDiagnosticVerbosity::Verbose) => {
            UNBALANCED_BRACKETS_MESSAGE
        }
        _ => INVALID_REGEX_MESSAGE,
    }
}

pub fn report_regex_compile_error(
    error_context: &str,
    pattern: &str,
    verbosity: RegexDiagnosticVerbosity,
    detail: RegexDiagnosticDetail,
) -> GitError {
    let message = regex_diagnostic_message(detail, verbosity);
    eprintln!("fatal: {error_context}, '{pattern}': {message}");
    GitError::Exit(128)
}

pub fn report_regex_error(
    error_context: &str,
    pattern: &str,
    verbosity: RegexDiagnosticVerbosity,
    err: &GitError,
) -> GitError {
    report_regex_compile_error(
        error_context,
        pattern,
        verbosity,
        RegexDiagnosticDetail::from_error(err),
    )
}

pub struct GrepCompileConfig<'a> {
    pub patterns: &'a [String],
    pub kind: PatternKind,
    pub ignore_case: bool,
    pub word: bool,
    pub line_regexp: bool,
    pub diagnostic_verbosity: RegexDiagnosticVerbosity,
}

// ---------------------------------------------------------------------------
// Regular-expression engine (POSIX BRE/ERE subset) + fixed strings
// ---------------------------------------------------------------------------

pub struct GrepMatcher {
    patterns: Vec<CompiledPattern>,
    line_regexp: bool,
}

pub enum CompiledPattern {
    Fixed { needle: Vec<u8>, ignore_case: bool },
    Regex(Regex),
}

impl GrepMatcher {
    pub fn compile(config: GrepCompileConfig<'_>) -> Result<Self> {
        Self::compile_with_error_context(config, "command line")
    }

    pub fn compile_with_error_context(
        config: GrepCompileConfig<'_>,
        error_context: &str,
    ) -> Result<Self> {
        let mut patterns = Vec::new();
        for raw in config.patterns {
            let compiled = match config.kind {
                PatternKind::Fixed => CompiledPattern::Fixed {
                    needle: raw.as_bytes().to_vec(),
                    ignore_case: config.ignore_case,
                },
                PatternKind::Basic | PatternKind::Extended | PatternKind::Perl => {
                    let mode = match config.kind {
                        PatternKind::Basic => RegexMode::Bre,
                        PatternKind::Extended => RegexMode::Ere,
                        _ => RegexMode::Pcre,
                    };
                    let regex = Regex::compile(raw, mode, config.ignore_case, config.word)
                        .map_err(|err| {
                            report_regex_error(
                                error_context,
                                raw,
                                config.diagnostic_verbosity,
                                &err,
                            )
                        })?;
                    CompiledPattern::Regex(regex)
                }
            };
            patterns.push(compiled);
        }
        Ok(Self {
            patterns,
            line_regexp: config.line_regexp,
        })
    }

    pub fn pattern_count(&self) -> usize {
        self.patterns.len()
    }

    pub fn matches_any(&self, haystack: &[u8]) -> bool {
        (0..self.pattern_count()).any(|idx| self.find_idx(idx, haystack, 0).is_some())
    }

    pub fn matches_all(&self, haystack: &[u8]) -> bool {
        (0..self.pattern_count()).all(|idx| self.find_idx(idx, haystack, 0).is_some())
    }

    /// Find the leftmost match of pattern `idx` starting at `from`.
    pub fn find_idx(&self, idx: usize, line: &[u8], from: usize) -> Option<(usize, usize)> {
        let pattern = &self.patterns[idx];
        if self.line_regexp {
            if pattern.matches_line(line, true) && from == 0 {
                return Some((0, line.len()));
            }
            return None;
        }
        pattern.find_from(line, from)
    }

    /// Byte spans of (non-overlapping, left-most) matches on `line`, for `-o`.
    /// In expression mode, scans only the positive (atom) patterns.
    pub fn match_spans_expr(&self, expr: Option<&Expr>, line: &[u8]) -> Vec<(usize, usize)> {
        let indices = self.positive_pattern_indices(expr);
        let mut spans = Vec::new();
        let mut start = 0;
        while start <= line.len() {
            let mut best: Option<(usize, usize)> = None;
            for &idx in &indices {
                if let Some((s, e)) = self.find_idx(idx, line, start) {
                    best = match best {
                        Some((bs, _)) if bs <= s => best,
                        _ => Some((s, e)),
                    };
                }
            }
            match best {
                Some((s, e)) => {
                    spans.push((s, e));
                    start = if e > s { e } else { e + 1 };
                }
                None => break,
            }
        }
        spans
    }

    pub fn matches_all_positive_patterns<'a>(
        &self,
        expr: Option<&Expr>,
        lines: impl IntoIterator<Item = &'a [u8]>,
    ) -> bool {
        let indices = self.positive_pattern_indices(expr);
        if indices.len() <= 1 {
            return true;
        }
        let mut seen = vec![false; indices.len()];
        for line in lines {
            for (seen, idx) in seen.iter_mut().zip(&indices) {
                if !*seen && self.find_idx(*idx, line, 0).is_some() {
                    *seen = true;
                }
            }
            if seen.iter().all(|matched| *matched) {
                return true;
            }
        }
        false
    }

    fn positive_pattern_indices(&self, expr: Option<&Expr>) -> Vec<usize> {
        match expr {
            Some(e) => {
                let mut v = Vec::new();
                collect_positive_atoms(e, false, &mut v);
                v
            }
            None => (0..self.patterns.len()).collect(),
        }
    }
}

fn collect_positive_atoms(expr: &Expr, negated: bool, out: &mut Vec<usize>) {
    match expr {
        Expr::Atom(idx) => {
            if !negated {
                out.push(*idx);
            }
        }
        Expr::Not(inner) => collect_positive_atoms(inner, !negated, out),
        Expr::And(l, r) | Expr::Or(l, r) => {
            collect_positive_atoms(l, negated, out);
            collect_positive_atoms(r, negated, out);
        }
    }
}

impl CompiledPattern {
    fn matches_line(&self, line: &[u8], line_regexp: bool) -> bool {
        match self {
            CompiledPattern::Fixed {
                needle,
                ignore_case,
            } => {
                if line_regexp {
                    return bytes_eq(line, needle, *ignore_case);
                }
                contains(line, needle, *ignore_case)
            }
            CompiledPattern::Regex(regex) => {
                if line_regexp {
                    return regex.matches_whole(line);
                }
                regex.find_from(line, 0).is_some()
            }
        }
    }

    fn find_from(&self, line: &[u8], from: usize) -> Option<(usize, usize)> {
        match self {
            CompiledPattern::Fixed {
                needle,
                ignore_case,
            } => find_substring(line, needle, *ignore_case, from).map(|s| (s, s + needle.len())),
            CompiledPattern::Regex(regex) => regex.find_from(line, from),
        }
    }
}

fn bytes_eq(a: &[u8], b: &[u8], ignore_case: bool) -> bool {
    if a.len() != b.len() {
        return false;
    }
    if ignore_case {
        a.iter().zip(b).all(|(x, y)| x.eq_ignore_ascii_case(y))
    } else {
        a == b
    }
}

pub fn contains(haystack: &[u8], needle: &[u8], ignore_case: bool) -> bool {
    find_substring(haystack, needle, ignore_case, 0).is_some()
}

fn find_substring(haystack: &[u8], needle: &[u8], ignore_case: bool, from: usize) -> Option<usize> {
    if needle.is_empty() {
        return Some(from.min(haystack.len()));
    }
    if from > haystack.len() || needle.len() > haystack.len() - from {
        return None;
    }
    for start in from..=haystack.len() - needle.len() {
        let window = &haystack[start..start + needle.len()];
        let hit = if ignore_case {
            window
                .iter()
                .zip(needle)
                .all(|(x, y)| x.eq_ignore_ascii_case(y))
        } else {
            window == needle
        };
        if hit {
            return Some(start);
        }
    }
    None
}

// --- Regex AST -------------------------------------------------------------

/// Which dialect the pattern text is parsed as. `Pcre` is the in-house
/// Perl-compatible mode backing `-P` / `--perl-regexp` /
/// `grep.patternType=perl`: ERE-style syntax plus the PCRE extensions the
/// upstream test suite exercises (lazy quantifiers, `\x{..}`, `\p{..}`,
/// escapes inside classes, inline `(?i)`, named groups and backreferences,
/// and leading `(*VERB)` control verbs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegexMode {
    Bre,
    Ere,
    Pcre,
}

#[derive(Debug, Clone)]
enum Node {
    Literal(u8),
    AnyChar,
    Class {
        negate: bool,
        items: Vec<ClassItem>,
    },
    StartAnchor,
    EndAnchor,
    WordBoundary,
    NonWordBoundary,
    Concat(Vec<Node>),
    Alt(Vec<Node>),
    Repeat {
        node: Box<Node>,
        min: usize,
        max: Option<usize>,
        greedy: bool,
    },
    Group(Box<Node>),
    /// PCRE capturing group `(...)` / `(?P<name>...)`; `usize` is the group
    /// number (1-based, as in `\1`).
    Capture(usize, Box<Node>),
    /// PCRE backreference `\1` / `(?P=name)` to a capturing group.
    Backref(usize),
    /// Subtree matched case-insensitively (inline `(?i)`).
    IgnoreCase(Box<Node>),
    Empty,
}

#[derive(Debug, Clone)]
enum ClassItem {
    Single(u8),
    Range(u8, u8),
    Posix(PosixClass),
    /// `\p{..}` / `\P{..}` Unicode general-category escape, matched against the
    /// ASCII range (sley's grep operates bytewise; non-ASCII bytes never match).
    Category {
        negate: bool,
        cat: PerlCategory,
    },
}

/// ASCII approximation of the Unicode general categories the upstream tests
/// use with `\p{..}`.
#[derive(Debug, Clone, Copy)]
enum PerlCategory {
    /// `L` — letters (`Lu` and `Ll` are the cased subsets).
    Letter,
    UppercaseLetter,
    LowercaseLetter,
    /// `N` / `Nd` — (decimal) numbers.
    Number,
    /// `P` — punctuation; `Ps` / `Pe` are the open/close subsets.
    Punctuation,
    OpenPunctuation,
    ClosePunctuation,
    /// `S` — symbols.
    Symbol,
    /// `Z` / `Zs` — (space) separators.
    Separator,
}

#[derive(Debug, Clone, Copy)]
enum PosixClass {
    Alpha,
    Digit,
    Alnum,
    Space,
    Upper,
    Lower,
    Punct,
    Blank,
    Xdigit,
    Cntrl,
    Print,
    Graph,
}

#[derive(Debug, Clone)]
pub struct Regex {
    root: Node,
    ignore_case: bool,
    /// Number of capturing groups (PCRE mode); sizes the backreference slots.
    num_groups: usize,
}

impl Regex {
    pub fn compile(pattern: &str, mode: RegexMode, ignore_case: bool, word: bool) -> Result<Self> {
        Self::compile_bytes(pattern.as_bytes(), mode, ignore_case, word)
    }

    /// Compile a pattern given as raw bytes. Userdiff word regexes embed
    /// non-UTF-8 byte ranges (`[\xc0-\xff][\x80-\xbf]+`), so the byte form is
    /// the primitive; [`Regex::compile`] delegates here.
    pub fn compile_bytes(
        pattern: &[u8],
        mode: RegexMode,
        ignore_case: bool,
        word: bool,
    ) -> Result<Self> {
        let mut bytes = pattern;
        if mode == RegexMode::Pcre {
            // PCRE control verbs — `(*NO_JIT)`, `(*UTF)`, ... — are
            // engine-tuning directives at the start of the pattern; sley's
            // engine has no JIT to disable, so accept and ignore them.
            while bytes.starts_with(b"(*") {
                let Some(end) = bytes.iter().position(|&b| b == b')') else {
                    break;
                };
                bytes = &bytes[end + 1..];
            }
        }
        let mut parser = RegexParser {
            bytes,
            pos: 0,
            mode,
            num_groups: 0,
            group_names: Vec::new(),
        };
        let mut root = parser.parse_alternation()?;
        if parser.pos != bytes.len() {
            return Err(GitError::Command(format!(
                "invalid regular expression: {}",
                String::from_utf8_lossy(pattern)
            )));
        }
        if word {
            root = Node::Concat(vec![
                Node::WordBoundary,
                Node::Group(Box::new(root)),
                Node::WordBoundary,
            ]);
        }
        Ok(Self {
            root,
            ignore_case,
            num_groups: parser.num_groups,
        })
    }

    pub fn find_from(&self, text: &[u8], from: usize) -> Option<(usize, usize)> {
        self.find_from_with(text, from, self.ignore_case)
    }

    fn find_from_with(
        &self,
        text: &[u8],
        from: usize,
        ignore_case: bool,
    ) -> Option<(usize, usize)> {
        for start in from..=text.len() {
            let ctx = MatchCtx::new(text, self.num_groups);
            if let Some(end) = match_node(&self.root, &ctx, start, ignore_case) {
                return Some((start, end));
            }
        }
        None
    }

    fn matches_whole(&self, text: &[u8]) -> bool {
        let ctx = MatchCtx::new(text, self.num_groups);
        match_anchored_full(&self.root, &ctx, self.ignore_case)
    }

    /// Substring match with the caller's case sensitivity (used by the
    /// `log --grep` family, which resolves `-i` per invocation rather than at
    /// compile time).
    pub fn is_match_with_case(&self, text: &[u8], ignore_case: bool) -> bool {
        self.find_from_with(text, 0, ignore_case || self.ignore_case)
            .is_some()
    }

    /// Leftmost match with capture-group spans, like POSIX `regexec` filling
    /// `pmatch`. Index 0 is the whole match; index `n` is group `n`'s span or
    /// `None` when the group did not participate in the match.
    pub fn find_captures(&self, text: &[u8]) -> Option<Vec<Option<(usize, usize)>>> {
        for start in 0..=text.len() {
            let ctx = MatchCtx::new(text, self.num_groups);
            if let Some(end) = match_node(&self.root, &ctx, start, self.ignore_case) {
                let mut spans = ctx.captures.into_inner();
                spans[0] = Some((start, end));
                return Some(spans);
            }
        }
        None
    }

    /// Leftmost match approximating POSIX leftmost-*longest* alternation: at
    /// the first matching position, every top-level alternative is tried and
    /// the longest match wins (ties keep the earliest alternative). Userdiff
    /// word regexes are flat alternations of token shapes where `regexec`'s
    /// longest-match rule is observable (`0xdead` must tokenize via the hex
    /// alternative, not as `0` by the earlier decimal one).
    pub fn find_longest_alternative(&self, text: &[u8]) -> Option<(usize, usize)> {
        let branches: Vec<&Node> = match &self.root {
            Node::Alt(branches) => branches.iter().collect(),
            other => vec![other],
        };
        for start in 0..=text.len() {
            let mut best: Option<usize> = None;
            for branch in &branches {
                let ctx = MatchCtx::new(text, self.num_groups);
                if let Some(end) = match_node(branch, &ctx, start, self.ignore_case)
                    && best.is_none_or(|current| end > current)
                {
                    best = Some(end);
                }
            }
            if let Some(end) = best {
                return Some((start, end));
            }
        }
        None
    }
}

struct RegexParser<'a> {
    bytes: &'a [u8],
    pos: usize,
    mode: RegexMode,
    /// Capturing groups assigned so far (PCRE mode).
    num_groups: usize,
    /// `(?P<name>...)` name → group number.
    group_names: Vec<(Vec<u8>, usize)>,
}

impl RegexParser<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    /// `|`, `()`, `+`, `?`, `{n,m}` are unescaped metacharacters (ERE and PCRE)
    /// rather than BRE's escaped forms.
    fn extended(&self) -> bool {
        self.mode != RegexMode::Bre
    }

    fn pcre(&self) -> bool {
        self.mode == RegexMode::Pcre
    }

    fn parse_alternation(&mut self) -> Result<Node> {
        let mut branches = vec![self.parse_concat()?];
        loop {
            if self.at_alternation() {
                self.consume_alternation();
                branches.push(self.parse_concat()?);
            } else {
                break;
            }
        }
        if branches.len() == 1 {
            Ok(branches.remove(0))
        } else {
            Ok(Node::Alt(branches))
        }
    }

    fn at_alternation(&self) -> bool {
        match self.peek() {
            Some(b'|') if self.extended() => true,
            Some(b'\\') if !self.extended() => self.bytes.get(self.pos + 1) == Some(&b'|'),
            _ => false,
        }
    }

    fn consume_alternation(&mut self) {
        if self.extended() {
            self.pos += 1;
        } else {
            self.pos += 2;
        }
    }

    fn at_group_close(&self) -> bool {
        match self.peek() {
            Some(b')') if self.extended() => true,
            Some(b'\\') if !self.extended() => self.bytes.get(self.pos + 1) == Some(&b')'),
            _ => false,
        }
    }

    fn parse_concat(&mut self) -> Result<Node> {
        let mut nodes = Vec::new();
        while let Some(byte) = self.peek() {
            if self.at_alternation() || self.at_group_close() {
                break;
            }
            if byte == b'$' && self.is_end_anchor_position() {
                self.pos += 1;
                nodes.push(Node::EndAnchor);
                continue;
            }
            // Inline `(?i)`: the rest of the enclosing group/branch is matched
            // case-insensitively (PCRE flag-setting group).
            if self.pcre() && self.bytes[self.pos..].starts_with(b"(?i)") {
                self.pos += 4;
                let rest = self.parse_concat()?;
                nodes.push(Node::IgnoreCase(Box::new(rest)));
                break;
            }
            let atom = self.parse_atom(nodes.is_empty())?;
            let quantified = self.parse_quantifier(atom)?;
            nodes.push(quantified);
        }
        if nodes.is_empty() {
            Ok(Node::Empty)
        } else if nodes.len() == 1 {
            Ok(nodes.remove(0))
        } else {
            Ok(Node::Concat(nodes))
        }
    }

    fn is_end_anchor_position(&self) -> bool {
        let next = self.pos + 1;
        if next >= self.bytes.len() {
            return true;
        }
        if self.extended() {
            matches!(self.bytes.get(next), Some(b'|') | Some(b')'))
        } else {
            self.bytes.get(next) == Some(&b'\\')
                && matches!(self.bytes.get(next + 1), Some(b'|') | Some(b')'))
        }
    }

    fn parse_atom(&mut self, at_branch_start: bool) -> Result<Node> {
        let Some(byte) = self.peek() else {
            return Ok(Node::Empty);
        };
        match byte {
            b'^' if at_branch_start => {
                self.pos += 1;
                Ok(Node::StartAnchor)
            }
            b'.' => {
                self.pos += 1;
                Ok(Node::AnyChar)
            }
            b'[' => self.parse_class(),
            b'(' if self.pcre() => self.parse_pcre_group(),
            b'(' if self.extended() => {
                // POSIX ERE groups are capturing (regexec fills pmatch);
                // userdiff funcname headings use group 1 when it participated.
                self.pos += 1;
                self.num_groups += 1;
                let idx = self.num_groups;
                let inner = self.parse_alternation()?;
                if self.peek() != Some(b')') {
                    return Err(GitError::Command("unbalanced ( in regex".into()));
                }
                self.pos += 1;
                Ok(Node::Capture(idx, Box::new(inner)))
            }
            b'\\' => self.parse_escape(),
            other => {
                self.pos += 1;
                Ok(Node::Literal(other))
            }
        }
    }

    /// `(` in PCRE mode: plain `(...)` captures; `(?:...)` groups without
    /// capturing; `(?P<name>...)` is a named capture; `(?P=name)` is a
    /// backreference to one.
    fn parse_pcre_group(&mut self) -> Result<Node> {
        debug_assert_eq!(self.peek(), Some(b'('));
        let rest = &self.bytes[self.pos + 1..];
        if rest.starts_with(b"?P=") {
            let name_start = self.pos + 4;
            let Some(close) = self.bytes[name_start..].iter().position(|&b| b == b')') else {
                return Err(GitError::Command("unbalanced ( in regex".into()));
            };
            let name = &self.bytes[name_start..name_start + close];
            let Some(&(_, idx)) = self.group_names.iter().find(|(n, _)| n == name) else {
                return Err(GitError::Command(format!(
                    "reference to non-existent subpattern: {}",
                    String::from_utf8_lossy(name)
                )));
            };
            self.pos = name_start + close + 1;
            return Ok(Node::Backref(idx));
        }
        let mut capture_index = None;
        if rest.starts_with(b"?P<") {
            let name_start = self.pos + 4;
            let Some(close) = self.bytes[name_start..].iter().position(|&b| b == b'>') else {
                return Err(GitError::Command("malformed (?P<name>) group".into()));
            };
            let name = self.bytes[name_start..name_start + close].to_vec();
            self.num_groups += 1;
            self.group_names.push((name, self.num_groups));
            capture_index = Some(self.num_groups);
            self.pos = name_start + close + 1;
        } else if rest.starts_with(b"?:") {
            self.pos += 3;
        } else if rest.starts_with(b"?") {
            return Err(GitError::Command(
                "unsupported (?...) group in regex".into(),
            ));
        } else {
            self.pos += 1;
            self.num_groups += 1;
            capture_index = Some(self.num_groups);
        }
        let inner = self.parse_alternation()?;
        if self.peek() != Some(b')') {
            return Err(GitError::Command("unbalanced ( in regex".into()));
        }
        self.pos += 1;
        Ok(match capture_index {
            Some(idx) => Node::Capture(idx, Box::new(inner)),
            None => Node::Group(Box::new(inner)),
        })
    }

    fn parse_escape(&mut self) -> Result<Node> {
        let Some(next) = self.bytes.get(self.pos + 1).copied() else {
            self.pos += 1;
            return Ok(Node::Literal(b'\\'));
        };
        if self.pcre() {
            match next {
                b'1'..=b'9' => {
                    let idx = (next - b'0') as usize;
                    if idx <= self.num_groups {
                        self.pos += 2;
                        return Ok(Node::Backref(idx));
                    }
                }
                b'D' => {
                    self.pos += 2;
                    return Ok(Node::Class {
                        negate: true,
                        items: vec![ClassItem::Posix(PosixClass::Digit)],
                    });
                }
                b'S' => {
                    self.pos += 2;
                    return Ok(Node::Class {
                        negate: true,
                        items: vec![ClassItem::Posix(PosixClass::Space)],
                    });
                }
                b'x' => {
                    self.pos += 2;
                    let byte = self.parse_hex_escape()?;
                    return Ok(Node::Literal(byte));
                }
                b'p' | b'P' => {
                    self.pos += 2;
                    let item = self.parse_category_escape(next == b'P')?;
                    return Ok(Node::Class {
                        negate: false,
                        items: vec![item],
                    });
                }
                _ => {}
            }
        }
        if !self.extended() && next == b'(' {
            // POSIX BRE groups are capturing, like ERE `(...)` above.
            self.pos += 2;
            self.num_groups += 1;
            let idx = self.num_groups;
            let inner = self.parse_alternation()?;
            if !self.at_group_close() {
                return Err(GitError::Command("unbalanced \\( in regex".into()));
            }
            self.pos += 2;
            return Ok(Node::Capture(idx, Box::new(inner)));
        }
        match next {
            b'b' => {
                self.pos += 2;
                Ok(Node::WordBoundary)
            }
            b'B' => {
                self.pos += 2;
                Ok(Node::NonWordBoundary)
            }
            b'w' => {
                self.pos += 2;
                Ok(Node::Class {
                    negate: false,
                    items: vec![ClassItem::Posix(PosixClass::Alnum), ClassItem::Single(b'_')],
                })
            }
            b'W' => {
                self.pos += 2;
                Ok(Node::Class {
                    negate: true,
                    items: vec![ClassItem::Posix(PosixClass::Alnum), ClassItem::Single(b'_')],
                })
            }
            b'd' => {
                self.pos += 2;
                Ok(Node::Class {
                    negate: false,
                    items: vec![ClassItem::Posix(PosixClass::Digit)],
                })
            }
            b's' => {
                self.pos += 2;
                Ok(Node::Class {
                    negate: false,
                    items: vec![ClassItem::Posix(PosixClass::Space)],
                })
            }
            b't' => {
                self.pos += 2;
                Ok(Node::Literal(b'\t'))
            }
            b'n' => {
                self.pos += 2;
                Ok(Node::Literal(b'\n'))
            }
            other => {
                self.pos += 2;
                Ok(Node::Literal(other))
            }
        }
    }

    /// `\x{HH..}` / `\xHH` (cursor already past `\x`). Values above `0xFF` are
    /// rejected: the engine matches bytes.
    fn parse_hex_escape(&mut self) -> Result<u8> {
        let braced = self.peek() == Some(b'{');
        if braced {
            self.pos += 1;
        }
        let mut value: u32 = 0;
        let mut digits = 0;
        while let Some(byte) = self.peek() {
            let Some(digit) = (byte as char).to_digit(16) else {
                break;
            };
            value = value.saturating_mul(16).saturating_add(digit);
            digits += 1;
            self.pos += 1;
            if !braced && digits == 2 {
                break;
            }
        }
        if braced {
            if self.peek() != Some(b'}') {
                return Err(GitError::Command("malformed \\x{..} in regex".into()));
            }
            self.pos += 1;
        }
        if digits == 0 || value > 0xFF {
            return Err(GitError::Command("unsupported \\x escape in regex".into()));
        }
        Ok(value as u8)
    }

    /// `\p{Name}` / `\pL` (cursor already past `\p` / `\P`).
    fn parse_category_escape(&mut self, negate: bool) -> Result<ClassItem> {
        let name: Vec<u8> = if self.peek() == Some(b'{') {
            self.pos += 1;
            let Some(close) = self.bytes[self.pos..].iter().position(|&b| b == b'}') else {
                return Err(GitError::Command("malformed \\p{..} in regex".into()));
            };
            let name = self.bytes[self.pos..self.pos + close].to_vec();
            self.pos += close + 1;
            name
        } else {
            let Some(byte) = self.peek() else {
                return Err(GitError::Command("malformed \\p in regex".into()));
            };
            self.pos += 1;
            vec![byte]
        };
        let cat = match name.as_slice() {
            b"L" => PerlCategory::Letter,
            b"Lu" => PerlCategory::UppercaseLetter,
            b"Ll" => PerlCategory::LowercaseLetter,
            b"N" | b"Nd" => PerlCategory::Number,
            b"P" => PerlCategory::Punctuation,
            b"Ps" => PerlCategory::OpenPunctuation,
            b"Pe" => PerlCategory::ClosePunctuation,
            b"S" => PerlCategory::Symbol,
            b"Z" | b"Zs" => PerlCategory::Separator,
            other => {
                return Err(GitError::Command(format!(
                    "unsupported \\p category in regex: {}",
                    String::from_utf8_lossy(other)
                )));
            }
        };
        Ok(ClassItem::Category { negate, cat })
    }

    fn parse_class(&mut self) -> Result<Node> {
        self.pos += 1;
        let negate = matches!(self.peek(), Some(b'^'));
        if negate {
            self.pos += 1;
        }
        let mut items = Vec::new();
        let mut first = true;
        loop {
            let Some(byte) = self.peek() else {
                return Err(GitError::Command("unbalanced [ in regex".into()));
            };
            if byte == b']' && !first {
                self.pos += 1;
                break;
            }
            first = false;
            if byte == b'['
                && self.bytes.get(self.pos + 1) == Some(&b':')
                && let Some(class) = self.parse_posix_class()?
            {
                items.push(ClassItem::Posix(class));
                continue;
            }
            // PCRE: backslash escapes are live inside classes ([\d], [^\d], ...).
            // POSIX bracket expressions treat `\` as a literal member, so this
            // is gated on pcre mode.
            if byte == b'\\'
                && self.pcre()
                && let Some(next) = self.bytes.get(self.pos + 1).copied()
            {
                self.pos += 2;
                match next {
                    b'd' => items.push(ClassItem::Posix(PosixClass::Digit)),
                    b's' => items.push(ClassItem::Posix(PosixClass::Space)),
                    b'w' => {
                        items.push(ClassItem::Posix(PosixClass::Alnum));
                        items.push(ClassItem::Single(b'_'));
                    }
                    b'p' | b'P' => {
                        items.push(self.parse_category_escape(next == b'P')?);
                    }
                    b'x' => items.push(ClassItem::Single(self.parse_hex_escape()?)),
                    b't' => items.push(ClassItem::Single(b'\t')),
                    b'n' => items.push(ClassItem::Single(b'\n')),
                    other => items.push(ClassItem::Single(other)),
                }
                continue;
            }
            let lo = byte;
            if self.bytes.get(self.pos + 1) == Some(&b'-')
                && self.bytes.get(self.pos + 2).is_some_and(|c| *c != b']')
            {
                let hi = self.bytes[self.pos + 2];
                items.push(ClassItem::Range(lo, hi));
                self.pos += 3;
            } else {
                items.push(ClassItem::Single(lo));
                self.pos += 1;
            }
        }
        Ok(Node::Class { negate, items })
    }

    fn parse_posix_class(&mut self) -> Result<Option<PosixClass>> {
        let rest = &self.bytes[self.pos + 2..];
        let Some(end) = find_seq(rest, b":]") else {
            return Ok(None);
        };
        let name = &rest[..end];
        let class = match name {
            b"alpha" => PosixClass::Alpha,
            b"digit" => PosixClass::Digit,
            b"alnum" => PosixClass::Alnum,
            b"space" => PosixClass::Space,
            b"upper" => PosixClass::Upper,
            b"lower" => PosixClass::Lower,
            b"punct" => PosixClass::Punct,
            b"blank" => PosixClass::Blank,
            b"xdigit" => PosixClass::Xdigit,
            b"cntrl" => PosixClass::Cntrl,
            b"print" => PosixClass::Print,
            b"graph" => PosixClass::Graph,
            _ => return Ok(None),
        };
        self.pos += 2 + end + 2;
        Ok(Some(class))
    }

    fn parse_quantifier(&mut self, atom: Node) -> Result<Node> {
        let Some(byte) = self.peek() else {
            return Ok(atom);
        };
        let (min, max, consumed) = match byte {
            b'*' => (0, None, 1),
            b'+' if self.extended() => (1, None, 1),
            b'?' if self.extended() => (0, Some(1), 1),
            b'{' if self.extended() => match self.parse_bound(self.pos + 1, false)? {
                Some((min, max, end)) => (min, max, end - self.pos),
                None => return Ok(atom),
            },
            b'\\' if !self.extended() => {
                let next = self.bytes.get(self.pos + 1).copied();
                match next {
                    Some(b'+') => (1, None, 2),
                    Some(b'?') => (0, Some(1), 2),
                    Some(b'{') => match self.parse_bound(self.pos + 2, true)? {
                        Some((min, max, end)) => (min, max, end - self.pos),
                        None => return Ok(atom),
                    },
                    _ => return Ok(atom),
                }
            }
            _ => return Ok(atom),
        };
        self.pos += consumed;
        // PCRE lazy quantifiers: a trailing `?` flips the repeat to shortest-
        // match-first (`.*?`, `.+?`, `??`, `{n,m}?`).
        let mut greedy = true;
        if self.pcre() && self.peek() == Some(b'?') {
            self.pos += 1;
            greedy = false;
        }
        Ok(Node::Repeat {
            node: Box::new(atom),
            min,
            max,
            greedy,
        })
    }

    fn parse_bound(
        &self,
        start: usize,
        bre: bool,
    ) -> Result<Option<(usize, Option<usize>, usize)>> {
        let mut i = start;
        let mut min_digits = Vec::new();
        while let Some(c) = self.bytes.get(i).copied() {
            if c.is_ascii_digit() {
                min_digits.push(c);
                i += 1;
            } else {
                break;
            }
        }
        if min_digits.is_empty() {
            return Ok(None);
        }
        let min = parse_usize(&min_digits)?;
        let mut max = Some(min);
        if self.bytes.get(i) == Some(&b',') {
            i += 1;
            let mut max_digits = Vec::new();
            while let Some(c) = self.bytes.get(i).copied() {
                if c.is_ascii_digit() {
                    max_digits.push(c);
                    i += 1;
                } else {
                    break;
                }
            }
            max = if max_digits.is_empty() {
                None
            } else {
                Some(parse_usize(&max_digits)?)
            };
        }
        if bre {
            if self.bytes.get(i) == Some(&b'\\') && self.bytes.get(i + 1) == Some(&b'}') {
                return Ok(Some((min, max, i + 2)));
            }
        } else if self.bytes.get(i) == Some(&b'}') {
            return Ok(Some((min, max, i + 1)));
        }
        Ok(None)
    }
}

fn find_seq(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    (0..=haystack.len() - needle.len()).find(|&i| &haystack[i..i + needle.len()] == needle)
}

fn parse_usize(digits: &[u8]) -> Result<usize> {
    std::str::from_utf8(digits)
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .ok_or_else(|| GitError::Command("invalid repetition count in regex".into()))
}

// --- Regex matcher ---------------------------------------------------------

/// Per-match-attempt state: the subject text plus capture-group spans (PCRE
/// backreferences). Captures mutate during backtracking, hence the `RefCell`
/// (the continuation-passing matcher only holds `&` references).
struct MatchCtx<'a> {
    text: &'a [u8],
    captures: std::cell::RefCell<Vec<Option<(usize, usize)>>>,
}

impl<'a> MatchCtx<'a> {
    fn new(text: &'a [u8], num_groups: usize) -> Self {
        Self {
            text,
            captures: std::cell::RefCell::new(vec![None; num_groups + 1]),
        }
    }
}

fn match_node(root: &Node, ctx: &MatchCtx<'_>, pos: usize, ignore_case: bool) -> Option<usize> {
    match_seq(root, ctx, pos, ignore_case, &|p| Some(p))
}

fn match_anchored_full(root: &Node, ctx: &MatchCtx<'_>, ignore_case: bool) -> bool {
    match_seq(root, ctx, 0, ignore_case, &|p| {
        if p == ctx.text.len() { Some(p) } else { None }
    })
    .is_some()
}

fn match_seq(
    node: &Node,
    ctx: &MatchCtx<'_>,
    pos: usize,
    ignore_case: bool,
    cont: &dyn Fn(usize) -> Option<usize>,
) -> Option<usize> {
    let text = ctx.text;
    match node {
        Node::Empty => cont(pos),
        Node::Literal(byte) => {
            let c = text.get(pos)?;
            if byte_eq(*c, *byte, ignore_case) {
                cont(pos + 1)
            } else {
                None
            }
        }
        Node::AnyChar => {
            if pos < text.len() {
                cont(pos + 1)
            } else {
                None
            }
        }
        Node::Class { negate, items } => {
            let c = *text.get(pos)?;
            if class_matches(items, c, ignore_case) != *negate {
                cont(pos + 1)
            } else {
                None
            }
        }
        Node::StartAnchor => {
            if pos == 0 {
                cont(pos)
            } else {
                None
            }
        }
        Node::EndAnchor => {
            if pos == text.len() {
                cont(pos)
            } else {
                None
            }
        }
        Node::WordBoundary => {
            if is_word_boundary(text, pos) {
                cont(pos)
            } else {
                None
            }
        }
        Node::NonWordBoundary => {
            if !is_word_boundary(text, pos) {
                cont(pos)
            } else {
                None
            }
        }
        Node::Group(inner) => match_seq(inner, ctx, pos, ignore_case, cont),
        Node::IgnoreCase(inner) => match_seq(inner, ctx, pos, true, cont),
        Node::Capture(idx, inner) => {
            let start = pos;
            let idx = *idx;
            match_seq(inner, ctx, pos, ignore_case, &|p| {
                // Record the span for backreferences; restore on backtrack so a
                // failed continuation does not leak a stale span.
                let prev = ctx.captures.borrow()[idx];
                ctx.captures.borrow_mut()[idx] = Some((start, p));
                let result = cont(p);
                if result.is_none() {
                    ctx.captures.borrow_mut()[idx] = prev;
                }
                result
            })
        }
        Node::Backref(idx) => {
            // PCRE semantics: a backreference to an unset group fails to match.
            let span = ctx.captures.borrow()[*idx];
            let (start, end) = span?;
            let captured_len = end - start;
            if pos + captured_len > text.len() {
                return None;
            }
            let matches =
                (0..captured_len).all(|i| byte_eq(text[pos + i], text[start + i], ignore_case));
            if matches {
                cont(pos + captured_len)
            } else {
                None
            }
        }
        Node::Concat(nodes) => match_concat(nodes, ctx, pos, ignore_case, cont),
        Node::Alt(branches) => {
            for branch in branches {
                if let Some(end) = match_seq(branch, ctx, pos, ignore_case, cont) {
                    return Some(end);
                }
            }
            None
        }
        Node::Repeat {
            node,
            min,
            max,
            greedy,
        } => match_repeat(
            RepeatPattern {
                node,
                min: *min,
                max: *max,
                greedy: *greedy,
            },
            ctx,
            ignore_case,
            pos,
            cont,
        ),
    }
}

fn match_concat(
    nodes: &[Node],
    ctx: &MatchCtx<'_>,
    pos: usize,
    ignore_case: bool,
    cont: &dyn Fn(usize) -> Option<usize>,
) -> Option<usize> {
    match nodes.split_first() {
        None => cont(pos),
        Some((head, tail)) => match_seq(head, ctx, pos, ignore_case, &|p| {
            match_concat(tail, ctx, p, ignore_case, cont)
        }),
    }
}

struct RepeatPattern<'a> {
    node: &'a Node,
    min: usize,
    max: Option<usize>,
    greedy: bool,
}

fn match_repeat(
    repeat: RepeatPattern<'_>,
    ctx: &MatchCtx<'_>,
    ignore_case: bool,
    pos: usize,
    cont: &dyn Fn(usize) -> Option<usize>,
) -> Option<usize> {
    fn match_min(
        node: &Node,
        remaining: usize,
        ctx: &MatchCtx<'_>,
        pos: usize,
        ignore_case: bool,
        after_min: &dyn Fn(usize) -> Option<usize>,
    ) -> Option<usize> {
        if remaining == 0 {
            return after_min(pos);
        }
        match_seq(node, ctx, pos, ignore_case, &|p| {
            if p == pos {
                return after_min(p);
            }
            match_min(node, remaining - 1, ctx, p, ignore_case, after_min)
        })
    }

    /// Greedy: longest first — try one more iteration before yielding to the
    /// continuation.
    fn match_optional(
        node: &Node,
        remaining: Option<usize>,
        ctx: &MatchCtx<'_>,
        pos: usize,
        ignore_case: bool,
        cont: &dyn Fn(usize) -> Option<usize>,
    ) -> Option<usize> {
        if remaining == Some(0) {
            return cont(pos);
        }
        let next_remaining = remaining.map(|r| r - 1);
        let more = match_seq(node, ctx, pos, ignore_case, &|p| {
            if p == pos {
                None
            } else {
                match_optional(node, next_remaining, ctx, p, ignore_case, cont)
            }
        });
        if more.is_some() {
            return more;
        }
        cont(pos)
    }

    /// Lazy (`*?` etc.): shortest first — yield to the continuation before
    /// consuming another iteration.
    fn match_optional_lazy(
        node: &Node,
        remaining: Option<usize>,
        ctx: &MatchCtx<'_>,
        pos: usize,
        ignore_case: bool,
        cont: &dyn Fn(usize) -> Option<usize>,
    ) -> Option<usize> {
        if let Some(end) = cont(pos) {
            return Some(end);
        }
        if remaining == Some(0) {
            return None;
        }
        let next_remaining = remaining.map(|r| r - 1);
        match_seq(node, ctx, pos, ignore_case, &|p| {
            if p == pos {
                None
            } else {
                match_optional_lazy(node, next_remaining, ctx, p, ignore_case, cont)
            }
        })
    }

    let max_optional = repeat.max.map(|m| m.saturating_sub(repeat.min));
    match_min(repeat.node, repeat.min, ctx, pos, ignore_case, &|p| {
        if repeat.greedy {
            match_optional(repeat.node, max_optional, ctx, p, ignore_case, cont)
        } else {
            match_optional_lazy(repeat.node, max_optional, ctx, p, ignore_case, cont)
        }
    })
}

fn byte_eq(a: u8, b: u8, ignore_case: bool) -> bool {
    if ignore_case {
        a.eq_ignore_ascii_case(&b)
    } else {
        a == b
    }
}

fn class_matches(items: &[ClassItem], ch: u8, ignore_case: bool) -> bool {
    for item in items {
        match item {
            ClassItem::Single(b) => {
                if byte_eq(ch, *b, ignore_case) {
                    return true;
                }
            }
            ClassItem::Range(lo, hi) => {
                if (*lo..=*hi).contains(&ch) {
                    return true;
                }
                if ignore_case {
                    let lower = ch.to_ascii_lowercase();
                    let upper = ch.to_ascii_uppercase();
                    if (*lo..=*hi).contains(&lower) || (*lo..=*hi).contains(&upper) {
                        return true;
                    }
                }
            }
            ClassItem::Posix(class) => {
                if posix_matches(*class, ch) {
                    return true;
                }
            }
            ClassItem::Category { negate, cat } => {
                if perl_category_matches(*cat, ch) != *negate {
                    return true;
                }
            }
        }
    }
    false
}

/// ASCII projection of the Unicode general categories (`\p{..}`). The grep
/// engine is bytewise, so non-ASCII bytes are conservatively "no match".
fn perl_category_matches(cat: PerlCategory, ch: u8) -> bool {
    match cat {
        PerlCategory::Letter => ch.is_ascii_alphabetic(),
        PerlCategory::UppercaseLetter => ch.is_ascii_uppercase(),
        PerlCategory::LowercaseLetter => ch.is_ascii_lowercase(),
        PerlCategory::Number => ch.is_ascii_digit(),
        PerlCategory::Punctuation => {
            ch.is_ascii_punctuation()
                && !matches!(
                    ch,
                    b'$' | b'+' | b'<' | b'=' | b'>' | b'^' | b'`' | b'|' | b'~'
                )
        }
        PerlCategory::OpenPunctuation => matches!(ch, b'(' | b'[' | b'{'),
        PerlCategory::ClosePunctuation => matches!(ch, b')' | b']' | b'}'),
        PerlCategory::Symbol => matches!(
            ch,
            b'$' | b'+' | b'<' | b'=' | b'>' | b'^' | b'`' | b'|' | b'~'
        ),
        PerlCategory::Separator => ch == b' ',
    }
}

fn posix_matches(class: PosixClass, ch: u8) -> bool {
    match class {
        PosixClass::Alpha => ch.is_ascii_alphabetic(),
        PosixClass::Digit => ch.is_ascii_digit(),
        PosixClass::Alnum => ch.is_ascii_alphanumeric(),
        PosixClass::Space => ch.is_ascii_whitespace() || ch == 0x0b,
        PosixClass::Upper => ch.is_ascii_uppercase(),
        PosixClass::Lower => ch.is_ascii_lowercase(),
        PosixClass::Punct => ch.is_ascii_punctuation(),
        PosixClass::Blank => ch == b' ' || ch == b'\t',
        PosixClass::Xdigit => ch.is_ascii_hexdigit(),
        PosixClass::Cntrl => ch.is_ascii_control(),
        PosixClass::Print => ch.is_ascii_graphic() || ch == b' ',
        PosixClass::Graph => ch.is_ascii_graphic(),
    }
}

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn is_word_boundary(text: &[u8], pos: usize) -> bool {
    let before = pos
        .checked_sub(1)
        .and_then(|i| text.get(i))
        .copied()
        .map(is_word_byte)
        .unwrap_or(false);
    let after = text.get(pos).copied().map(is_word_byte).unwrap_or(false);
    before != after
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regex_diagnostic_default_stays_canonical() {
        assert_eq!(
            regex_diagnostic_message(
                RegexDiagnosticDetail::UnbalancedBrackets,
                RegexDiagnosticVerbosity::Default,
            ),
            INVALID_REGEX_MESSAGE
        );
    }

    #[test]
    fn regex_diagnostic_verbose_surfaces_bracket_detail() {
        assert_eq!(
            regex_diagnostic_message(
                RegexDiagnosticDetail::UnbalancedBrackets,
                RegexDiagnosticVerbosity::Verbose,
            ),
            UNBALANCED_BRACKETS_MESSAGE
        );
    }
}
