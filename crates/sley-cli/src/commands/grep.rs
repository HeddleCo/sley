//! `git grep` — search tracked content for a pattern.
//!
//! Searches the working tree by default, or the index (`--cached`) / one or more
//! tree-ishes (revisions named on the command line). Output matches Git's
//! `<path>:<line>:<text>` form (with a `<rev>:` prefix when searching a tree-ish),
//! and the common reporting flags (`-n`, `-l`, `-c`, `-i`, `-w`, `-v`, `-F`, ...).
//!
//! As with the other extracted commands, a glob of the crate root pulls in the
//! shared plumbing (`RepositoryContext`, `worktree_root_for_git_dir`,
//! `FileObjectDatabase`, ...);
//! see `commands::stash` for the rationale.

use crate::*;
use sley_object::TreeEntries;
use std::borrow::Cow;

/// How the regular expression text is interpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PatternKind {
    /// POSIX basic regular expressions (Git's default).
    Basic,
    /// POSIX extended regular expressions (`-E` / `--extended-regexp`).
    Extended,
    /// Fixed strings (`-F` / `--fixed-strings`).
    Fixed,
}

/// Mirror of git's `GREP_PATTERN_TYPE_*`. `Unspecified` means "fall back to
/// `extended_regexp_option`".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PatternTypeOption {
    Unspecified,
    Bre,
    Ere,
    Fixed,
    Pcre,
}

/// A token in the boolean grep expression (parsed from the argv stream).
#[derive(Clone)]
enum ExprToken {
    Pattern(usize), // index into `opts.patterns`
    And,
    Or,
    Not,
    Open,
    Close,
}

/// The parsed boolean expression tree (`-e A --and ( -e B --or --not -e C )`).
#[derive(Clone)]
enum Expr {
    /// Leaf: index into the compiled pattern list.
    Atom(usize),
    Not(Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
}

/// Parsed command-line options for `git grep`.
struct GrepOptions {
    patterns: Vec<String>,
    /// `-f`/`-e`/positional patterns recorded in argv order with boolean glue, so
    /// the expression tree can be reconstructed.
    tokens: Vec<ExprToken>,
    kind: PatternKind,
    ignore_case: bool,
    word: bool,
    line_regexp: bool,
    invert: bool,
    line_number: bool,
    column: bool,
    files_with_matches: bool,
    files_without_match: bool,
    count: bool,
    name_only_quiet: bool,
    show_filename: Option<bool>,
    only_matching: bool,
    text: bool,
    ignore_binary: bool,
    full_name: bool,
    null_data: bool,
    cached: bool,
    max_depth: Option<i64>,
    max_count: Option<i64>,
    before_context: usize,
    after_context: usize,
    show_function: bool,
    function_context: bool,
    revs: Vec<String>,
    pathspecs: Vec<String>,
}

impl GrepOptions {
    fn new() -> Self {
        Self {
            patterns: Vec::new(),
            tokens: Vec::new(),
            kind: PatternKind::Basic,
            ignore_case: false,
            word: false,
            line_regexp: false,
            invert: false,
            line_number: false,
            column: false,
            files_with_matches: false,
            files_without_match: false,
            count: false,
            name_only_quiet: false,
            show_filename: None,
            only_matching: false,
            text: false,
            ignore_binary: false,
            full_name: false,
            null_data: false,
            cached: false,
            max_depth: None,
            max_count: None,
            before_context: 0,
            after_context: 0,
            show_function: false,
            function_context: false,
            revs: Vec::new(),
            pathspecs: Vec::new(),
        }
    }

