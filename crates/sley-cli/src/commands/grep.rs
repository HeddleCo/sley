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
#![allow(clippy::expect_used)]

use crate::*;
use sley_grep::{
    Expr, ExprToken, GrepCompileConfig, GrepMatcher, PatternKind, PatternTypeOption,
    RegexDiagnosticVerbosity,
};
use sley_pathspec::{parse_normalized_pathspec_element, pathspec_attrs_match_with};
use std::borrow::Cow;
use std::cell::RefCell;

/// Parsed command-line options for `git grep`.
struct GrepOptions {
    patterns: Vec<String>,
    nul_pattern_from_file: bool,
    /// `-f`/`-e`/positional patterns recorded in argv order with boolean glue, so
    /// the expression tree can be reconstructed.
    tokens: Vec<ExprToken>,
    kind: PatternKind,
    ignore_case: bool,
    word: bool,
    line_regexp: bool,
    invert: bool,
    line_number: bool,
    /// Whether `-n`/`--no-line-number` was given explicitly (so config defaults
    /// are not applied over it).
    line_number_set: bool,
    column: bool,
    column_set: bool,
    files_with_matches: bool,
    files_without_match: bool,
    all_match: bool,
    count: bool,
    name_only_quiet: bool,
    show_filename: Option<bool>,
    only_matching: bool,
    text: bool,
    ignore_binary: bool,
    full_name: bool,
    full_name_set: bool,
    null_data: bool,
    cached: bool,
    untracked: bool,
    exclude_standard: bool,
    max_depth: Option<i64>,
    max_count: Option<i64>,
    before_context: usize,
    after_context: usize,
    show_function: bool,
    function_context: bool,
    heading: bool,
    break_between_files: bool,
    color: bool,
    /// `--recurse-submodules` / `submodule.recurse`: descend into populated,
    /// active submodules, prefixing their paths with the gitlink path.
    recurse_submodules: bool,
    /// Whether `--recurse-submodules`/`--no-recurse-submodules` was given on the
    /// command line (so it overrides the `submodule.recurse` config default).
    recurse_submodules_set: bool,
    /// `-O`/`--open-files-in-pager`: outer `None` = not requested; `Some(None)` =
    /// the default pager (resolved from `git_pager`); `Some(Some(p))` = an
    /// explicit pager command.
    open_pager: Option<Option<String>>,
    revs: Vec<String>,
    pathspecs: Vec<String>,
}

impl GrepOptions {
    fn new() -> Self {
        Self {
            patterns: Vec::new(),
            nul_pattern_from_file: false,
            tokens: Vec::new(),
            kind: PatternKind::Basic,
            ignore_case: false,
            word: false,
            line_regexp: false,
            invert: false,
            line_number: false,
            line_number_set: false,
            column: false,
            column_set: false,
            files_with_matches: false,
            files_without_match: false,
            all_match: false,
            count: false,
            name_only_quiet: false,
            show_filename: None,
            only_matching: false,
            text: false,
            ignore_binary: false,
            full_name: false,
            full_name_set: false,
            null_data: false,
            cached: false,
            untracked: false,
            exclude_standard: false,
            max_depth: None,
            max_count: None,
            before_context: 0,
            after_context: 0,
            show_function: false,
            function_context: false,
            heading: false,
            break_between_files: false,
            color: false,
            recurse_submodules: false,
            recurse_submodules_set: false,
            open_pager: None,
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

/// Effective `grep.*` config decisions resolved from the layered config stream.
struct GrepConfig {
    pattern_type: PatternTypeOption,
    extended: bool,
    linenumber: Option<bool>,
    column: Option<bool>,
    fullname: Option<bool>,
}

/// Resolve the effective pattern type from config (`grep.patternType`,
/// `grep.extendedRegexp`) plus the command-line override accumulated during argv
/// parsing. Config order is file entries first, then `-c` injected parameters;
/// `grep.patternType=default` resets to unspecified so a later
/// `grep.extendedRegexp` can take effect. The command-line flags (`-E/-G/-F/-P`)
/// override config entirely.
fn resolve_pattern_config(config: &GitConfig) -> Result<GrepConfig> {
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
        apply(
            &param.canonical_key.to_ascii_lowercase(),
            param.value.as_deref(),
        )?;
    }

    Ok(GrepConfig {
        pattern_type,
        extended,
        linenumber,
        column,
        fullname,
    })
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
            "--" => {
                // The option terminator is also the rev/path separator; record it
                // as a positional marker so the later scan can split on it.
                saw_double_dash = true;
                positionals.push(DASHDASH.to_string());
            }
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
            "-n" | "--line-number" => {
                opts.line_number = true;
                opts.line_number_set = true;
            }
            "--no-line-number" => {
                opts.line_number = false;
                opts.line_number_set = true;
            }
            "--column" => {
                opts.column = true;
                opts.column_set = true;
            }
            "--no-column" => {
                opts.column = false;
                opts.column_set = true;
            }
            "-l" | "--files-with-matches" | "--name-only" => opts.files_with_matches = true,
            "-L" | "--files-without-match" => opts.files_without_match = true,
            "--all-match" => opts.all_match = true,
            "-c" | "--count" => opts.count = true,
            "-q" | "--quiet" => opts.name_only_quiet = true,
            "-o" | "--only-matching" => opts.only_matching = true,
            "-H" => opts.show_filename = Some(true),
            "-h" | "--no-filename" => opts.show_filename = Some(false),
            "--full-name" => {
                opts.full_name = true;
                opts.full_name_set = true;
            }
            "--no-full-name" => {
                opts.full_name = false;
                opts.full_name_set = true;
            }
            "-a" | "--text" => opts.text = true,
            "-I" => opts.ignore_binary = true,
            "-z" | "--null" => opts.null_data = true,
            "--cached" => opts.cached = true,
            "--untracked" => {
                opts.untracked = true;
                opts.exclude_standard = true;
            }
            "-p" | "--show-function" => opts.show_function = true,
            "-W" | "--function-context" => opts.function_context = true,
            "--heading" => opts.heading = true,
            "--no-heading" => opts.heading = false,
            "--break" => opts.break_between_files = true,
            "--no-break" => opts.break_between_files = false,
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
            "--exclude-standard" => opts.exclude_standard = true,
            "--no-exclude-standard" => opts.exclude_standard = false,
            "--color" => opts.color = true,
            "--no-color" => opts.color = false,
            value if value.starts_with("--color=") => {
                opts.color = !matches!(value.strip_prefix("--color="), Some("never" | "false"));
            }
            "--recurse-submodules" => {
                opts.recurse_submodules = true;
                opts.recurse_submodules_set = true;
            }
            "--no-recurse-submodules" => {
                opts.recurse_submodules = false;
                opts.recurse_submodules_set = true;
            }
            "-O" | "--open-files-in-pager" => opts.open_pager = Some(None),
            value if let Some(v) = value.strip_prefix("--open-files-in-pager=") => {
                opts.open_pager = Some(Some(v.to_string()));
            }
            value if let Some(v) = value.strip_prefix("-O") => {
                opts.open_pager = Some(Some(v.to_string()));
            }
            "--threads" => {
                let _ = iter.next();
            }
            value if value.starts_with("--threads=") => {}
            value if value.starts_with("--color=") => {}
            value
                if value.starts_with('-')
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

    let repo = match RepositoryContext::discover_current() {
        Ok(repo) => Some(repo),
        Err(err) => {
            if no_index || grep_fallback_to_no_index()? {
                None
            } else {
                return Err(err);
            }
        }
    };
    // `--attr-source` / `GIT_ATTR_SOURCE` names a tree to read `.gitattributes`
    // from; with no repository there is nothing to resolve it against, so git
    // dies as soon as the default attr source is computed (attr.c).
    if repo.is_none() && std::env::var_os("GIT_ATTR_SOURCE").is_some() {
        eprintln!("fatal: cannot use --attr-source or GIT_ATTR_SOURCE without repo");
        return Err(GitError::Exit(128));
    }

    // Resolve `--recurse-submodules` (CLI override beats the `submodule.recurse`
    // config default). git ignores it without an index (`--no-index`/no repo).
    let mut recurse = opts.recurse_submodules;
    if !opts.recurse_submodules_set
        && let Some(repo) = repo.as_ref()
    {
        recurse = resolve_recurse_submodules(repo.config())?;
    }
    if no_index || repo.is_none() {
        recurse = false;
    }
    if recurse && opts.untracked {
        eprintln!("fatal: --untracked not supported with --recurse-submodules");
        return Err(GitError::Exit(128));
    }
    opts.recurse_submodules = recurse;

    // `-O`/`--open-files-in-pager`: resolve the pager command up front (its
    // resolution can read `core.pager`) and, when active, redirect matched file
    // names into a collector instead of stdout.
    let pager_cmd = resolve_open_pager(&opts.open_pager, repo.as_ref().map(|r| r.config()));
    if pager_cmd.is_some() {
        // show_in_pager forces color off in the (suppressed) grep output.
        opts.color = false;
    }
    let pager_collector: Option<RefCell<Vec<Vec<u8>>>> =
        pager_cmd.as_ref().map(|_| RefCell::new(Vec::new()));

    if no_index || opts.untracked || repo.is_none() {
        let pattern_type = cli_pattern_type.unwrap_or(PatternTypeOption::Bre);
        opts.kind = match pattern_type {
            PatternTypeOption::Ere => PatternKind::Extended,
            PatternTypeOption::Fixed => PatternKind::Fixed,
            PatternTypeOption::Pcre => PatternKind::Perl,
            _ => PatternKind::Basic,
        };
        reject_nul_pattern_without_pcre(&opts)?;
        let color_config = repo.as_ref().map(|repo| repo.config());
        let any = grep_no_index(
            &opts,
            color_config,
            repo.as_ref(),
            &positionals,
            DASHDASH,
            pager_collector.as_ref(),
        )?;
        if let (Some(pager), Some(collector)) = (&pager_cmd, &pager_collector)
            && any
        {
            run_open_pager(pager, &opts, &collector.borrow())?;
        }
        return if any { Ok(()) } else { Err(GitError::Exit(1)) };
    }

    let repo = repo.expect("repository discovery succeeded above");
    let cwd = repo.cwd();
    let git_dir = repo.git_dir();
    let format = repo.format();
    let db = repo.objects();

    // Resolve config-driven pattern type + display defaults, then the CLI
    // override (`-E/-G/-F/-P` win over config).
    let cfg = resolve_pattern_config(repo.config())?;
    let cfg_linenumber = cfg.linenumber;
    let cfg_column = cfg.column;
    let cfg_fullname = cfg.fullname;
    let mut pattern_type = cfg.pattern_type;
    if let Some(cli) = cli_pattern_type {
        pattern_type = cli;
    }
    if pattern_type == PatternTypeOption::Unspecified {
        pattern_type = if cfg.extended {
            PatternTypeOption::Ere
        } else {
            PatternTypeOption::Bre
        };
    }
    opts.kind = match pattern_type {
        PatternTypeOption::Ere => PatternKind::Extended,
        PatternTypeOption::Fixed => PatternKind::Fixed,
        PatternTypeOption::Pcre => PatternKind::Perl,
        _ => PatternKind::Basic,
    };
    reject_nul_pattern_without_pcre(&opts)?;
    // `grep.*` config sets the default; an explicit CLI flag (tracked by the
    // `_set` markers) overrides it. git applies config first, then CLI overrides.
    if let Some(v) = cfg_linenumber
        && !opts.line_number_set
    {
        opts.line_number = v;
    }
    if let Some(v) = cfg_column
        && !opts.column_set
    {
        opts.column = v;
    }
    if let Some(v) = cfg_fullname
        && !opts.full_name_set
    {
        opts.full_name = v;
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

    // `-O` works only on the worktree, never against `--cached` or a tree-ish.
    if pager_cmd.is_some() && (opts.cached || !opts.revs.is_empty()) {
        eprintln!("fatal: --open-files-in-pager only works on the worktree");
        return Err(GitError::Exit(128));
    }

    let matcher = GrepMatcher::compile(GrepCompileConfig {
        patterns: &opts.patterns,
        kind: opts.kind,
        ignore_case: opts.ignore_case,
        word: opts.word,
        line_regexp: opts.line_regexp,
        diagnostic_verbosity: RegexDiagnosticVerbosity::from_env(),
    })?;
    let expr = build_expr(&opts.tokens);

    let worktree_root = match worktree_root_for_git_dir(git_dir) {
        Ok(root) if root.is_dir() => Some(root),
        _ => None,
    };
    let pathspec = GrepPathspec::new(
        worktree_root.as_deref(),
        cwd,
        opts.full_name,
        &opts.pathspecs,
    )?;
    let userdiff_attributes = worktree_root
        .as_deref()
        .map(sley_worktree::StandardAttributeMatcher::from_worktree_root)
        .transpose()?;
    let userdiff = commands::userdiff::UserdiffResolver::with_attributes(
        userdiff_attributes,
        Some(repo.config().clone()),
    );

    let plan = GrepPlan {
        matcher: &matcher,
        expr: expr.as_ref(),
        opts: &opts,
        pathspec: &pathspec,
        colors: GrepColors::from_config(repo.config(), opts.color),
        pager: pager_collector.as_ref(),
        userdiff: Some(&userdiff),
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
                config: repo.config(),
            },
            b"",
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
                    config: repo.config(),
                    common_dir: repo.git_dir(),
                    worktree_root: worktree_root.as_deref(),
                },
                b"",
                &plan,
                &mut out,
            )?;
            any_match = any_match || matched;
        }
    }

    pathspec.report_unmatched()?;

    if let (Some(pager), Some(collector)) = (&pager_cmd, &pager_collector)
        && any_match
    {
        run_open_pager(pager, &opts, &collector.borrow())?;
    }

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
    colors: GrepColors,
    /// `-O`: when set, matched file display paths are collected here (instead of
    /// printed) so they can be handed to the pager once the search completes.
    pager: Option<&'a RefCell<Vec<Vec<u8>>>>,
    userdiff: Option<&'a commands::userdiff::UserdiffResolver>,
}

fn grep_userdiff_driver(
    plan: &GrepPlan<'_>,
    path: &[u8],
) -> Result<Option<std::rc::Rc<commands::userdiff::ResolvedDriver>>> {
    if !(plan.opts.show_function || plan.opts.function_context) {
        return Ok(None);
    }
    match plan.userdiff {
        Some(userdiff) => userdiff.driver_for_path(path),
        None => Ok(None),
    }
}

struct GrepColors {
    enabled: bool,
    filename: String,
    separator: String,
    matched: String,
    reset: String,
}

impl GrepColors {
    fn none() -> Self {
        Self {
            enabled: false,
            filename: String::new(),
            separator: String::new(),
            matched: String::new(),
            reset: String::new(),
        }
    }

    fn from_config(config: &GitConfig, enabled: bool) -> Self {
        if !enabled {
            return Self::none();
        }
        let filename = config
            .get("color", Some("grep"), "filename")
            .map(|spec| git_color_spec_to_ansi(spec, enabled))
            .unwrap_or_default();
        let separator = config
            .get("color", Some("grep"), "separator")
            .map(|spec| git_color_spec_to_ansi(spec, enabled))
            .unwrap_or_default();
        let matched_spec = config
            .get("color", Some("grep"), "matchSelected")
            .or_else(|| config.get("color", Some("grep"), "match"))
            .unwrap_or("bold red");
        Self {
            enabled,
            filename,
            separator,
            matched: git_color_spec_to_ansi(matched_spec, enabled),
            reset: git_color_spec_to_ansi("reset", enabled),
        }
    }
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
    if raw.contains(&0) {
        opts.nul_pattern_from_file = true;
    }
    for line in raw.split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        opts.push_pattern(String::from_utf8_lossy(line).into_owned());
    }
    Ok(())
}