    fn push_pattern(&mut self, text: String) {
        // git splits a pattern containing newlines into several OR'd patterns
        // (`append_grep_pattern` is called per line of a multi-line `-e`/positional).
        if text.contains('\n') {
            for line in text.split('\n') {
                if line.is_empty() {
                    continue;
                }
                let idx = self.patterns.len();
                self.patterns.push(line.to_string());
                self.tokens.push(ExprToken::Pattern(idx));
            }
            return;
        }
        let idx = self.patterns.len();
        self.patterns.push(text);
        self.tokens.push(ExprToken::Pattern(idx));
    }
}

/// Resolve the effective pattern type from config (`grep.patternType`,
/// `grep.extendedRegexp`) plus the command-line override accumulated during argv
/// parsing. Config order is file entries first, then `-c` injected parameters;
/// `grep.patternType=default` resets to unspecified so a later
/// `grep.extendedRegexp` can take effect. The command-line flags (`-E/-G/-F/-P`)
/// override config entirely.
fn resolve_pattern_config(
    config: &GitConfig,
) -> Result<(PatternTypeOption, bool, Option<bool>, Option<bool>, Option<bool>)> {
    let mut pattern_type = PatternTypeOption::Unspecified;
    let mut extended = false;
    let mut linenumber: Option<bool> = None;
    let mut column: Option<bool> = None;
    let mut fullname: Option<bool> = None;

    let mut apply = |canonical_key: &str, value: Option<&str>| -> Result<()> {
        match canonical_key {
            "grep.patterntype" => {
                let v = value.unwrap_or("");
                pattern_type = parse_pattern_type_arg(v)?;
            }
            "grep.extendedregexp" => {
                extended = parse_grep_bool(value);
            }
            "grep.linenumber" => linenumber = Some(parse_grep_bool(value)),
            "grep.column" => column = Some(parse_grep_bool(value)),
            "grep.fullname" => fullname = Some(parse_grep_bool(value)),
            _ => {}
        }
        Ok(())
    };

    // 1. Config file entries, in file order.
    for section in &config.sections {
        if !section.name.eq_ignore_ascii_case("grep") {
            continue;
        }
        for entry in &section.entries {
            let canonical = format!("grep.{}", entry.key.to_ascii_lowercase());
            apply(&canonical, entry.value.as_deref())?;
        }
    }

    // 2. `-c` / GIT_CONFIG_PARAMETERS injected entries, in order.
    for param in injected_config_parameters()? {
        apply(&param.canonical_key.to_ascii_lowercase(), param.value.as_deref())?;
    }

    Ok((pattern_type, extended, linenumber, column, fullname))
}

fn parse_pattern_type_arg(arg: &str) -> Result<PatternTypeOption> {
    Ok(match arg {
        "default" => PatternTypeOption::Unspecified,
        "basic" => PatternTypeOption::Bre,
        "extended" => PatternTypeOption::Ere,
        "fixed" => PatternTypeOption::Fixed,
        "perl" => PatternTypeOption::Pcre,
        other => {
            eprintln!("fatal: bad grep.patternType argument: {other}");
            return Err(GitError::Exit(128));
        }
    })
}

/// git's `git_config_bool`: bare key -> true; true/yes/on/non-zero int -> true.
fn parse_grep_bool(value: Option<&str>) -> bool {
    match value {
        None => true,
        Some(v) => {
            let lower = v.to_ascii_lowercase();
            match lower.as_str() {
                "true" | "yes" | "on" => true,
                "false" | "no" | "off" | "" => false,
                other => other.parse::<i64>().map(|n| n != 0).unwrap_or(true),
            }
        }
    }
}

pub(crate) fn cmd_grep(args: &[String]) -> Result<()> {
    let mut opts = GrepOptions::new();
    // `positionals` is the post-option token stream (pattern, revs, paths). A `--`
    // among them is preserved as the literal marker `\0DD\0` so the later rev/path
    // scan can split on it exactly as git does.
    let mut positionals: Vec<String> = Vec::new();
    const DASHDASH: &str = "\u{0}DD\u{0}";
    let mut saw_double_dash = false;
    // Command-line pattern-type override: `-E/-G/-F/-P` set this (last wins).
    let mut cli_pattern_type: Option<PatternTypeOption> = None;
    let mut no_index = false;
    let mut iter = args.iter().peekable();

    while let Some(arg) = iter.next() {
        if saw_double_dash {
            // Option parsing has stopped; everything is a positional. Preserve a
            // literal `--` as the marker so the rev/path scan can split on it.
            if arg == "--" {
                positionals.push(DASHDASH.to_string());
            } else {
                positionals.push(arg.clone());
            }
            continue;
        }
        match arg.as_str() {
            "--" => saw_double_dash = true,
            "-e" => {
                let Some(value) = iter.next() else {
                    return grep_option_requires_value("-e");
                };
                opts.push_pattern(value.clone());
            }
            value if let Some(value) = value.strip_prefix("-e") => {
                opts.push_pattern(value.to_string());
            }
            value if let Some(value) = value.strip_prefix("--regexp=") => {
                opts.push_pattern(value.to_string());
            }
            "-f" => {
                let Some(file) = iter.next() else {
                    return grep_option_requires_value("-f");
                };
                load_pattern_file(file, &mut opts)?;
            }
            value if let Some(file) = value.strip_prefix("-f") => {
                load_pattern_file(file, &mut opts)?;
            }
            value if let Some(file) = value.strip_prefix("--file=") => {
                load_pattern_file(file, &mut opts)?;
            }
            "--and" => opts.tokens.push(ExprToken::And),
            "--or" => opts.tokens.push(ExprToken::Or),
            "--not" => opts.tokens.push(ExprToken::Not),
            "(" => opts.tokens.push(ExprToken::Open),
            ")" => opts.tokens.push(ExprToken::Close),
            "-E" | "--extended-regexp" => cli_pattern_type = Some(PatternTypeOption::Ere),
            "-G" | "--basic-regexp" => cli_pattern_type = Some(PatternTypeOption::Bre),
            "-F" | "--fixed-strings" => cli_pattern_type = Some(PatternTypeOption::Fixed),
            "-P" | "--perl-regexp" => cli_pattern_type = Some(PatternTypeOption::Pcre),
            "-i" | "--ignore-case" => opts.ignore_case = true,
            "--no-ignore-case" => opts.ignore_case = false,
            "-w" | "--word-regexp" => opts.word = true,
            "-x" | "--line-regexp" => opts.line_regexp = true,
            "-v" | "--invert-match" => opts.invert = true,
            "-n" | "--line-number" => opts.line_number = true,
            "--no-line-number" => opts.line_number = false,
            "--column" => opts.column = true,
            "--no-column" => opts.column = false,
            "-l" | "--files-with-matches" | "--name-only" => opts.files_with_matches = true,
            "-L" | "--files-without-match" => opts.files_without_match = true,
            "-c" | "--count" => opts.count = true,
            "-q" | "--quiet" => opts.name_only_quiet = true,
            "-o" | "--only-matching" => opts.only_matching = true,
            "-H" => opts.show_filename = Some(true),
            "-h" | "--no-filename" => opts.show_filename = Some(false),
            "--full-name" => opts.full_name = true,
            "--no-full-name" => opts.full_name = false,
            "-a" | "--text" => opts.text = true,
            "-I" => opts.ignore_binary = true,
            "-z" | "--null" => opts.null_data = true,
            "--cached" => opts.cached = true,
            "-p" | "--show-function" => opts.show_function = true,
            "-W" | "--function-context" => opts.function_context = true,
            "--no-index" => no_index = true,
            "-r" | "--recursive" => opts.max_depth = Some(-1),
            "--no-recursive" => opts.max_depth = Some(0),
            "--max-depth" => {
                let Some(value) = iter.next() else {
                    return grep_option_requires_value("--max-depth");
                };
                opts.max_depth = Some(parse_int_arg(value, "--max-depth")?);
            }
            value if let Some(v) = value.strip_prefix("--max-depth=") => {
                opts.max_depth = Some(parse_int_arg(v, "--max-depth")?);
            }
            "-m" | "--max-count" => {
                let Some(value) = iter.next() else {
                    return grep_option_requires_value("--max-count");
                };
                opts.max_count = Some(parse_int_arg(value, "--max-count")?);
            }
            value if let Some(v) = value.strip_prefix("--max-count=") => {
                opts.max_count = Some(parse_int_arg(v, "--max-count")?);
            }
            value if let Some(v) = value.strip_prefix("-m") => {
                opts.max_count = Some(parse_int_arg(v, "--max-count")?);
            }
            "-A" | "--after-context" => {
                let Some(value) = iter.next() else {
                    return grep_option_requires_value("--after-context");
                };
                opts.after_context = parse_context_arg(value)?;
            }
            value if let Some(v) = value.strip_prefix("--after-context=") => {
                opts.after_context = parse_context_arg(v)?;
            }
            value if let Some(v) = value.strip_prefix("-A") => {
                opts.after_context = parse_context_arg(v)?;
            }
            "-B" | "--before-context" => {
                let Some(value) = iter.next() else {
                    return grep_option_requires_value("--before-context");
                };
                opts.before_context = parse_context_arg(value)?;
            }
            value if let Some(v) = value.strip_prefix("--before-context=") => {
                opts.before_context = parse_context_arg(v)?;
            }
            value if let Some(v) = value.strip_prefix("-B") => {
                opts.before_context = parse_context_arg(v)?;
            }
            "-C" | "--context" => {
                let Some(value) = iter.next() else {
                    return grep_option_requires_value("--context");
                };
                let n = parse_context_arg(value)?;
                opts.before_context = n;
                opts.after_context = n;
            }
            value if let Some(v) = value.strip_prefix("--context=") => {
                let n = parse_context_arg(v)?;
                opts.before_context = n;
                opts.after_context = n;
            }
            value if let Some(v) = value.strip_prefix("-C") => {
                let n = parse_context_arg(v)?;
                opts.before_context = n;
                opts.after_context = n;
            }
            "--color"
            | "--no-color"
            | "--recurse-submodules"
            | "--no-recurse-submodules"
            | "--untracked"
            | "--no-exclude-standard"
            | "--exclude-standard"
            | "--heading"
            | "--no-heading"
            | "--break"
            | "--no-break" => {
                // Accepted-but-no-op flags whose default behaviour we already match.
            }
            "--threads" => {
                let _ = iter.next();
            }
            value if value.starts_with("--threads=") => {}
            value if value.starts_with("--color=") => {}
            value if value.starts_with('-')
                && value.len() > 1
                && !value.starts_with("--")
                && !is_negative_number(value) =>
            {
                match expand_short_flag_bundle(value, &mut opts, &mut cli_pattern_type)? {
                    ShortBundle::Handled => {}
                    ShortBundle::Unknown(flag) => {
                        return grep_unknown_option(&flag);
                    }
                }
            }
            value if value.starts_with("--") => {
                return grep_unknown_option(value.trim_start_matches('-'));
            }
            other => positionals.push(other.to_string()),
        }
    }

    let have_pattern = !opts.patterns.is_empty() || !opts.tokens.is_empty();
    // git: "skip a -- separator; we know it cannot be separating revisions from
    // pathnames if we haven't even had any patterns yet."
    if !have_pattern && positionals.first().map(String::as_str) == Some(DASHDASH) {
        positionals.remove(0);
    }
    // The very first positional is the pattern unless one was supplied via
    // `-e`/`-f`/boolean tokens.
    if !have_pattern {
        if positionals.is_empty() {
            eprintln!("fatal: no pattern given");
            return Err(GitError::Exit(128));
        }
        opts.push_pattern(positionals.remove(0));
    }

    if no_index {
        return Err(GitError::Unsupported("grep --no-index".into()));
    }

    let repo = RepositoryContext::discover_current()?;
    let cwd = repo.cwd();
    let git_dir = repo.git_dir();
    let format = repo.format();
    let db = repo.objects();

    // Resolve config-driven pattern type + display defaults, then the CLI
    // override (`-E/-G/-F/-P` win over config).
    let (mut pattern_type, extended, cfg_linenumber, cfg_column, cfg_fullname) =
        resolve_pattern_config(repo.config())?;
    if let Some(cli) = cli_pattern_type {
        pattern_type = cli;
    }
    if pattern_type == PatternTypeOption::Unspecified {
        pattern_type = if extended {
            PatternTypeOption::Ere
        } else {
            PatternTypeOption::Bre
        };
    }
    if pattern_type == PatternTypeOption::Pcre {
        eprintln!(
            "fatal: cannot use Perl-compatible regexes; sley was not built with PCRE support"
        );
        return Err(GitError::Exit(128));
    }
    opts.kind = match pattern_type {
        PatternTypeOption::Ere => PatternKind::Extended,
        PatternTypeOption::Fixed => PatternKind::Fixed,
        _ => PatternKind::Basic,
    };
    // `grep.*` config defaults apply only when the matching CLI flag was absent.
    if let Some(v) = cfg_linenumber {
        // CLI `-n`/`--no-line-number` already toggled opts.line_number; config is
        // the default, so only apply it if no CLI flag changed it. We approximate
        // by treating any CLI `-n` as overriding (git: linenum set by config, then
        // OPT_NEGBIT for -n overrides). Since opts.line_number starts false and CLI
        // sets it, prefer config only when CLI left it default-false AND config true.
        if !opts.line_number {
            opts.line_number = v;
        }
    }
    if let Some(v) = cfg_column {
        if !opts.column {
            opts.column = v;
        }
    }
    if let Some(v) = cfg_fullname {
        if !opts.full_name {
            opts.full_name = v;
        }
    }

    // Disambiguate remaining positionals into revs and pathspecs, mirroring git:
    // if a `--` is present, everything before it must resolve as a rev and
    // everything after is a path; otherwise stop at the first non-rev and treat
    // the rest as paths.
    let has_dashdash = positionals.iter().any(|p| p == DASHDASH);
    let mut in_paths = false;
    for value in positionals {
        if value == DASHDASH {
            in_paths = true;
            continue;
        }
        if in_paths {
            opts.pathspecs.push(value);
            continue;
        }
        if has_dashdash {
            // Up to the `--`, every token is taken as a rev.
            opts.revs.push(value);
        } else {
            match repo.resolve_revision(&value) {
                Ok(_) => opts.revs.push(value),
                Err(_) => {
                    in_paths = true;
                    opts.pathspecs.push(value);
                }
            }
        }
    }

    let matcher = GrepMatcher::compile(&opts)?;
    let expr = build_expr(&opts.tokens);

    let worktree_root = match worktree_root_for_git_dir(git_dir) {
        Ok(root) if root.is_dir() => Some(root),
        _ => None,
    };
    let pathspec = GrepPathspec::new(worktree_root.as_deref(), cwd, opts.full_name, &opts.pathspecs)?;

    let plan = GrepPlan {
        matcher: &matcher,
        expr: expr.as_ref(),
        opts: &opts,
        pathspec: &pathspec,
    };

    let mut any_match = false;
    let mut out = io::stdout();
    if opts.revs.is_empty() {
        let Some(worktree_root) = worktree_root.as_deref() else {
            return Err(GitError::Command("grep: missing worktree".into()));
        };
        any_match = grep_index_source(
            GrepIndexSource {
                git_dir,
                worktree_root,
                format,
                db,
            },
            &plan,
            &mut out,
        )?;
    } else {
        for rev in &opts.revs {
            let oid = repo.resolve_revision(rev)?;
            let tree_oid = sley_rev::peel_to_tree(db, format, &oid)?;
            let matched = grep_tree_source(
                GrepTreeSource {
                    db,
                    format,
                    tree_oid: &tree_oid,
                    rev,
                },
                &plan,
                &mut out,
            )?;
            any_match = any_match || matched;
        }
    }

    pathspec.report_unmatched()?;

    if any_match {
        Ok(())
    } else {
        Err(GitError::Exit(1))
    }
}

/// Bundle of the matcher + expression + options threaded through the source
/// iterators (keeps function arities small).
struct GrepPlan<'a> {
    matcher: &'a GrepMatcher,
    expr: Option<&'a Expr>,
    opts: &'a GrepOptions,
    pathspec: &'a GrepPathspec,
}

fn is_negative_number(value: &str) -> bool {
    value.len() > 1 && value.starts_with('-') && value[1..].bytes().all(|b| b.is_ascii_digit())
}

fn parse_int_arg(value: &str, flag: &str) -> Result<i64> {
    value.parse::<i64>().map_err(|_| {
        eprintln!("fatal: invalid number for '{flag}': {value}");
        GitError::Exit(128)
    })
}

fn parse_context_arg(value: &str) -> Result<usize> {
    let n: i64 = value.parse().map_err(|_| {
        eprintln!("fatal: invalid context length argument: {value}");
        GitError::Exit(128)
    })?;
    Ok(n.max(0) as usize)
}

/// Load patterns from a `-f` file (or `-f -` = stdin); each non-empty line is a
/// separate `-e`-style pattern, joined into the OR-list. Empty lines are dropped.
fn load_pattern_file(file: &str, opts: &mut GrepOptions) -> Result<()> {
    let raw = if file == "-" {
        let mut buf = Vec::new();
        io::stdin().read_to_end(&mut buf)?;
        buf
    } else {
        // git reads `-f` relative to the cwd.
        match fs::read(file) {
            Ok(bytes) => bytes,
            Err(_) => {
                eprintln!("fatal: cannot open '{file}'");
                return Err(GitError::Exit(128));
            }
        }
    };
    for line in raw.split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        opts.push_pattern(String::from_utf8_lossy(line).into_owned());
    }
    Ok(())
}

/// Build the boolean expression tree from the token stream, or `None` when the
/// patterns are a plain OR-list (no `--and`/`--or`/`--not`/`(`).
fn build_expr(tokens: &[ExprToken]) -> Option<Expr> {
    let has_boolean = tokens.iter().any(|t| {
        matches!(
            t,
            ExprToken::And | ExprToken::Or | ExprToken::Not | ExprToken::Open | ExprToken::Close
        )
    });
    if !has_boolean {
        return None;
    }
    let mut parser = ExprParser { tokens, pos: 0 };
    parser.parse_or()
}

struct ExprParser<'a> {
    tokens: &'a [ExprToken],
    pos: usize,
}

impl ExprParser<'_> {
    fn peek(&self) -> Option<&ExprToken> {
        self.tokens.get(self.pos)
    }

    fn parse_or(&mut self) -> Option<Expr> {
        let mut left = self.parse_and()?;
        while matches!(self.peek(), Some(ExprToken::Or)) {
            self.pos += 1;
            let right = self.parse_and()?;
            left = Expr::Or(Box::new(left), Box::new(right));
        }
        Some(left)
    }

    fn parse_and(&mut self) -> Option<Expr> {
        let mut left = self.parse_unary()?;
        loop {
            match self.peek() {
                Some(ExprToken::And) => {
                    self.pos += 1;
                    let right = self.parse_unary()?;
                    left = Expr::And(Box::new(left), Box::new(right));
                }
                // Implicit AND between adjacent atoms (git treats `-e A -e B` in
                // expression context as A AND B once any boolean token appears).
                Some(ExprToken::Pattern(_)) | Some(ExprToken::Not) | Some(ExprToken::Open) => {
                    let right = self.parse_unary()?;
                    left = Expr::And(Box::new(left), Box::new(right));
                }
                _ => break,
            }
        }
        Some(left)
    }

    fn parse_unary(&mut self) -> Option<Expr> {
        match self.peek() {
            Some(ExprToken::Not) => {
                self.pos += 1;
                let inner = self.parse_unary()?;
                Some(Expr::Not(Box::new(inner)))
            }
            Some(ExprToken::Open) => {
                self.pos += 1;
                let inner = self.parse_or()?;
                if matches!(self.peek(), Some(ExprToken::Close)) {
                    self.pos += 1;
                }
                Some(inner)
            }
            Some(ExprToken::Pattern(idx)) => {
                let idx = *idx;
                self.pos += 1;
                Some(Expr::Atom(idx))
            }
            _ => None,
        }
    }
}

/// Result of expanding a `-abc` short-flag bundle.
enum ShortBundle {
    Handled,
    Unknown(String),
}

/// Expands a bundle of single-character flags (e.g. `-iwn`). Stops and treats
/// the remainder as a glued value for value-taking flags (`-e`/`-f`).
fn expand_short_flag_bundle(
    bundle: &str,
    opts: &mut GrepOptions,
    cli_pattern_type: &mut Option<PatternTypeOption>,
) -> Result<ShortBundle> {
    let chars: Vec<char> = bundle.chars().collect();
    let mut idx = 1;
    while idx < chars.len() {
        let ch = chars[idx];
        match ch {
            'e' => {
                let rest: String = chars[idx + 1..].iter().collect();
                if rest.is_empty() {
                    grep_option_requires_value("-e")?;
                }
                opts.push_pattern(rest);
                return Ok(ShortBundle::Handled);
            }
            'f' => {
                let rest: String = chars[idx + 1..].iter().collect();
                if rest.is_empty() {
                    grep_option_requires_value("-f")?;
                }
                load_pattern_file(&rest, opts)?;
                return Ok(ShortBundle::Handled);
            }
            'E' => *cli_pattern_type = Some(PatternTypeOption::Ere),
            'G' => *cli_pattern_type = Some(PatternTypeOption::Bre),
            'F' => *cli_pattern_type = Some(PatternTypeOption::Fixed),
            'P' => *cli_pattern_type = Some(PatternTypeOption::Pcre),
            'i' => opts.ignore_case = true,
            'w' => opts.word = true,
            'x' => opts.line_regexp = true,
            'v' => opts.invert = true,
            'n' => opts.line_number = true,
            'l' => opts.files_with_matches = true,
            'L' => opts.files_without_match = true,
            'c' => opts.count = true,
            'q' => opts.name_only_quiet = true,
            'o' => opts.only_matching = true,
            'H' => opts.show_filename = Some(true),
            'h' => opts.show_filename = Some(false),
            'a' => opts.text = true,
            'I' => opts.ignore_binary = true,
            'z' => opts.null_data = true,
            'p' => opts.show_function = true,
            'W' => opts.function_context = true,
            'r' => opts.max_depth = Some(-1),
            other => return Ok(ShortBundle::Unknown(other.to_string())),
        }
        idx += 1;
    }
    Ok(ShortBundle::Handled)
}