fn reject_nul_pattern_without_pcre(opts: &GrepOptions) -> Result<()> {
    if opts.nul_pattern_from_file && opts.kind != PatternKind::Perl {
        eprintln!(
            "fatal: given pattern contains NULL byte (This is only supported with -P under PCRE v2)"
        );
        return Err(GitError::Exit(128));
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
            'n' => {
                opts.line_number = true;
                opts.line_number_set = true;
            }
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

fn grep_fallback_to_no_index() -> Result<bool> {
    let mut fallback = false;
    for param in injected_config_parameters()? {
        if param
            .canonical_key
            .eq_ignore_ascii_case("grep.fallbacktonoindex")
        {
            fallback = parse_grep_bool(param.value.as_deref());
        }
    }
    Ok(fallback)
}

/// Resolve the `submodule.recurse` config default (file entries first, then
/// `-c`-injected parameters; last value wins). git reads this in
/// `grep_cmd_config` before the command-line `--recurse-submodules` override.
fn resolve_recurse_submodules(config: &GitConfig) -> Result<bool> {
    let mut value = config
        .get_bool("submodule", None, "recurse")
        .unwrap_or(false);
    for param in injected_config_parameters()? {
        if param
            .canonical_key
            .eq_ignore_ascii_case("submodule.recurse")
        {
            value = parse_grep_bool(param.value.as_deref());
        }
    }
    Ok(value)
}

/// git's `git_pager(repo, 1)`: `GIT_PAGER` env, then `core.pager`, then `PAGER`
/// env, then the compiled default (`less`). An empty value or `cat` disables
/// paging (returns `None`).
fn git_pager(config: Option<&GitConfig>) -> Option<String> {
    let pager = std::env::var("GIT_PAGER")
        .ok()
        .or_else(|| config.and_then(|c| c.get("core", None, "pager").map(str::to_string)))
        .or_else(|| std::env::var("PAGER").ok())
        .unwrap_or_else(|| "less".to_string());
    if pager.is_empty() || pager == "cat" {
        None
    } else {
        Some(pager)
    }
}

/// Resolve the `-O`/`--open-files-in-pager` argument to the actual pager command,
/// or `None` when `-O` was not requested (or its default resolution disables
/// paging). An explicit `-O<cmd>` is taken verbatim.
fn resolve_open_pager(
    open_pager: &Option<Option<String>>,
    config: Option<&GitConfig>,
) -> Option<String> {
    match open_pager {
        None => None,
        Some(Some(explicit)) => Some(explicit.clone()),
        Some(None) => git_pager(config),
    }
}

/// git's pager basename test: when the command is longer than 4 bytes and the
/// fifth-from-last byte is a directory separator, only the trailing 4 bytes are
/// compared (so `./less` and `/usr/bin/less` both reduce to `less`).
fn pager_basename(pager: &str) -> &str {
    let bytes = pager.as_bytes();
    if bytes.len() > 4 && bytes[bytes.len() - 5] == b'/' {
        &pager[pager.len() - 4..]
    } else {
        pager
    }
}

/// Run the resolved `-O` pager over the matched files (git's `run_pager`). The
/// pager string is executed as a shell command with the (optional `+/` jump and)
/// file arguments passed positionally, mirroring `run_command(use_shell=1)`.
#[cfg(unix)]
fn run_open_pager(pager: &str, opts: &GrepOptions, files: &[Vec<u8>]) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;

    let base = pager_basename(pager);
    let mut extra: Vec<std::ffi::OsString> = Vec::new();
    // less honors `-I` for a case-insensitive search.
    if opts.ignore_case && base == "less" {
        extra.push(std::ffi::OsString::from("-I"));
    }
    // A single pattern jumps less/vi to the first match (`+/*PAT` / `+/PAT`).
    if opts.patterns.len() == 1 && (base == "less" || base == "vi") {
        let star = if base == "less" { "*" } else { "" };
        extra.push(std::ffi::OsString::from(format!(
            "+/{star}{}",
            opts.patterns[0]
        )));
    }
    for file in files {
        extra.push(std::ffi::OsStr::from_bytes(file).to_os_string());
    }

    // `sh -c '<pager> "$@"' <pager> <extra...>`: the pager string is the shell
    // snippet and the file list expands as positional parameters.
    let status = ProcessCommand::new("sh")
        .arg("-c")
        .arg(format!("{pager} \"$@\""))
        .arg(pager)
        .args(&extra)
        .status()?;
    if !status.success() {
        return Err(GitError::Exit(status.code().unwrap_or(1)));
    }
    Ok(())
}

#[cfg(not(unix))]
fn run_open_pager(_pager: &str, _opts: &GrepOptions, _files: &[Vec<u8>]) -> Result<()> {
    // The `-O`/`--open-files-in-pager` path shells out via `sh -c` and maps raw
    // path bytes through `OsStrExt`; neither is available off Unix.
    Err(GitError::Unsupported(
        "grep --open-files-in-pager is only supported on Unix".to_string(),
    ))
}

fn grep_no_index(
    opts: &GrepOptions,
    color_config: Option<&GitConfig>,
    repo: Option<&RepositoryContext>,
    positionals: &[String],
    dashdash: &str,
    pager: Option<&RefCell<Vec<Vec<u8>>>>,
) -> Result<bool> {
    let cwd = env::current_dir()?;
    let cwd_canon = fs::canonicalize(&cwd)?;
    let raw_paths = no_index_paths(positionals, dashdash)?;
    let worktree_root = if opts.untracked {
        repo.and_then(|repo| worktree_root_for_git_dir(repo.git_dir()).ok())
    } else {
        None
    };
    let pathspec_args: &[String] = if opts.untracked { &raw_paths } else { &[] };
    let pathspec = GrepPathspec::new(
        worktree_root.as_deref(),
        &cwd,
        opts.full_name,
        pathspec_args,
    )?;
    let matcher = GrepMatcher::compile(GrepCompileConfig {
        patterns: &opts.patterns,
        kind: opts.kind,
        ignore_case: opts.ignore_case,
        word: opts.word,
        line_regexp: opts.line_regexp,
        diagnostic_verbosity: RegexDiagnosticVerbosity::from_env(),
    })?;
    let expr = build_expr(&opts.tokens);
    let plan = GrepPlan {
        matcher: &matcher,
        expr: expr.as_ref(),
        opts,
        pathspec: &pathspec,
        colors: color_config
            .map(|config| GrepColors::from_config(config, opts.color))
            .unwrap_or_else(GrepColors::none),
        pager,
        userdiff: None,
    };
    let ignore = if opts.exclude_standard {
        NoIndexIgnore::from_cwd(&cwd)?
    } else {
        NoIndexIgnore::default()
    };
    let mut files = Vec::new();
    if opts.untracked {
        collect_no_index_path(
            &cwd,
            &cwd_canon,
            "",
            &ignore,
            worktree_root.as_deref(),
            &mut files,
        )?;
    } else {
        for raw in raw_paths {
            collect_no_index_path(
                &cwd,
                &cwd_canon,
                &raw,
                &ignore,
                worktree_root.as_deref(),
                &mut files,
            )?;
        }
    }
    files.sort_by(|a, b| a.display.cmp(&b.display));

    let mut any_match = false;
    let mut printed_file = false;
    let mut out = io::stdout();
    for file in files {
        if opts.untracked && !pathspec.matches(&file.match_path) {
            continue;
        }
        let Ok(content) = fs::read(&file.absolute) else {
            continue;
        };
        let display = if opts.untracked {
            plan.pathspec.display(&file.match_path)
        } else {
            file.display.clone()
        };
        let matched = grep_buffer(
            &content,
            &display,
            None,
            None,
            &plan,
            &mut out,
            &mut printed_file,
        )?;
        any_match = any_match || matched;
    }
    if opts.untracked {
        pathspec.report_unmatched()?;
    }
    Ok(any_match)
}

fn no_index_paths(positionals: &[String], dashdash: &str) -> Result<Vec<String>> {
    let has_dashdash = positionals.iter().any(|p| p == dashdash);
    if has_dashdash {
        let Some(split) = positionals.iter().position(|p| p == dashdash) else {
            return Ok(Vec::new());
        };
        if split > 0 {
            eprintln!("fatal: option '--no-index' cannot be used with revs");
            return Err(GitError::Exit(128));
        }
        let paths: Vec<String> = positionals[split + 1..].to_vec();
        return Ok(if paths.is_empty() {
            vec![".".to_string()]
        } else {
            paths
        });
    }
    Ok(if positionals.is_empty() {
        vec![".".to_string()]
    } else {
        positionals.to_vec()
    })
}

#[derive(Default)]
struct NoIndexIgnore {
    patterns: Vec<String>,
}

impl NoIndexIgnore {
    fn from_cwd(cwd: &Path) -> Result<Self> {
        let mut patterns = Vec::new();
        let gitignore = cwd.join(".gitignore");
        if let Ok(text) = fs::read_to_string(gitignore) {
            for line in text.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                patterns.push(line.to_string());
            }
        }
        Ok(Self { patterns })
    }

    fn ignores(&self, display: &[u8]) -> bool {
        if self.patterns.is_empty() {
            return false;
        }
        let path = String::from_utf8_lossy(display);
        let basename = path.rsplit('/').next().unwrap_or(&path);
        self.patterns.iter().any(|pattern| {
            if pattern.contains('/') {
                wildcard_match(pattern.as_bytes(), path.as_bytes())
            } else {
                wildcard_match(pattern.as_bytes(), basename.as_bytes())
            }
        })
    }
}