fn grep_option_requires_value(flag: &str) -> Result<()> {
    eprintln!(
        "fatal: switch `{}' requires a value",
        flag.trim_start_matches('-')
    );
    Err(GitError::Exit(128))
}

fn grep_unknown_option(flag: &str) -> Result<()> {
    eprintln!("error: unknown option `{flag}'");
    eprintln!("usage: git grep [<options>] [-e] <pattern> [<rev>...] [[--] <path>...]");
    Err(GitError::Exit(128))
}

// ---------------------------------------------------------------------------
// Source iteration
// ---------------------------------------------------------------------------

/// Greps the working tree (default) or the index (`--cached`).
struct GrepIndexSource<'a> {
    git_dir: &'a Path,
    worktree_root: &'a Path,
    format: ObjectFormat,
    db: &'a FileObjectDatabase,
}

fn grep_index_source(
    source: GrepIndexSource<'_>,
    plan: &GrepPlan<'_>,
    out: &mut impl Write,
) -> Result<bool> {
    let Some(index) = sley_worktree::read_repository_index(source.git_dir, source.format)? else {
        return Ok(false);
    };
    let mut any = false;
    let mut printed_file = false;
    for entry in &index.entries {
        if (entry.flags >> 12) & 0x3 != 0 {
            continue;
        }
        if entry.mode == 0o160000 {
            continue;
        }
        let path = &entry.path;
        if !plan.pathspec.matches(path) {
            continue;
        }
        if !plan.pathspec.within_max_depth(path, plan.opts.max_depth) {
            continue;
        }
        // CE_VALID (assume-unchanged) and intent-to-add entries: for a working-tree
        // search git falls back to the worktree file. When the worktree file is
        // gone but the entry has CE_VALID, git uses the cached blob.
        let content: Cow<'_, [u8]> = if plan.opts.cached {
            let object = source.db.read_object(&entry.oid)?;
            Cow::Owned(object.body.to_vec())
        } else {
            let absolute = source.worktree_root.join(bytes_to_path(path));
            match fs::read(&absolute) {
                Ok(bytes) => Cow::Owned(bytes),
                Err(_) => {
                    // CE_VALID (assume-unchanged) falls back to the indexed blob
                    // when the worktree file is gone.
                    if (entry.flags & 0x8000) != 0 {
                        let object = source.db.read_object(&entry.oid)?;
                        Cow::Owned(object.body.to_vec())
                    } else {
                        continue;
                    }
                }
            }
        };
        let display = plan.pathspec.display(path);
        let matched = grep_buffer(&content, &display, None, plan, out, &mut printed_file)?;
        any = any || matched;
    }
    Ok(any)
}

/// Greps a tree-ish, recursing through subtrees.
struct GrepTreeSource<'a> {
    db: &'a FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &'a ObjectId,
    rev: &'a str,
}

fn grep_tree_source(
    source: GrepTreeSource<'_>,
    plan: &GrepPlan<'_>,
    out: &mut impl Write,
) -> Result<bool> {
    let mut entries: Vec<(Vec<u8>, ObjectId)> = Vec::new();
    collect_tree_blobs(
        source.db,
        source.format,
        source.tree_oid,
        &mut Vec::new(),
        &mut entries,
    )?;
    let mut any = false;
    let mut printed_file = false;
    for (path, oid) in entries {
        if !plan.pathspec.matches(&path) {
            continue;
        }
        if !plan.pathspec.within_max_depth(&path, plan.opts.max_depth) {
            continue;
        }
        let display = plan.pathspec.display(&path);
        let object = source.db.read_object(&oid)?;
        let content = &object.body;
        let matched = grep_buffer(content, &display, Some(source.rev), plan, out, &mut printed_file)?;
        any = any || matched;
    }
    Ok(any)
}