struct NoIndexFile {
    absolute: PathBuf,
    display: Vec<u8>,
    match_path: Vec<u8>,
}

fn collect_no_index_path(
    cwd: &Path,
    cwd_canon: &Path,
    raw: &str,
    ignore: &NoIndexIgnore,
    match_root: Option<&Path>,
    out: &mut Vec<NoIndexFile>,
) -> Result<()> {
    let path = cwd.join(raw);
    if !path.exists() {
        eprintln!("fatal: {raw}: no such path in the working tree");
        return Err(GitError::Exit(128));
    }
    let canon = fs::canonicalize(&path)?;
    if !canon.starts_with(cwd_canon) {
        eprintln!("fatal: {raw}: '{raw}' is outside the directory tree");
        return Err(GitError::Exit(128));
    }
    if path.is_dir() {
        collect_no_index_dir(cwd, &path, ignore, match_root, out)?;
    } else if path.is_file() {
        push_no_index_file(cwd, &path, ignore, match_root, out)?;
    }
    Ok(())
}

fn collect_no_index_dir(
    cwd: &Path,
    dir: &Path,
    ignore: &NoIndexIgnore,
    match_root: Option<&Path>,
    out: &mut Vec<NoIndexFile>,
) -> Result<()> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(dir)? {
        entries.push(entry?);
    }
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let name = entry.file_name();
        if name.as_encoded_bytes() == b".git" && path.is_dir() {
            continue;
        }
        if path.is_dir() {
            collect_no_index_dir(cwd, &path, ignore, match_root, out)?;
        } else if path.is_file() {
            push_no_index_file(cwd, &path, ignore, match_root, out)?;
        }
    }
    Ok(())
}

fn push_no_index_file(
    cwd: &Path,
    path: &Path,
    ignore: &NoIndexIgnore,
    match_root: Option<&Path>,
    out: &mut Vec<NoIndexFile>,
) -> Result<()> {
    let display_path = path.strip_prefix(cwd).unwrap_or(path);
    let display = path_to_bytes(display_path);
    if display.is_empty() || ignore.ignores(&display) {
        return Ok(());
    }
    let match_path = match_root
        .and_then(|root| path.strip_prefix(root).ok())
        .map(path_to_bytes)
        .unwrap_or_else(|| display.clone());
    out.push(NoIndexFile {
        absolute: path.to_path_buf(),
        display,
        match_path,
    });
    Ok(())
}

fn wildcard_match(pattern: &[u8], text: &[u8]) -> bool {
    fn inner(pattern: &[u8], text: &[u8]) -> bool {
        match pattern.split_first() {
            None => text.is_empty(),
            Some((&b'*', rest)) => {
                inner(rest, text) || (!text.is_empty() && inner(pattern, &text[1..]))
            }
            Some((&b'?', rest)) => !text.is_empty() && inner(rest, &text[1..]),
            Some((&p, rest)) => text.first() == Some(&p) && inner(rest, &text[1..]),
        }
    }
    inner(pattern, text)
}

// ---------------------------------------------------------------------------
// Source iteration
// ---------------------------------------------------------------------------

/// An owned handle on a submodule repository, opened during `--recurse-submodules`
/// so its index/object database/config outlive the recursive grep call.
struct OwnedSubrepo {
    git_dir: PathBuf,
    worktree_root: Option<PathBuf>,
    format: ObjectFormat,
    db: FileObjectDatabase,
    config: GitConfig,
}

/// `<gitdir>/modules/<name>`: git's `submodule_name_to_gitdir` location for a
/// submodule's git directory.
fn submodule_name_to_gitdir(common_dir: &Path, name: &str) -> PathBuf {
    common_dir.join("modules").join(name)
}

/// Open a submodule via its populated worktree gitlink (`<worktree>/.git`), as
/// `grep_cache`'s recursion does. Returns `None` for an unpopulated/unresolvable
/// gitlink.
fn open_submodule_worktree(worktree_root: &Path, sub_rel: &[u8]) -> Option<OwnedSubrepo> {
    let sub_worktree = worktree_root.join(bytes_to_path(sub_rel));
    let git_dir = sley_diff_merge::gitlink_git_dir(&sub_worktree)?;
    let common = common_git_dir_for_git_dir(&git_dir).ok()?;
    let format = repository_object_format(&common).ok()?;
    let db = FileObjectDatabase::from_git_dir(&common, format);
    let config = read_repo_config(&git_dir).ok()?;
    Some(OwnedSubrepo {
        git_dir: common,
        worktree_root: Some(sub_worktree),
        format,
        db,
        config,
    })
}

/// Open a submodule's object database for tree-mode recursion. git's
/// `repo_submodule_init` resolves the gitdir from the populated worktree gitlink
/// first (handling an in-place, non-absorbed submodule) and otherwise from the
/// absorbed `<common>/modules/<name>` location (handling a worktree path that no
/// longer matches the gitlink's recorded path, e.g. a moved submodule grepped at
/// a historic rev). Returns the opened repo plus the submodule's worktree (when
/// populated), for the nested recursion level.
fn open_submodule_tree(
    worktree_root: Option<&Path>,
    common_dir: &Path,
    name: &str,
    path: &[u8],
) -> Option<OwnedSubrepo> {
    let sub_worktree = worktree_root.map(|root| root.join(bytes_to_path(path)));
    let git_dir = sub_worktree
        .as_deref()
        .and_then(sley_diff_merge::gitlink_git_dir)
        .or_else(|| {
            let absorbed = submodule_name_to_gitdir(common_dir, name);
            absorbed.is_dir().then_some(absorbed)
        })?;
    let common = common_git_dir_for_git_dir(&git_dir).ok()?;
    let format = repository_object_format(&common).ok()?;
    let db = FileObjectDatabase::from_git_dir(&common, format);
    let config = read_repo_config(&git_dir).ok()?;
    Some(OwnedSubrepo {
        git_dir: common,
        worktree_root: sub_worktree.filter(|root| root.exists()),
        format,
        db,
        config,
    })
}

/// git's `is_tree_submodule_active`: an entry's path maps (via `.gitmodules`) to a
/// submodule name, then `submodule.<name>.active`, `submodule.active`, and finally
/// the presence of `submodule.<name>.url` decide activeness.
fn submodule_active(
    config: &GitConfig,
    submodules: &sley_submodule::SubmoduleConfigSet,
    path: &[u8],
) -> bool {
    let Ok(path_str) = std::str::from_utf8(path) else {
        return false;
    };
    let Some(module) = submodules.from_path(path_str) else {
        return false;
    };
    let name = module.name.as_str();
    if let Some(active) = config.get_bool("submodule", Some(name), "active") {
        return active;
    }
    let active_specs: Vec<&str> = config
        .get_all("submodule", None, "active")
        .into_iter()
        .flatten()
        .collect();
    if !active_specs.is_empty() {
        return active_specs.iter().any(|spec| {
            sley_pathspec::PathspecElement::parse(
                spec.as_bytes(),
                sley_pathspec::PathspecMatchMagic::default(),
            )
            .map(|element| element.matches_path(path))
            .unwrap_or(false)
        });
    }
    config.get("submodule", Some(name), "url").is_some()
}

/// Load the submodule set from a worktree `.gitmodules`, falling back to the
/// `.gitmodules` blob recorded in the index (git reads the worktree file when it
/// exists, else the index/HEAD copy).
fn load_gitmodules_worktree(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
) -> sley_submodule::SubmoduleConfigSet {
    if let Ok(config) = GitConfig::read(worktree_root.join(".gitmodules")) {
        return sley_submodule::SubmoduleConfigSet::parse(&config);
    }
    if let Ok(Some(index)) = sley_worktree::read_repository_index(git_dir, format) {
        for entry in &index.entries {
            let name: &[u8] = &entry.path;
            if name == b".gitmodules".as_slice() && (entry.flags >> 12) & 0x3 == 0 {
                if let Ok(object) = db.read_object(&entry.oid)
                    && let Ok(config) = GitConfig::parse(&object.body)
                {
                    return sley_submodule::SubmoduleConfigSet::parse(&config);
                }
                break;
            }
        }
    }
    sley_submodule::SubmoduleConfigSet::default()
}

/// Load the submodule set from a tree's `.gitmodules` blob (git's
/// `gitmodules_config_oid` for the grepped tree).
fn load_gitmodules_tree(
    db: &FileObjectDatabase,
    flat: &sley_diff_merge::MergeEntryMap,
) -> sley_submodule::SubmoduleConfigSet {
    if let Some((_, oid)) = flat.get(b".gitmodules".as_slice())
        && let Ok(object) = db.read_object(oid)
        && let Ok(config) = GitConfig::parse(&object.body)
    {
        return sley_submodule::SubmoduleConfigSet::parse(&config);
    }
    sley_submodule::SubmoduleConfigSet::default()
}

/// Concatenate a submodule path prefix with an entry path.
fn join_prefix(prefix: &[u8], name: &[u8]) -> Vec<u8> {
    let mut joined = Vec::with_capacity(prefix.len() + name.len());
    joined.extend_from_slice(prefix);
    joined.extend_from_slice(name);
    joined
}

/// The recursion prefix for a submodule at `name` under `prefix` (a trailing
/// slash separates it from the submodule's own entries).
fn submodule_prefix(prefix: &[u8], name: &[u8]) -> Vec<u8> {
    let mut joined = join_prefix(prefix, name);
    joined.push(b'/');
    joined
}

/// Greps the working tree (default) or the index (`--cached`).
struct GrepIndexSource<'a> {
    git_dir: &'a Path,
    worktree_root: &'a Path,
    format: ObjectFormat,
    db: &'a FileObjectDatabase,
    config: &'a GitConfig,
}

fn grep_index_source(
    source: GrepIndexSource<'_>,
    prefix: &[u8],
    plan: &GrepPlan<'_>,
    out: &mut impl Write,
) -> Result<bool> {
    let mut printed_file = false;
    grep_index_level(&source, prefix, plan, out, &mut printed_file)
}

fn grep_index_level(
    source: &GrepIndexSource<'_>,
    prefix: &[u8],
    plan: &GrepPlan<'_>,
    out: &mut impl Write,
    printed_file: &mut bool,
) -> Result<bool> {
    let Some(index) = sley_worktree::read_repository_index(source.git_dir, source.format)? else {
        return Ok(false);
    };
    const CE_VALID: u16 = 0x8000;

    // git's `clear_skip_worktree_from_present_files`: under sparse checkout a
    // SKIP_WORKTREE entry whose file is actually present in the worktree loses the
    // bit, so the live file is searched. `core.sparseCheckout` is written to the
    // per-worktree config (`extensions.worktreeConfig`), so it is consulted first.
    let worktree_config =
        GitConfig::read(source.git_dir.join("config.worktree")).unwrap_or_default();
    let sparse_enabled = worktree_config
        .get_bool("core", None, "sparseCheckout")
        .or_else(|| source.config.get_bool("core", None, "sparseCheckout"))
        .unwrap_or(false);
    let expect_outside = worktree_config
        .get_bool("sparse", None, "expectFilesOutsideOfPatterns")
        .or_else(|| {
            source
                .config
                .get_bool("sparse", None, "expectFilesOutsideOfPatterns")
        })
        .unwrap_or(false);
    let clear_sparse = sparse_enabled && !expect_outside;

    let submodules = if plan.opts.recurse_submodules {
        Some(load_gitmodules_worktree(
            source.worktree_root,
            source.git_dir,
            source.format,
            source.db,
        ))
    } else {
        None
    };

    let entries = &index.entries;
    let mut any = false;
    let mut i = 0;
    while i < entries.len() {
        let entry = &entries[i];
        let stage = (entry.flags >> 12) & 0x3;
        let path = entry.path.to_vec();
        let mode = entry.mode;
        let flags = entry.flags;
        let oid = entry.oid;
        let is_ita = entry.is_intent_to_add();
        let is_skip_wt = entry.is_skip_worktree();
        // Either advance one entry, or (after processing an unmerged path) past all
        // of its higher-stage siblings, mirroring git's `if (ce_stage(ce))` skip.
        let next = |processed: bool, i: usize| -> usize {
            if processed && stage != 0 {
                let mut j = i + 1;
                while j < entries.len() && {
                    let other: &[u8] = &entries[j].path;
                    other == path.as_slice()
                } {
                    j += 1;
                }
                j
            } else {
                i + 1
            }
        };

        if mode == 0o160000 {
            if let Some(submodules) = &submodules
                && submodule_active(source.config, submodules, &path)
                && let Some(sub) = open_submodule_worktree(source.worktree_root, &path)
                && let Some(sub_worktree) = sub.worktree_root.as_deref()
            {
                let sub_source = GrepIndexSource {
                    git_dir: &sub.git_dir,
                    worktree_root: sub_worktree,
                    format: sub.format,
                    db: &sub.db,
                    config: &sub.config,
                };
                let sub_prefix = submodule_prefix(prefix, &path);
                let matched = grep_index_level(&sub_source, &sub_prefix, plan, out, printed_file)?;
                any = any || matched;
            }
            i = next(false, i);
            continue;
        }

        // Skip SKIP_WORKTREE entries unless --cached, unless sparse-clearing
        // restored a present file.
        let skip_wt = is_skip_wt
            && !(clear_sparse
                && source
                    .worktree_root
                    .join(bytes_to_path(&path))
                    .symlink_metadata()
                    .is_ok());
        if !plan.opts.cached && skip_wt {
            i = next(false, i);
            continue;
        }

        let full = join_prefix(prefix, &path);
        if !plan.pathspec.matches(&full)
            || !plan.pathspec.within_max_depth(&full, plan.opts.max_depth)
        {
            i = next(false, i);
            continue;
        }

        // git: with `cached || CE_VALID` use the recorded blob (skipping stage and
        // intent-to-add entries, which carry no real content); else the live file.
        let use_cached = plan.opts.cached || (flags & CE_VALID) != 0;
        if !plan.opts.cached
            && plan.opts.files_without_match
            && (flags & CE_VALID) != 0
            && oid == ObjectId::empty_blob(source.format)
        {
            i = next(false, i);
            continue;
        }
        let content: Cow<'_, [u8]> = if use_cached {
            if stage != 0 || is_ita {
                i = next(false, i);
                continue;
            }
            let object = read_object_maybe_prefetch_promisor(source.db, &oid)?;
            Cow::Owned(object.body.to_vec())
        } else {
            let absolute = source.worktree_root.join(bytes_to_path(&path));
            match fs::read(&absolute) {
                Ok(bytes) => Cow::Owned(bytes),
                Err(_) => {
                    i = next(false, i);
                    continue;
                }
            }
        };
        let display = plan.pathspec.display(&full);
        let driver = grep_userdiff_driver(plan, &full)?;
        let funcname = driver.as_ref().and_then(|driver| driver.funcname.as_ref());
        let matched = grep_buffer(&content, &display, None, funcname, plan, out, printed_file)?;
        any = any || matched;
        i = next(true, i);
    }
    Ok(any)
}