fn collect_tree_blobs(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    prefix: &mut Vec<u8>,
    out: &mut Vec<(Vec<u8>, ObjectId)>,
) -> Result<()> {
    let object = db.read_object(tree_oid)?;
    if object.object_type != ObjectType::Tree {
        return Ok(());
    }
    for entry in TreeEntries::new(format, &object.body) {
        let entry = entry?;
        let object_type = tree_entry_object_type(entry.mode);
        let base_len = prefix.len();
        if !prefix.is_empty() {
            prefix.push(b'/');
        }
        prefix.extend_from_slice(entry.name);
        match object_type {
            ObjectType::Tree => {
                collect_tree_blobs(db, format, &entry.oid, prefix, out)?;
            }
            ObjectType::Blob if entry.mode != 0o160000 => {
                out.push((prefix.clone(), entry.oid));
            }
            _ => {}
        }
        prefix.truncate(base_len);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Per-file matching and output
// ---------------------------------------------------------------------------

/// One matched line plus its column (1-based byte column of the leftmost match,
/// or 0 when no positive leaf matched — e.g. an inverted/NOT result).
struct LineHit {
    line_no: usize,
    column: usize,
}

fn grep_buffer(
    content: &[u8],
    display_path: &[u8],
    rev: Option<&str>,
    plan: &GrepPlan<'_>,
    out: &mut impl Write,
    printed_file: &mut bool,
) -> Result<bool> {
    let opts = plan.opts;
    let is_binary = !opts.text && buffer_is_binary(content);
    if is_binary && opts.ignore_binary {
        return Ok(false);
    }

    let lines: Vec<&[u8]> = split_lines(content, b'\n').collect();

    // Evaluate each line: matched? and at what column.
    let mut hits: Vec<LineHit> = Vec::new();
    let max = opts.max_count.unwrap_or(-1);
    for (i, line) in lines.iter().enumerate() {
        // max_count 0 => never match (and exit non-zero); negative => no limit.
        if max == 0 {
            break;
        }
        let (matched, col, icol) = eval_line(plan, line);
        if matched != opts.invert {
            // git: cno = invert ? icol : col; a missing match (None) prints as 1.
            let chosen = if opts.invert { icol } else { col };
            let column = chosen.map(|c| c + 1).unwrap_or(1);
            hits.push(LineHit {
                line_no: i + 1,
                column,
            });
            if max > 0 && hits.len() as i64 >= max {
                break;
            }
        }
    }
    let any = !hits.is_empty();

    if opts.name_only_quiet {
        return Ok(any);
    }

    let show_filename = opts.show_filename.unwrap_or(true);
    let field_sep: &[u8] = if opts.null_data { b"\0" } else { b":" };
    let list_term = if opts.null_data { b'\0' } else { b'\n' };

    if opts.count {
        if any {
            // `-h` suppresses the whole path prefix (including the rev), printing
            // just the count.
            if show_filename {
                if let Some(rev) = rev {
                    out.write_all(rev.as_bytes())?;
                    out.write_all(b":")?;
                }
                write_quoted_path(out, display_path, opts.null_data)?;
                out.write_all(field_sep)?;
            }
            out.write_all(hits.len().to_string().as_bytes())?;
            out.write_all(b"\n")?;
        }
        return Ok(any);
    }

    if opts.files_with_matches {
        if any {
            write_path_line(out, rev, display_path, list_term, opts.null_data)?;
        }
        return Ok(any);
    }

    if opts.files_without_match {
        if !any {
            write_path_line(out, rev, display_path, list_term, opts.null_data)?;
        }
        return Ok(any);
    }

    if !any {
        return Ok(false);
    }

    if is_binary {
        out.write_all(b"Binary file ")?;
        if let Some(rev) = rev {
            out.write_all(rev.as_bytes())?;
            out.write_all(b":")?;
        }
        out.write_all(display_path)?;
        out.write_all(b" matches\n")?;
        return Ok(true);
    }

    if opts.only_matching {
        for hit in &hits {
            let line = lines[hit.line_no - 1];
            let spans = plan.matcher.match_spans_expr(plan.expr, line);
            // git advances `cno` cumulatively: it starts at the 1-based offset of
            // the first match, then after each match adds that match's end offset
            // measured from the running `bol` (`cno += rm_eo; bol += rm_eo`). We
            // mirror that with an explicit moving `bol`.
            let mut cno = spans.first().map(|s| s.0 + 1).unwrap_or(0);
            let mut bol = 0usize;
            for (i, span) in spans.iter().enumerate() {
                if i > 0 {
                    // Add the previous match's rm_eo (relative to the prior bol).
                    let prev = spans[i - 1];
                    cno += prev.1 - bol;
                    bol = prev.1;
                }
                write_match_prefix(out, rev, display_path, show_filename, field_sep)?;
                if opts.line_number {
                    out.write_all(hit.line_no.to_string().as_bytes())?;
                    out.write_all(field_sep)?;
                }
                if opts.column {
                    out.write_all(cno.to_string().as_bytes())?;
                    out.write_all(field_sep)?;
                }
                out.write_all(&line[span.0..span.1])?;
                out.write_all(b"\n")?;
            }
        }
        return Ok(true);
    }

    // Determine the set of lines to print, including context and function context.
    let to_print = compute_output_lines(&lines, &hits, opts);
    emit_lines(
        out,
        &lines,
        &to_print,
        &hits,
        rev,
        display_path,
        show_filename,
        field_sep,
        opts,
        printed_file,
    )?;
    Ok(true)
}

/// Evaluate whether a line matches. Returns `(matched, col, icol)` where `col` is
/// the leftmost positive-leaf match offset and `icol` the leftmost negated-leaf
/// offset (both 0-based, `None` when absent). git prints `(invert ? icol : col)`
/// + 1, defaulting to 1 when absent.
fn eval_line(plan: &GrepPlan<'_>, line: &[u8]) -> (bool, Option<usize>, Option<usize>) {
    match plan.expr {
        Some(expr) => {
            let mut col: Option<usize> = None;
            let mut icol: Option<usize> = None;
            let matched = eval_expr(plan.matcher, expr, line, &mut col, &mut icol);
            (matched, col, icol)
        }
        None => {
            // Plain OR-list: matched if any pattern matches; col = leftmost. No
            // negated leaves, so icol stays None.
            let mut col: Option<usize> = None;
            let mut matched = false;
            for idx in 0..plan.matcher.patterns.len() {
                if let Some((s, _)) = plan.matcher.find_idx(idx, line, 0) {
                    matched = true;
                    col = Some(col.map_or(s, |c: usize| c.min(s)));
                    if !plan.opts.column {
                        break;
                    }
                }
            }
            (matched, col, None)
        }
    }
}

/// Evaluate a boolean expression, threading `col` (positive-leaf leftmost match)
/// and `icol` (negated-leaf leftmost match). A `--not` swaps the two on the way
/// down, mirroring git's `match_expr_eval`.
fn eval_expr(
    matcher: &GrepMatcher,
    expr: &Expr,
    line: &[u8],
    col: &mut Option<usize>,
    icol: &mut Option<usize>,
) -> bool {
    match expr {
        Expr::Atom(idx) => {
            let found = matcher.find_idx(*idx, line, 0);
            if let Some((s, _)) = found {
                *col = Some(col.map_or(s, |c| c.min(s)));
            }
            found.is_some()
        }
        Expr::Not(inner) => {
            // Swap col/icol for the negated subtree.
            !eval_expr(matcher, inner, line, icol, col)
        }
        Expr::And(l, r) => {
            // git does not short-circuit AND under --column. Evaluate both.
            let lh = eval_expr(matcher, l, line, col, icol);
            let rh = eval_expr(matcher, r, line, col, icol);
            lh && rh
        }
        Expr::Or(l, r) => {
            let lh = eval_expr(matcher, l, line, col, icol);
            let rh = eval_expr(matcher, r, line, col, icol);
            lh || rh
        }
    }
}

/// Bit flags for which lines to print and how.
#[derive(Clone, Copy, Default)]
struct LineFlag {
    selected: bool, // matched line (prints with `:`)
    context: bool,  // context line (prints with `-`)
    function: bool, // function header line (prints with `=`)
}

/// Compute, for each input line, whether it is selected/context/function.
fn compute_output_lines(
    lines: &[&[u8]],
    hits: &[LineHit],
    opts: &GrepOptions,
) -> Vec<LineFlag> {
    let mut flags = vec![LineFlag::default(); lines.len()];
    for hit in hits {
        flags[hit.line_no - 1].selected = true;
    }
    // -A/-B/-C context.
    if opts.before_context > 0 || opts.after_context > 0 {
        for hit in hits {
            let center = hit.line_no - 1;
            let start = center.saturating_sub(opts.before_context);
            let end = (center + opts.after_context).min(lines.len() - 1);
            for line in flags.iter_mut().take(end + 1).skip(start) {
                if !line.selected {
                    line.context = true;
                }
            }
        }
    }
    // -W function context: extend each match to its whole function body and add
    // the function header line marked with `=`.
    if opts.function_context {
        for hit in hits {
            let center = hit.line_no - 1;
            let (header, end, header_is_func) = function_bounds(lines, center);
            for (i, line) in flags.iter_mut().enumerate().take(end + 1) {
                if i >= header && i <= end && !line.selected {
                    line.context = true;
                }
            }
            // Only mark the header with `=` when it is a real funcname line
            // (a bare include/preamble block above the match shows as `-`).
            if header_is_func && header < center && !flags[header].selected {
                flags[header].context = false;
                flags[header].function = true;
            }
        }
    } else if opts.show_function {
        // -p: prepend the enclosing function header (a `=` line) per hunk.
        for hit in hits {
            let center = hit.line_no - 1;
            if let Some(header) = enclosing_function(lines, center) {
                if !flags[header].selected {
                    flags[header].function = true;
                }
            }
        }
    }
    flags
}

/// A line is a "function" header under git's default `match_funcname`: a
/// non-empty line whose first byte is a letter, `_`, or `$`.
fn is_funcline(line: &[u8]) -> bool {
    match line.first() {
        None => false,
        Some(&b) => b.is_ascii_alphabetic() || b == b'_' || b == b'$',
    }
}

/// Find the function header line at or above `from`.
fn enclosing_function(lines: &[&[u8]], from: usize) -> Option<usize> {
    (0..=from).rev().find(|&i| is_funcline(lines[i]))
}

/// For -W: return `(function_header_line, last_line_of_function, header_is_func)`
/// bracketing `from`. When no funcname line precedes the match, the body extends
/// to the top of the file and `header_is_func` is false.
fn function_bounds(lines: &[&[u8]], from: usize) -> (usize, usize, bool) {
    let (header, header_is_func) = match enclosing_function(lines, from) {
        Some(h) => (h, true),
        None => (0, false),
    };
    // The function ends just before the next function header.
    let mut end = lines.len() - 1;
    for i in (header + 1)..lines.len() {
        if is_funcline(lines[i]) {
            end = i - 1;
            break;
        }
    }
    // Trim trailing empty lines (git: "Trailing empty lines are not interesting").
    while end > from && lines[end].is_empty() {
        end -= 1;
    }
    (header, end, header_is_func)
}

#[allow(clippy::too_many_arguments)]
fn emit_lines(
    out: &mut impl Write,
    lines: &[&[u8]],
    flags: &[LineFlag],
    hits: &[LineHit],
    rev: Option<&str>,
    display_path: &[u8],
    show_filename: bool,
    field_sep: &[u8],
    opts: &GrepOptions,
    printed_file: &mut bool,
) -> Result<()> {
    // Column lookup per selected line.
    let col_of = |line_no: usize| -> usize {
        hits.iter()
            .find(|h| h.line_no == line_no)
            .map(|h| h.column)
            .unwrap_or(0)
    };

    let has_context = opts.before_context > 0
        || opts.after_context > 0
        || opts.function_context;

    let mut last_printed: Option<usize> = None;
    for (i, flag) in flags.iter().enumerate() {
        if !(flag.selected || flag.context || flag.function) {
            continue;
        }
        // Hunk separator `--` between non-adjacent printed groups (only with
        // context, matching git).
        if has_context {
            if let Some(prev) = last_printed {
                if i > prev + 1 {
                    if *printed_file {
                        // Between files git also prints `--`; handled by the gap too.
                    }
                    out.write_all(b"--\n")?;
                }
            } else if *printed_file {
                out.write_all(b"--\n")?;
            }
        }
        let sep: &[u8] = if flag.selected {
            field_sep
        } else if flag.function {
            b"="
        } else {
            b"-"
        };
        write_match_prefix_sep(out, rev, display_path, show_filename, sep)?;
        if opts.line_number {
            out.write_all((i + 1).to_string().as_bytes())?;
            out.write_all(sep)?;
        }
        if opts.column && flag.selected {
            let c = col_of(i + 1);
            out.write_all(c.to_string().as_bytes())?;
            out.write_all(field_sep)?;
        }
        out.write_all(lines[i])?;
        out.write_all(b"\n")?;
        last_printed = Some(i);
    }
    *printed_file = true;
    Ok(())
}

/// Writes the `<rev>:<path><sep>` prefix for a matched/context line.
fn write_match_prefix(
    out: &mut impl Write,
    rev: Option<&str>,
    display_path: &[u8],
    show_filename: bool,
    field_sep: &[u8],
) -> Result<()> {
    write_match_prefix_sep(out, rev, display_path, show_filename, field_sep)
}

fn write_match_prefix_sep(
    out: &mut impl Write,
    rev: Option<&str>,
    display_path: &[u8],
    show_filename: bool,
    sep: &[u8],
) -> Result<()> {
    if let Some(rev) = rev {
        out.write_all(rev.as_bytes())?;
        out.write_all(b":")?;
    }
    if show_filename || rev.is_some() {
        // The path field is quoted; the `-z` (null) form writes raw bytes.
        let raw = sep == b"\0";
        write_quoted_path(out, display_path, raw)?;
        out.write_all(sep)?;
    }
    Ok(())
}

/// Writes a bare `<rev>:<path>` line (for `-l`/`-L`), quoted unless `-z`.
fn write_path_line(
    out: &mut impl Write,
    rev: Option<&str>,
    display_path: &[u8],
    line_sep: u8,
    raw: bool,
) -> Result<()> {
    if let Some(rev) = rev {
        out.write_all(rev.as_bytes())?;
        out.write_all(b":")?;
    }
    write_quoted_path(out, display_path, raw)?;
    out.write_all(&[line_sep])?;
    Ok(())
}

/// Writes a path, C-style quoted (git's `quote_path`) unless `raw` (`-z`).
fn write_quoted_path(out: &mut impl Write, path: &[u8], raw: bool) -> Result<()> {
    if raw {
        out.write_all(path)?;
    } else {
        out.write_all(status_quote_path(path, false).as_bytes())?;
    }
    Ok(())
}

/// Splits a buffer into lines on `sep`, dropping a single trailing separator so
/// that a final newline does not yield an empty trailing line.
fn split_lines(content: &[u8], sep: u8) -> impl Iterator<Item = &[u8]> {
    let trimmed = if content.last() == Some(&sep) {
        &content[..content.len() - 1]
    } else {
        content
    };
    SplitLines {
        rest: trimmed,
        sep,
        done: trimmed.is_empty() && content.is_empty(),
        empty_input: content.is_empty(),
    }
}

struct SplitLines<'a> {
    rest: &'a [u8],
    sep: u8,
    done: bool,
    empty_input: bool,
}

impl<'a> Iterator for SplitLines<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        if self.empty_input {
            self.done = true;
            return None;
        }
        match self.rest.iter().position(|byte| *byte == self.sep) {
            Some(pos) => {
                let line = &self.rest[..pos];
                self.rest = &self.rest[pos + 1..];
                Some(line)
            }
            None => {
                self.done = true;
                Some(self.rest)
            }
        }
    }
}