/// Greps a tree-ish, recursing through subtrees and (with
/// `--recurse-submodules`) into submodule trees.
struct GrepTreeSource<'a> {
    db: &'a FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &'a ObjectId,
    rev: &'a str,
    config: &'a GitConfig,
    /// The repository's common dir, for resolving `modules/<name>` submodule git
    /// directories. For a nested level this is the parent submodule's git dir.
    common_dir: &'a Path,
    /// The repository's worktree root (when populated), for resolving a
    /// submodule's in-place gitlink. For a nested level this is the parent
    /// submodule's worktree.
    worktree_root: Option<&'a Path>,
}

fn grep_tree_source(
    source: GrepTreeSource<'_>,
    prefix: &[u8],
    plan: &GrepPlan<'_>,
    out: &mut impl Write,
) -> Result<bool> {
    let mut printed_file = false;
    grep_tree_level(&source, prefix, plan, out, &mut printed_file)
}

fn grep_tree_level(
    source: &GrepTreeSource<'_>,
    prefix: &[u8],
    plan: &GrepPlan<'_>,
    out: &mut impl Write,
    printed_file: &mut bool,
) -> Result<bool> {
    // `flatten_tree` yields a path-sorted map (blobs, symlinks, and gitlinks),
    // which is the order `git grep <tree-ish>` prints in.
    let flat = sley_diff_merge::flatten_tree(source.db, source.format, source.tree_oid)?;
    let submodules = if plan.opts.recurse_submodules {
        Some(load_gitmodules_tree(source.db, &flat))
    } else {
        None
    };

    let mut any = false;
    for (path, (mode, oid)) in &flat {
        let full = join_prefix(prefix, path);
        if *mode == 0o160000 {
            if let Some(submodules) = &submodules
                && submodule_active(source.config, submodules, path)
                && let Ok(path_str) = std::str::from_utf8(path)
                && let Some(name) = submodules.from_path(path_str).map(|m| m.name.clone())
                && let Some(sub) =
                    open_submodule_tree(source.worktree_root, source.common_dir, &name, path)
                && let Ok(sub_tree) = sley_rev::peel_to_tree(&sub.db, sub.format, oid)
            {
                let sub_source = GrepTreeSource {
                    db: &sub.db,
                    format: sub.format,
                    tree_oid: &sub_tree,
                    rev: source.rev,
                    config: &sub.config,
                    common_dir: &sub.git_dir,
                    worktree_root: sub.worktree_root.as_deref(),
                };
                let sub_prefix = submodule_prefix(prefix, path);
                let matched = grep_tree_level(&sub_source, &sub_prefix, plan, out, printed_file)?;
                any = any || matched;
            }
            continue;
        }
        if !plan
            .pathspec
            .matches_tree(&full, source.db, source.format, source.tree_oid)?
        {
            continue;
        }
        if !plan.pathspec.within_max_depth(&full, plan.opts.max_depth) {
            continue;
        }
        let display = plan.pathspec.display(&full);
        let object = read_object_maybe_prefetch_promisor(source.db, oid)?;
        let driver = grep_userdiff_driver(plan, &full)?;
        let funcname = driver.as_ref().and_then(|driver| driver.funcname.as_ref());
        let matched = grep_buffer(
            &object.body,
            &display,
            Some(source.rev),
            funcname,
            plan,
            out,
            printed_file,
        )?;
        any = any || matched;
    }
    Ok(any)
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
    funcname: Option<&commands::userdiff::CompiledFuncname>,
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

    if opts.files_without_match && opts.all_match {
        return Ok(false);
    }

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
    let mut any = !hits.is_empty();
    if any
        && opts.all_match
        && !plan
            .matcher
            .matches_all_positive_patterns(plan.expr, lines.iter().copied())
    {
        hits.clear();
        any = false;
    }

    // `-O`: name-only collection into the pager list (git sets `name_only` and
    // redirects `output` to `append_path`). The file's display path is recorded
    // once when it has any match; nothing is written to stdout.
    if let Some(collector) = plan.pager {
        if any {
            collector.borrow_mut().push(display_path.to_vec());
        }
        return Ok(any);
    }

    if opts.name_only_quiet {
        // git's `status_only` returns `unmatch_name_only` for files that reached
        // the end without a hit: with `-L`, a file with no match counts as a hit.
        if opts.files_without_match {
            return Ok(!any);
        }
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
        return Ok(!any);
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
            for span in spans {
                write_match_prefix(out, rev, display_path, show_filename, field_sep)?;
                if opts.line_number {
                    out.write_all(hit.line_no.to_string().as_bytes())?;
                    out.write_all(field_sep)?;
                }
                if opts.column {
                    out.write_all((span.0 + 1).to_string().as_bytes())?;
                    out.write_all(field_sep)?;
                }
                out.write_all(&line[span.0..span.1])?;
                out.write_all(b"\n")?;
            }
        }
        return Ok(true);
    }

    // Determine the set of lines to print, including context and function context.
    let to_print = compute_output_lines(&lines, &hits, opts, funcname);
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
        plan.matcher,
        plan.expr,
        &plan.colors,
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
            for idx in 0..plan.matcher.pattern_count() {
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
    funcname: Option<&commands::userdiff::CompiledFuncname>,
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
            let (start, end, func_header) = function_bounds(lines, center, funcname);
            for (i, line) in flags.iter_mut().enumerate().take(end + 1) {
                if i >= start && i <= end && !line.selected {
                    line.context = true;
                }
            }
            // Only mark the header with `=` when it is a real funcname line
            // (a bare include/preamble block above the match shows as `-`).
            if let Some(header) = func_header
                && header < center
                && !flags[header].selected
            {
                flags[header].context = false;
                flags[header].function = true;
            }
        }
    } else if opts.show_function {
        // -p: prepend the enclosing function header (a `=` line) per hunk.
        for hit in hits {
            let center = hit.line_no - 1;
            if let Some(header) = enclosing_function(lines, center, funcname)
                && !flags[header].selected
            {
                flags[header].function = true;
            }
        }
    }
    flags
}

/// A line is a "function" header under git's default `match_funcname`: a
/// non-empty line whose first byte is a letter, `_`, or `$`.
fn is_funcline(line: &[u8], funcname: Option<&commands::userdiff::CompiledFuncname>) -> bool {
    if let Some(funcname) = funcname {
        funcname.match_line(line).is_some()
    } else {
        match line.first() {
            None => false,
            Some(&b) => b.is_ascii_alphabetic() || b == b'_' || b == b'$',
        }
    }
}

/// Find the function header line at or above `from`.
fn enclosing_function(
    lines: &[&[u8]],
    from: usize,
    funcname: Option<&commands::userdiff::CompiledFuncname>,
) -> Option<usize> {
    (0..=from).rev().find(|&i| is_funcline(lines[i], funcname))
}

/// For -W: return `(first_context_line, last_line_of_function, func_header)`.
/// When no funcname line precedes the match, the body extends to the top of the
/// file and `func_header` is `None`.
fn function_bounds(
    lines: &[&[u8]],
    from: usize,
    funcname: Option<&commands::userdiff::CompiledFuncname>,
) -> (usize, usize, Option<usize>) {
    let func_header = enclosing_function(lines, from, funcname);
    let mut start = match func_header {
        Some(h) => h,
        None => 0,
    };
    if funcname.is_some() && func_header.is_some() {
        while start > 0 && !lines[start - 1].is_empty() {
            start -= 1;
        }
    }
    // The function ends just before the next function header.
    let mut end = lines.len() - 1;
    let search_start = func_header.map_or(start + 1, |header| header + 1);
    for i in search_start..lines.len() {
        if is_funcline(lines[i], funcname) {
            end = i - 1;
            break;
        }
    }
    // Trim trailing empty lines (git: "Trailing empty lines are not interesting").
    while end > from && lines[end].is_empty() {
        end -= 1;
    }
    (start, end, func_header)
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
    matcher: &GrepMatcher,
    expr: Option<&Expr>,
    colors: &GrepColors,
    printed_file: &mut bool,
) -> Result<()> {
    // Column lookup per selected line.
    let col_of = |line_no: usize| -> usize {
        hits.iter()
            .find(|h| h.line_no == line_no)
            .map(|h| h.column)
            .unwrap_or(0)
    };

    let has_context = opts.before_context > 0 || opts.after_context > 0 || opts.function_context;
    let heading = opts.heading && show_filename;
    let mut wrote_in_file = false;

    if opts.break_between_files && *printed_file {
        out.write_all(b"\n")?;
    }

    if heading {
        write_heading_path(out, rev, display_path, opts, colors)?;
        wrote_in_file = true;
    }

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
                    write_colored_bytes(out, b"--", &colors.separator, &colors.reset)?;
                    out.write_all(b"\n")?;
                }
            } else if *printed_file && !opts.break_between_files && !heading {
                write_colored_bytes(out, b"--", &colors.separator, &colors.reset)?;
                out.write_all(b"\n")?;
            }
        }
        let sep: &[u8] = if flag.selected {
            field_sep
        } else if flag.function {
            b"="
        } else {
            b"-"
        };
        if !heading {
            write_match_prefix_sep_colored(out, rev, display_path, show_filename, sep, colors)?;
        }
        if opts.line_number {
            out.write_all((i + 1).to_string().as_bytes())?;
            out.write_all(sep)?;
        }
        if opts.column && flag.selected {
            let c = col_of(i + 1);
            out.write_all(c.to_string().as_bytes())?;
            out.write_all(field_sep)?;
        }
        if flag.selected {
            write_highlighted_line(out, matcher, expr, lines[i], colors)?;
        } else {
            out.write_all(lines[i])?;
        }
        out.write_all(b"\n")?;
        last_printed = Some(i);
        wrote_in_file = true;
    }
    if wrote_in_file {
        *printed_file = true;
    }
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

fn write_match_prefix_sep_colored(
    out: &mut impl Write,
    rev: Option<&str>,
    display_path: &[u8],
    show_filename: bool,
    sep: &[u8],
    colors: &GrepColors,
) -> Result<()> {
    if let Some(rev) = rev {
        out.write_all(rev.as_bytes())?;
        write_colored_bytes(out, b":", &colors.separator, &colors.reset)?;
    }
    if show_filename || rev.is_some() {
        let raw = sep == b"\0";
        let quoted = if raw {
            display_path.to_vec()
        } else {
            status_quote_path(display_path, false).into_bytes()
        };
        write_colored_bytes(out, &quoted, &colors.filename, &colors.reset)?;
        write_colored_bytes(out, sep, &colors.separator, &colors.reset)?;
    }
    Ok(())
}

fn write_heading_path(
    out: &mut impl Write,
    rev: Option<&str>,
    display_path: &[u8],
    opts: &GrepOptions,
    colors: &GrepColors,
) -> Result<()> {
    if let Some(rev) = rev {
        out.write_all(rev.as_bytes())?;
        write_colored_bytes(out, b":", &colors.separator, &colors.reset)?;
    }
    let raw = opts.null_data;
    let quoted = if raw {
        display_path.to_vec()
    } else {
        status_quote_path(display_path, false).into_bytes()
    };
    write_colored_bytes(out, &quoted, &colors.filename, &colors.reset)?;
    out.write_all(b"\n")?;
    Ok(())
}

fn write_highlighted_line(
    out: &mut impl Write,
    matcher: &GrepMatcher,
    expr: Option<&Expr>,
    line: &[u8],
    colors: &GrepColors,
) -> Result<()> {
    if !colors.enabled || colors.matched.is_empty() {
        out.write_all(line)?;
        return Ok(());
    }
    let spans = matcher.match_spans_expr(expr, line);
    if spans.is_empty() {
        out.write_all(line)?;
        return Ok(());
    }
    let mut cursor = 0;
    for (start, end) in spans {
        if start > cursor {
            out.write_all(&line[cursor..start])?;
        }
        write_colored_bytes(out, &line[start..end], &colors.matched, &colors.reset)?;
        cursor = end;
    }
    if cursor < line.len() {
        out.write_all(&line[cursor..])?;
    }
    Ok(())
}