/// Git's binary-file heuristic: a NUL byte in the first 8000 bytes.
fn buffer_is_binary(content: &[u8]) -> bool {
    let window = content.len().min(8000);
    content[..window].contains(&0)
}

fn bytes_to_path(bytes: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
    }
}

// ---------------------------------------------------------------------------
// Pathspec limiting (with cwd-relative display)
// ---------------------------------------------------------------------------

struct GrepPathFilter {
    original: String,
    normalized: Vec<u8>,
    /// Whether this pathspec is a bare directory-restricting spec (used by
    /// `--max-depth`, where depth is measured relative to the spec's directory).
    is_dir_spec: bool,
    matched: Cell<bool>,
}

struct GrepPathspec {
    prefix: Vec<u8>,
    cwd_depth: usize,
    full_name: bool,
    filters: Vec<GrepPathFilter>,
}

impl GrepPathspec {
    fn new(
        worktree_root: Option<&Path>,
        cwd: &Path,
        full_name: bool,
        pathspecs: &[String],
    ) -> Result<Self> {
        let prefix = if let Some(root) = worktree_root {
            let root = fs::canonicalize(root)?;
            let cwd = fs::canonicalize(cwd)?;
            match cwd.strip_prefix(&root) {
                Ok(rel) => rel.to_string_lossy().replace('\\', "/").into_bytes(),
                Err(_) => Vec::new(),
            }
        } else {
            Vec::new()
        };
        let cwd_depth = prefix
            .split(|byte| *byte == b'/')
            .filter(|component| !component.is_empty())
            .count();
        let mut filters = Vec::new();
        for spec in pathspecs {
            let normalized = normalize_grep_pathspec(&prefix, spec)?;
            let is_dir_spec = !spec.bytes().any(|b| matches!(b, b'*' | b'?' | b'['));
            filters.push(GrepPathFilter {
                original: spec.clone(),
                normalized,
                is_dir_spec,
                matched: Cell::new(false),
            });
        }
        Ok(Self {
            prefix,
            cwd_depth,
            full_name,
            filters,
        })
    }

    fn matches(&self, path: &[u8]) -> bool {
        if self.filters.is_empty() {
            return self.prefix.is_empty() || path_under_prefix(path, &self.prefix);
        }
        let mut matched = false;
        for filter in &self.filters {
            if grep_pathspec_match(&filter.normalized, path) {
                filter.matched.set(true);
                matched = true;
            }
        }
        matched
    }

    /// `--max-depth N`: limit the search to files at most N directory levels below
    /// the base of each pathspec (or the cwd prefix when no pathspec). N<0 = no
    /// limit. A glob pathspec disables the depth limit for the files it matches.
    fn within_max_depth(&self, path: &[u8], max_depth: Option<i64>) -> bool {
        let Some(max) = max_depth else { return true };
        if max < 0 {
            return true;
        }
        let max = max as usize;
        if self.filters.is_empty() {
            // Base is the cwd prefix.
            let rest = if self.prefix.is_empty() {
                path
            } else {
                match strip_dir_prefix(path, &self.prefix) {
                    Some(r) => r,
                    None => return true,
                }
            };
            return slash_count(rest) <= max;
        }
        // For each matching filter, measure depth relative to that filter's base.
        for filter in &self.filters {
            if !grep_pathspec_match(&filter.normalized, path) {
                continue;
            }
            if !filter.is_dir_spec {
                // Glob pathspec: no depth limit.
                return true;
            }
            let base = &filter.normalized;
            let rest = if base.is_empty() {
                path
            } else if path == base.as_slice() {
                return true;
            } else {
                match strip_dir_prefix(path, base) {
                    Some(r) => r,
                    None => continue,
                }
            };
            if slash_count(rest) <= max {
                return true;
            }
        }
        false
    }

    fn display(&self, path: &[u8]) -> Vec<u8> {
        if self.full_name || self.prefix.is_empty() {
            return path.to_vec();
        }
        if let Some(rest) = strip_dir_prefix(path, &self.prefix) {
            return rest.to_vec();
        }
        let mut display = Vec::new();
        for _ in 0..self.cwd_depth {
            display.extend_from_slice(b"../");
        }
        display.extend_from_slice(path);
        display
    }

    fn report_unmatched(&self) -> Result<()> {
        let mut unmatched = false;
        for filter in &self.filters {
            if !filter.matched.get() {
                eprintln!(
                    "error: pathspec '{}' did not match any file(s) known to git",
                    filter.original
                );
                unmatched = true;
            }
        }
        if unmatched {
            eprintln!("Did you forget to 'git add'?");
            return Err(GitError::Exit(1));
        }
        Ok(())
    }
}