fn write_colored_bytes(out: &mut impl Write, bytes: &[u8], color: &str, reset: &str) -> Result<()> {
    if color.is_empty() {
        out.write_all(bytes)?;
    } else {
        out.write_all(color.as_bytes())?;
        out.write_all(bytes)?;
        out.write_all(reset.as_bytes())?;
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

fn path_to_bytes(path: &Path) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        path.to_string_lossy().replace('\\', "/").into_bytes()
    }
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
    element: sley_pathspec::PathspecElement,
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
    attributes: Option<sley_worktree::StandardAttributeMatcher>,
    worktree_root: Option<PathBuf>,
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
        let magic = effective_pathspec_flags();
        for spec in pathspecs {
            let element = parse_normalized_pathspec_element(&prefix, spec, magic)?;
            let is_dir_spec = !element
                .pattern()
                .iter()
                .any(|b| matches!(b, b'*' | b'?' | b'['));
            filters.push(GrepPathFilter {
                original: spec.clone(),
                element,
                is_dir_spec,
                matched: Cell::new(false),
            });
        }
        let needs_attrs = filters
            .iter()
            .any(|filter| !filter.element.attr_requirements().is_empty());
        let attributes = if needs_attrs {
            worktree_root
                .map(sley_worktree::StandardAttributeMatcher::from_worktree_root)
                .transpose()?
        } else {
            None
        };
        Ok(Self {
            prefix,
            cwd_depth,
            full_name,
            filters,
            attributes,
            worktree_root: worktree_root.map(Path::to_path_buf),
        })
    }

    fn matches(&self, path: &[u8]) -> bool {
        self.matches_inner(path, |filter, path| {
            grep_pathspec_match(&filter.element, path)
                && pathspec_attrs_match_with(&filter.element, |requested| {
                    attribute_checks_for_matching(
                        self.attributes
                            .as_ref()
                            .map(|matcher| matcher.attributes_for_path(path, requested, false))
                            .unwrap_or_default(),
                    )
                })
        })
    }

    fn matches_tree(
        &self,
        path: &[u8],
        db: &FileObjectDatabase,
        format: ObjectFormat,
        tree_oid: &ObjectId,
    ) -> Result<bool> {
        let Some(root) = self.worktree_root.as_deref() else {
            return Ok(self.matches(path));
        };
        Ok(self.matches_inner(path, |filter, path| {
            grep_pathspec_match(&filter.element, path)
                && pathspec_attrs_match_with(&filter.element, |requested| {
                    attribute_checks_for_matching(
                        sley_worktree::standard_attributes_for_path_from_tree(
                            root,
                            root.join(".git"),
                            db,
                            format,
                            tree_oid,
                            path,
                            requested,
                            false,
                        )
                        .unwrap_or_default(),
                    )
                })
        }))
    }

    fn matches_inner(
        &self,
        path: &[u8],
        mut matches: impl FnMut(&GrepPathFilter, &[u8]) -> bool,
    ) -> bool {
        if self.filters.is_empty() {
            return self.prefix.is_empty() || path_under_prefix(path, &self.prefix);
        }
        let mut have_include = false;
        let mut included = false;
        for filter in &self.filters {
            if filter.element.is_exclude() {
                if matches(filter, path) {
                    filter.matched.set(true);
                    return false;
                }
            } else {
                have_include = true;
                if matches(filter, path) {
                    filter.matched.set(true);
                    included = true;
                }
            }
        }
        if have_include {
            included
        } else {
            self.prefix.is_empty() || path_under_prefix(path, &self.prefix)
        }
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
            if filter.element.is_exclude() || !grep_pathspec_match(&filter.element, path) {
                continue;
            }
            if !filter.is_dir_spec {
                // Glob pathspec: no depth limit.
                return true;
            }
            let base = filter.element.pattern();
            let rest = if base.is_empty() {
                path
            } else if path == base {
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
            if !filter.element.is_exclude() && !filter.matched.get() {
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

fn grep_pathspec_match(spec: &sley_pathspec::PathspecElement, path: &[u8]) -> bool {
    spec.matches_path(path)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use sley_grep::{Regex, RegexMode, contains};

    fn regex_match(pattern: &str, extended: bool, text: &str) -> bool {
        let mode = if extended {
            RegexMode::Ere
        } else {
            RegexMode::Bre
        };
        let re = Regex::compile(pattern, mode, false, false).expect("compile");
        re.find_from(text.as_bytes(), 0).is_some()
    }

    fn pcre_match(pattern: &str, text: &str) -> bool {
        let re = Regex::compile(pattern, RegexMode::Pcre, false, false).expect("compile");
        re.find_from(text.as_bytes(), 0).is_some()
    }

    fn pcre_find(pattern: &str, text: &str) -> Option<(usize, usize)> {
        let re = Regex::compile(pattern, RegexMode::Pcre, false, false).expect("compile");
        re.find_from(text.as_bytes(), 0)
    }

    #[test]
    fn pcre_escapes_and_classes() {
        assert!(pcre_match(r"a\x{2b}b\x{2a}c", "xa+b*cy"));
        assert!(pcre_match(r"[\d]", "abc5"));
        assert!(!pcre_match(r"[\d]", "abc"));
        assert!(pcre_match(r"[\d]\s", "a 5 b"));
        assert!(pcre_match(r"\d+", "abc123"));
        assert!(!pcre_match(r"\D", "123"));
        assert!(pcre_match(r"[^\d]+", "12a3"));
    }

    #[test]
    fn pcre_unicode_categories_ascii() {
        assert!(pcre_match(
            r"\p{Ps}.*?\p{Pe}",
            "printf(\"Hello world.\\n\");"
        ));
        assert!(!pcre_match(r"\p{Ps}\p{Pe}", "no parens here"));
        assert!(pcre_match(r"(*NO_JIT)\p{Ps}.*?\p{Pe}", "f(x)"));
        assert!(pcre_match(r"\p{L}+", "word"));
    }

    #[test]
    fn pcre_lazy_quantifier_is_shortest_match() {
        // Greedy spans to the last `)`; lazy stops at the first.
        assert_eq!(pcre_find(r"\(.*?\)", "(a)(b)"), Some((0, 3)));
        assert_eq!(pcre_find(r"\(.*\)", "(a)(b)"), Some((0, 6)));
    }

    #[test]
    fn pcre_backreferences() {
        assert!(pcre_match(r"(.)\1", "hello")); // "ll"
        assert!(!pcre_match(r"(.)\1", "abcd"));
        assert!(pcre_match(r"(?P<one>.)(?P=one)", "hello"));
        assert!(!pcre_match(r"(?P<one>.)(?P=one)", "abcd"));
    }

    #[test]
    fn pcre_inline_ignore_case_group() {
        let re = Regex::compile("He((?i)ll)o", RegexMode::Pcre, false, false).expect("compile");
        assert!(re.find_from(b"Hello", 0).is_some());
        assert!(re.find_from(b"HeLLo", 0).is_some());
        assert!(re.find_from(b"hello", 0).is_none()); // leading H stays case-sensitive
    }

    #[test]
    fn pcre_non_capturing_group_and_alternation() {
        assert!(pcre_match(r"(?:foo|bar)baz", "xbarbazy"));
        assert!(!pcre_match(r"(?:foo|bar)baz", "xbaz"));
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
    fn unicode_icase_matches_literals_fixed_strings_and_classes() {
        assert!(contains(
            "TILRAUN: Halló Heimur!".as_bytes(),
            "HALLÓ".as_bytes(),
            true
        ));

        let bre = Regex::compile("HALLÓ", RegexMode::Bre, true, false).expect("compile BRE");
        assert!(bre.find_from("Halló".as_bytes(), 0).is_some());

        let pcre = Regex::compile("[Æ]\0Ð", RegexMode::Pcre, true, false).expect("compile PCRE");
        assert_eq!(pcre.find_from("æ\0ð".as_bytes(), 0), Some((0, 5)));
    }

    #[test]
    fn pcre_utf8_atoms_quantify_and_report_full_byte_spans() {
        let repeated = Regex::compile("ó+", RegexMode::Pcre, false, false).expect("compile");
        assert_eq!(repeated.find_from("xóó".as_bytes(), 0), Some((1, 5)));

        let dot = Regex::compile("ll.", RegexMode::Pcre, false, false).expect("compile");
        assert_eq!(dot.find_from("Halló".as_bytes(), 0), Some((2, 6)));
    }

    #[test]
    fn unicode_icase_preserves_invalid_utf8_subject_bytes() {
        let pcre = Regex::compile("Æ", RegexMode::Pcre, true, false).expect("compile");
        assert_eq!(pcre.find_from(b"\x80\n\xc3\xa6", 0), Some((2, 4)));
    }

    #[test]
    fn wildmatch_crosses_slash() {
        assert!(grep_test_pathspec(b"*.txt").matches_path(b"sub/c.txt"));
        assert!(grep_test_pathspec(b"sub/*").matches_path(b"sub/c.txt"));
        assert!(!grep_test_pathspec(b"*.rs").matches_path(b"sub/c.txt"));
        assert!(grep_test_pathspec(b"a?c").matches_path(b"abc"));
    }

    #[test]
    fn pathspec_dir_prefix_matches() {
        assert!(grep_pathspec_match(
            &grep_test_pathspec(b"sub"),
            b"sub/c.txt"
        ));
        assert!(grep_pathspec_match(
            &grep_test_pathspec(b"sub/c.txt"),
            b"sub/c.txt"
        ));
        assert!(!grep_pathspec_match(
            &grep_test_pathspec(b"sub"),
            b"submarine"
        ));
    }

    fn grep_test_pathspec(pattern: &[u8]) -> sley_pathspec::PathspecElement {
        sley_pathspec::PathspecElement::parse(pattern, sley_pathspec::PathspecMatchMagic::default())
            .expect("test pathspec parses")
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

    #[test]
    fn pager_basename_strips_dir_for_less_and_vi() {
        // git's `len > 4 && is_dir_sep(pager[len-5])` trailing-4 rule.
        assert_eq!(pager_basename("./less"), "less");
        assert_eq!(pager_basename("/usr/bin/less"), "less");
        assert_eq!(pager_basename("less"), "less");
        assert_eq!(pager_basename("vi"), "vi");
        assert_eq!(pager_basename("printf x"), "printf x");
    }

    #[test]
    fn submodule_prefix_joins_with_trailing_slash() {
        assert_eq!(join_prefix(b"", b"submodule"), b"submodule");
        assert_eq!(submodule_prefix(b"", b"submodule"), b"submodule/");
        assert_eq!(submodule_prefix(b"submodule/", b"sub"), b"submodule/sub/");
        assert_eq!(join_prefix(b"submodule/", b"a"), b"submodule/a");
    }
}