fn slash_count(path: &[u8]) -> usize {
    path.iter().filter(|&&b| b == b'/').count()
}

fn path_under_prefix(path: &[u8], prefix: &[u8]) -> bool {
    strip_dir_prefix(path, prefix).is_some()
}

fn strip_dir_prefix<'a>(path: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    if prefix.is_empty() {
        return Some(path);
    }
    let rest = path.strip_prefix(prefix)?;
    let rest = rest.strip_prefix(b"/")?;
    if rest.is_empty() { None } else { Some(rest) }
}

fn normalize_grep_pathspec(prefix: &[u8], arg: &str) -> Result<Vec<u8>> {
    let mut components: Vec<Vec<u8>> = prefix
        .split(|byte| *byte == b'/')
        .filter(|component| !component.is_empty())
        .map(Vec::from)
        .collect();
    for component in Path::new(arg).components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                components.pop().ok_or_else(|| {
                    GitError::InvalidPath(format!("{arg}: '{arg}' is outside repository"))
                })?;
            }
            std::path::Component::Normal(name) => {
                components.push(name.to_string_lossy().as_bytes().to_vec());
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                return Err(GitError::Unsupported(
                    "grep pathspecs currently support relative paths".into(),
                ));
            }
        }
    }
    Ok(components.join(&b'/'))
}

fn grep_pathspec_match(spec: &[u8], path: &[u8]) -> bool {
    if spec.is_empty() {
        return true;
    }
    if path == spec {
        return true;
    }
    if let Some(rest) = path.strip_prefix(spec)
        && rest.first() == Some(&b'/')
    {
        return true;
    }
    if spec.iter().any(|b| matches!(b, b'*' | b'?' | b'[')) {
        return wildmatch(spec, path);
    }
    false
}

fn wildmatch(pattern: &[u8], text: &[u8]) -> bool {
    fn rec(pattern: &[u8], text: &[u8]) -> bool {
        let mut pi = 0;
        let mut ti = 0;
        while pi < pattern.len() {
            match pattern[pi] {
                b'*' => {
                    while pi < pattern.len() && pattern[pi] == b'*' {
                        pi += 1;
                    }
                    if pi == pattern.len() {
                        return true;
                    }
                    let mut k = ti;
                    loop {
                        if rec(&pattern[pi..], &text[k..]) {
                            return true;
                        }
                        if k >= text.len() {
                            return false;
                        }
                        k += 1;
                    }
                }
                b'?' => {
                    if ti >= text.len() {
                        return false;
                    }
                    pi += 1;
                    ti += 1;
                }
                b'[' => {
                    if ti >= text.len() {
                        return false;
                    }
                    match match_bracket(&pattern[pi..], text[ti]) {
                        BracketOutcome::Match(consumed) => {
                            pi += consumed;
                            ti += 1;
                        }
                        BracketOutcome::NoMatch => return false,
                        BracketOutcome::Malformed => {
                            if text[ti] != b'[' {
                                return false;
                            }
                            pi += 1;
                            ti += 1;
                        }
                    }
                }
                b'\\' if pi + 1 < pattern.len() => {
                    if ti >= text.len() || text[ti] != pattern[pi + 1] {
                        return false;
                    }
                    pi += 2;
                    ti += 1;
                }
                literal => {
                    if ti >= text.len() || text[ti] != literal {
                        return false;
                    }
                    pi += 1;
                    ti += 1;
                }
            }
        }
        ti == text.len()
    }
    rec(pattern, text)
}

enum BracketOutcome {
    Match(usize),
    NoMatch,
    Malformed,
}

fn match_bracket(pattern: &[u8], ch: u8) -> BracketOutcome {
    let mut i = 1;
    let negate = matches!(pattern.get(i), Some(b'!') | Some(b'^'));
    if negate {
        i += 1;
    }
    let mut matched = false;
    let mut first = true;
    while i < pattern.len() {
        let c = pattern[i];
        if c == b']' && !first {
            let hit = matched != negate;
            return if hit {
                BracketOutcome::Match(i + 1)
            } else {
                BracketOutcome::NoMatch
            };
        }
        first = false;
        if i + 2 < pattern.len() && pattern[i + 1] == b'-' && pattern[i + 2] != b']' {
            let lo = c;
            let hi = pattern[i + 2];
            if lo <= ch && ch <= hi {
                matched = true;
            }
            i += 3;
        } else {
            if c == ch {
                matched = true;
            }
            i += 1;
        }
    }
    BracketOutcome::Malformed
}

// ---------------------------------------------------------------------------
// Regular-expression engine (POSIX BRE/ERE subset) + fixed strings
// ---------------------------------------------------------------------------

struct GrepMatcher {
    patterns: Vec<CompiledPattern>,
    line_regexp: bool,
}

enum CompiledPattern {
    Fixed { needle: Vec<u8>, ignore_case: bool },
    Regex(Regex),
}

impl GrepMatcher {
    fn compile(opts: &GrepOptions) -> Result<Self> {
        let mut patterns = Vec::new();
        for raw in &opts.patterns {
            let compiled = match opts.kind {
                PatternKind::Fixed => CompiledPattern::Fixed {
                    needle: raw.as_bytes().to_vec(),
                    ignore_case: opts.ignore_case,
                },
                PatternKind::Basic | PatternKind::Extended => {
                    let extended = opts.kind == PatternKind::Extended;
                    let regex = Regex::compile(raw, extended, opts.ignore_case, opts.word)?;
                    CompiledPattern::Regex(regex)
                }
            };
            patterns.push(compiled);
        }
        Ok(Self {
            patterns,
            line_regexp: opts.line_regexp,
        })
    }

    /// Find the leftmost match of pattern `idx` starting at `from`.
    fn find_idx(&self, idx: usize, line: &[u8], from: usize) -> Option<(usize, usize)> {
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
    fn match_spans_expr(&self, expr: Option<&Expr>, line: &[u8]) -> Vec<(usize, usize)> {
        let indices: Vec<usize> = match expr {
            Some(e) => {
                let mut v = Vec::new();
                collect_positive_atoms(e, false, &mut v);
                v
            }
            None => (0..self.patterns.len()).collect(),
        };
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

fn contains(haystack: &[u8], needle: &[u8], ignore_case: bool) -> bool {
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

#[derive(Debug, Clone)]
enum Node {
    Literal(u8),
    AnyChar,
    Class { negate: bool, items: Vec<ClassItem> },
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
    Empty,
}

#[derive(Debug, Clone)]
enum ClassItem {
    Single(u8),
    Range(u8, u8),
    Posix(PosixClass),
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

struct Regex {
    root: Node,
    ignore_case: bool,
}

impl Regex {
    fn compile(pattern: &str, extended: bool, ignore_case: bool, word: bool) -> Result<Self> {
        let bytes = pattern.as_bytes();
        let mut parser = RegexParser {
            bytes,
            pos: 0,
            extended,
        };
        let mut root = parser.parse_alternation()?;
        if parser.pos != bytes.len() {
            return Err(GitError::Command(format!(
                "invalid regular expression: {pattern}"
            )));
        }
        if word {
            root = Node::Concat(vec![
                Node::WordBoundary,
                Node::Group(Box::new(root)),
                Node::WordBoundary,
            ]);
        }
        Ok(Self { root, ignore_case })
    }

    fn find_from(&self, text: &[u8], from: usize) -> Option<(usize, usize)> {
        for start in from..=text.len() {
            if let Some(end) = match_node(&self.root, text, start, self.ignore_case) {
                return Some((start, end));
            }
        }
        None
    }

    fn matches_whole(&self, text: &[u8]) -> bool {
        match_anchored_full(&self.root, text, self.ignore_case)
    }
}

struct RegexParser<'a> {
    bytes: &'a [u8],
    pos: usize,
    extended: bool,
}

impl RegexParser<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
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
            Some(b'|') if self.extended => true,
            Some(b'\\') if !self.extended => self.bytes.get(self.pos + 1) == Some(&b'|'),
            _ => false,
        }
    }

    fn consume_alternation(&mut self) {
        if self.extended {
            self.pos += 1;
        } else {
            self.pos += 2;
        }
    }

    fn at_group_close(&self) -> bool {
        match self.peek() {
            Some(b')') if self.extended => true,
            Some(b'\\') if !self.extended => self.bytes.get(self.pos + 1) == Some(&b')'),
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
        if self.extended {
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
            b'(' if self.extended => {
                self.pos += 1;
                let inner = self.parse_alternation()?;
                if self.peek() != Some(b')') {
                    return Err(GitError::Command("unbalanced ( in regex".into()));
                }
                self.pos += 1;
                Ok(Node::Group(Box::new(inner)))
            }
            b'\\' => self.parse_escape(),
            other => {
                self.pos += 1;
                Ok(Node::Literal(other))
            }
        }
    }

    fn parse_escape(&mut self) -> Result<Node> {
        let Some(next) = self.bytes.get(self.pos + 1).copied() else {
            self.pos += 1;
            return Ok(Node::Literal(b'\\'));
        };
        if !self.extended && next == b'(' {
            self.pos += 2;
            let inner = self.parse_alternation()?;
            if !self.at_group_close() {
                return Err(GitError::Command("unbalanced \\( in regex".into()));
            }
            self.pos += 2;
            return Ok(Node::Group(Box::new(inner)));
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

    fn parse_class(&mut self) -> Result<Node> {
        let start = self.pos;
        self.pos += 1;
        let negate = matches!(self.peek(), Some(b'^'));
        if negate {
            self.pos += 1;
        }
        let mut items = Vec::new();
        let mut first = true;
        loop {
            let Some(byte) = self.peek() else {
                self.pos = start + 1;
                return Ok(Node::Literal(b'['));
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
            b'+' if self.extended => (1, None, 1),
            b'?' if self.extended => (0, Some(1), 1),
            b'{' if self.extended => match self.parse_bound(self.pos + 1, false)? {
                Some((min, max, end)) => (min, max, end - self.pos),
                None => return Ok(atom),
            },
            b'\\' if !self.extended => {
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
        Ok(Node::Repeat {
            node: Box::new(atom),
            min,
            max,
            greedy: true,
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

fn match_node(root: &Node, text: &[u8], pos: usize, ignore_case: bool) -> Option<usize> {
    match_seq(root, text, pos, ignore_case, &|p| Some(p))
}

fn match_anchored_full(root: &Node, text: &[u8], ignore_case: bool) -> bool {
    match_seq(root, text, 0, ignore_case, &|p| {
        if p == text.len() { Some(p) } else { None }
    })
    .is_some()
}

fn match_seq(
    node: &Node,
    text: &[u8],
    pos: usize,
    ignore_case: bool,
    cont: &dyn Fn(usize) -> Option<usize>,
) -> Option<usize> {
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
        Node::Group(inner) => match_seq(inner, text, pos, ignore_case, cont),
        Node::Concat(nodes) => match_concat(nodes, text, pos, ignore_case, cont),
        Node::Alt(branches) => {
            for branch in branches {
                if let Some(end) = match_seq(branch, text, pos, ignore_case, cont) {
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
            MatchSubject { text, ignore_case },
            pos,
            cont,
        ),
    }
}

fn match_concat(
    nodes: &[Node],
    text: &[u8],
    pos: usize,
    ignore_case: bool,
    cont: &dyn Fn(usize) -> Option<usize>,
) -> Option<usize> {
    match nodes.split_first() {
        None => cont(pos),
        Some((head, tail)) => match_seq(head, text, pos, ignore_case, &|p| {
            match_concat(tail, text, p, ignore_case, cont)
        }),
    }
}

struct RepeatPattern<'a> {
    node: &'a Node,
    min: usize,
    max: Option<usize>,
    greedy: bool,
}

#[derive(Clone, Copy)]
struct MatchSubject<'a> {
    text: &'a [u8],
    ignore_case: bool,
}

fn match_repeat(
    repeat: RepeatPattern<'_>,
    subject: MatchSubject<'_>,
    pos: usize,
    cont: &dyn Fn(usize) -> Option<usize>,
) -> Option<usize> {
    fn match_min(
        node: &Node,
        remaining: usize,
        text: &[u8],
        pos: usize,
        ignore_case: bool,
        after_min: &dyn Fn(usize) -> Option<usize>,
    ) -> Option<usize> {
        if remaining == 0 {
            return after_min(pos);
        }
        match_seq(node, text, pos, ignore_case, &|p| {
            if p == pos {
                return after_min(p);
            }
            match_min(node, remaining - 1, text, p, ignore_case, after_min)
        })
    }

    fn match_optional(
        node: &Node,
        remaining: Option<usize>,
        text: &[u8],
        pos: usize,
        ignore_case: bool,
        cont: &dyn Fn(usize) -> Option<usize>,
    ) -> Option<usize> {
        if remaining == Some(0) {
            return cont(pos);
        }
        let next_remaining = remaining.map(|r| r - 1);
        let more = match_seq(node, text, pos, ignore_case, &|p| {
            if p == pos {
                None
            } else {
                match_optional(node, next_remaining, text, p, ignore_case, cont)
            }
        });
        if more.is_some() {
            return more;
        }
        cont(pos)
    }

    let max_optional = repeat.max.map(|m| m.saturating_sub(repeat.min));
    let _ = repeat.greedy;
    match_min(
        repeat.node,
        repeat.min,
        subject.text,
        pos,
        subject.ignore_case,
        &|p| {
            match_optional(
                repeat.node,
                max_optional,
                subject.text,
                p,
                subject.ignore_case,
                cont,
            )
        },
    )
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
        }
    }
    false
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

    fn regex_match(pattern: &str, extended: bool, text: &str) -> bool {
        let re = Regex::compile(pattern, extended, false, false).expect("compile");
        re.find_from(text.as_bytes(), 0).is_some()
    }

    #[test]
    fn bre_plus_is_literal_but_escaped_is_repeat() {
        assert!(regex_match("a+", false, "a+a"));
        assert!(!regex_match("a+", false, "aaa"));
        assert!(regex_match(r"a\+", false, "aaa"));
    }

    #[test]
    fn ere_plus_and_alternation() {
        assert!(regex_match("a+", true, "aaa"));
        assert!(regex_match("foo|bar", true, "xbarx"));
        assert!(!regex_match("foo|bar", true, "xbazx"));
    }

    #[test]
    fn dot_and_anchors() {
        assert!(regex_match("a.c", false, "abc"));
        assert!(regex_match("^abc$", false, "abc"));
        assert!(!regex_match("^abc$", false, "xabc"));
    }

    #[test]
    fn character_classes_and_posix() {
        assert!(regex_match("[abc]x", false, "bx"));
        assert!(!regex_match("[^abc]x", false, "ax"));
        assert!(regex_match("[[:digit:]]+", true, "abc123"));
        assert!(regex_match("[a-f0-9]", false, "e"));
    }

    #[test]
    fn fixed_string_contains() {
        assert!(contains(b"hello world", b"o w", false));
        assert!(contains(b"Hello", b"hello", true));
        assert!(!contains(b"Hello", b"hello", false));
    }

    #[test]
    fn wildmatch_crosses_slash() {
        assert!(wildmatch(b"*.txt", b"sub/c.txt"));
        assert!(wildmatch(b"sub/*", b"sub/c.txt"));
        assert!(!wildmatch(b"*.rs", b"sub/c.txt"));
        assert!(wildmatch(b"a?c", b"abc"));
    }

    #[test]
    fn pathspec_dir_prefix_matches() {
        assert!(grep_pathspec_match(b"sub", b"sub/c.txt"));
        assert!(grep_pathspec_match(b"sub/c.txt", b"sub/c.txt"));
        assert!(!grep_pathspec_match(b"sub", b"submarine"));
    }

    #[test]
    fn split_lines_handles_trailing_newline() {
        let lines: Vec<&[u8]> = split_lines(b"a\nb\n", b'\n').collect();
        assert_eq!(lines, vec![b"a".as_slice(), b"b".as_slice()]);
        let no_nl: Vec<&[u8]> = split_lines(b"a\nb", b'\n').collect();
        assert_eq!(no_nl, vec![b"a".as_slice(), b"b".as_slice()]);
        let empty: Vec<&[u8]> = split_lines(b"", b'\n').collect();
        assert!(empty.is_empty());
    }

    #[test]
    fn repeat_bounds() {
        assert!(regex_match(r"a\{2,3\}", false, "aa"));
        assert!(!regex_match(r"a\{2,3\}", false, "a"));
        assert!(regex_match("a{2,3}", true, "aaa"));
    }

    #[test]
    fn max_depth_slash_count() {
        assert_eq!(slash_count(b"a/b/c"), 2);
        assert_eq!(slash_count(b"v"), 0);
    }
}
