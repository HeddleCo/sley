//! `git shortlog`: summarize `git log` output grouped by author (or committer).
//!
//! Two input modes mirror upstream git: when one or more revisions are given on
//! the command line the revision graph is walked directly; otherwise the commit
//! summaries are read from standard input (the classic
//! `git log | git shortlog` pipeline). Output is the per-group commit count plus,
//! unless `--summary` is requested, the folded subject of every commit indented
//! beneath a `Name (count):` header.

// Command modules pull their shared plumbing from the crate root. A glob import
// works because a submodule can access its ancestor module's items (including
// private ones), so every helper, type, and re-export visible at the crate root
// (RepositoryContext, read_repo_config, rev_list_walk_commits,
// commit_identity_name_email, the `std::*` re-exports, ...) is in scope here
// without re-listing it.
use crate::*;

/// Which identity a commit is grouped under.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ShortlogGroup {
    Author,
    Committer,
    Trailer(String),
    Format(String),
}

/// Parsed line-wrap configuration for `-w[<width>[,<indent1>[,<indent2>]]]`.
///
/// `width == 0` disables wrapping entirely (git's documented escape hatch).
#[derive(Debug, Clone, Copy)]
struct ShortlogWrap {
    width: usize,
    indent1: usize,
    indent2: usize,
}

#[derive(Debug)]
struct ShortlogOptions {
    groups: Vec<ShortlogGroup>,
    numbered: bool,
    summary: bool,
    email: bool,
    wrap: Option<ShortlogWrap>,
    output: Option<String>,
    format: Option<String>,
    abbrev_len: Option<usize>,
    date_mode: DateMode,
    author_patterns: Vec<LogFilterPattern>,
    grep_patterns: Vec<LogFilterPattern>,
    regexp_mode: SimpleLogRegexMode,
    regexp_ignore_case: bool,
    setup_args: Vec<String>,
    has_input_specs: bool,
}

impl Default for ShortlogOptions {
    fn default() -> Self {
        Self {
            groups: Vec::new(),
            numbered: false,
            summary: false,
            email: false,
            wrap: None,
            output: None,
            format: None,
            abbrev_len: Some(7),
            date_mode: DateMode::Default,
            author_patterns: Vec::new(),
            grep_patterns: Vec::new(),
            regexp_mode: SimpleLogRegexMode::Basic,
            regexp_ignore_case: false,
            setup_args: Vec::new(),
            has_input_specs: false,
        }
    }
}

/// One author/committer bucket: the display key plus its subjects (oldest first).
struct ShortlogEntry {
    key: String,
    subjects: Vec<String>,
}

pub(crate) fn cmd_shortlog(args: &[String]) -> Result<()> {
    let mut options = parse_shortlog_args(args)?;
    if options.groups.is_empty() {
        options.groups.push(ShortlogGroup::Author);
    }

    let mut groups: Vec<ShortlogEntry> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();

    if !options.has_input_specs {
        read_shortlog_from_stdin(&options, &mut groups, &mut index)?;
    } else {
        read_shortlog_from_revisions(&options, &mut groups, &mut index)?;
    }

    sort_shortlog_groups(&mut groups, options.numbered);
    print_shortlog(&options, &groups)?;
    Ok(())
}

/// Parse the command line into [`ShortlogOptions`]. Every error path (unknown
/// option, malformed value, `-h`) funnels through `Err(GitError::Exit(...))` after
/// emitting git-compatible diagnostics.
fn parse_shortlog_args(args: &[String]) -> Result<ShortlogOptions> {
    let mut options = ShortlogOptions::default();
    let mut iter = args.iter().peekable();
    let mut no_more_options = false;
    while let Some(arg) = iter.next() {
        if no_more_options {
            options.setup_args.push(arg.clone());
            continue;
        }
        match arg.as_str() {
            "--" => {
                options.setup_args.push(arg.clone());
                options.has_input_specs = true;
                no_more_options = true;
            }
            "-h" | "--help" => return shortlog_usage_help(),
            "--committer" => options.groups = vec![ShortlogGroup::Committer],
            "--no-committer" => options.groups = vec![ShortlogGroup::Author],
            "--numbered" => options.numbered = true,
            "--no-numbered" => options.numbered = false,
            "--summary" => options.summary = true,
            "--no-summary" => options.summary = false,
            "--email" => options.email = true,
            "--no-email" => options.email = false,
            "--no-group" => options.groups.clear(),
            "--group" => {
                let value = iter
                    .next()
                    .ok_or_else(|| shortlog_option_requires_value("group"))?;
                options.groups.push(parse_shortlog_group(value)?);
            }
            "--format" | "--pretty" => {
                let value = iter
                    .next()
                    .ok_or_else(|| shortlog_option_requires_value(arg.trim_start_matches("--")))?;
                options.format = Some(shortlog_pretty_format_value(value)?);
            }
            "--date" => {
                let value = iter
                    .next()
                    .ok_or_else(|| shortlog_option_requires_value("date"))?;
                options.date_mode = log_date_mode(value)?;
            }
            "--abbrev" => {
                let value = iter
                    .next()
                    .ok_or_else(|| shortlog_option_requires_value("abbrev"))?;
                options.abbrev_len =
                    Some(value.parse::<usize>().map_err(|_| {
                        GitError::Command(format!("invalid abbrev length {value}"))
                    })?);
            }
            "--output" => {
                let value = iter
                    .next()
                    .ok_or_else(|| shortlog_option_requires_value("output"))?;
                options.output = Some(value.clone());
            }
            "--author" => {
                // `--author`/`--grep` are consumed by git's revision machinery,
                // not shortlog's own parser, so reuse the log-style diagnostics.
                let value = iter.next().ok_or_else(log_author_requires_value_error)?;
                options
                    .author_patterns
                    .push(LogFilterPattern::new(value, "header"));
            }
            "--grep" => {
                let value = iter.next().ok_or_else(log_grep_requires_value_error)?;
                options
                    .grep_patterns
                    .push(LogFilterPattern::new(value, "command line"));
            }
            "--regexp-ignore-case" => options.regexp_ignore_case = true,
            "--no-regexp-ignore-case" => options.regexp_ignore_case = false,
            "--fixed-strings" => options.regexp_mode = SimpleLogRegexMode::Fixed,
            "--extended-regexp" | "--basic-regexp" => {
                options.regexp_mode = SimpleLogRegexMode::Basic;
            }
            "--max-count" => {
                let value = iter
                    .next()
                    .ok_or_else(|| shortlog_option_requires_value("max-count"))?;
                options.setup_args.push(arg.clone());
                options.setup_args.push(value.clone());
            }
            "--all" | "--branches" => {
                options.setup_args.push(arg.clone());
                options.has_input_specs = true;
            }
            "--exclude" => {
                let value = iter
                    .next()
                    .ok_or_else(|| shortlog_option_requires_value("exclude"))?;
                options.setup_args.push(arg.clone());
                options.setup_args.push(value.clone());
                options.has_input_specs = true;
            }
            value => {
                if let Some(rest) = value.strip_prefix("--group=") {
                    options.groups.push(parse_shortlog_group(rest)?);
                } else if let Some(rest) = value.strip_prefix("--author=") {
                    options
                        .author_patterns
                        .push(LogFilterPattern::new(rest, "header"));
                } else if let Some(rest) = value.strip_prefix("--grep=") {
                    options
                        .grep_patterns
                        .push(LogFilterPattern::new(rest, "command line"));
                } else if let Some(rest) = value.strip_prefix("--format=") {
                    options.format = Some(shortlog_pretty_format_value(rest)?);
                } else if let Some(rest) = value.strip_prefix("--pretty=") {
                    options.format = Some(shortlog_pretty_format_value(rest)?);
                } else if let Some(rest) = value.strip_prefix("--date=") {
                    options.date_mode = log_date_mode(rest)?;
                } else if let Some(rest) = value.strip_prefix("--abbrev=") {
                    options.abbrev_len =
                        Some(rest.parse::<usize>().map_err(|_| {
                            GitError::Command(format!("invalid abbrev length {rest}"))
                        })?);
                } else if let Some(rest) = value.strip_prefix("--output=") {
                    options.output = Some(rest.to_string());
                } else if let Some(rest) = value.strip_prefix("--max-count=") {
                    parse_shortlog_count(rest)?;
                    options.setup_args.push(value.to_string());
                } else if value.starts_with("--exclude=") {
                    options.setup_args.push(value.to_string());
                    options.has_input_specs = true;
                } else if let Some(option) = shortlog_boolean_option_with_value(value) {
                    // Boolean flags reject an attached `=value`, matching git's
                    // `option '<name>' takes no value` diagnostic.
                    return shortlog_option_takes_no_value(option);
                } else if value.starts_with("--") {
                    return shortlog_unknown_option(value);
                } else if value == "-" {
                    // A lone dash is not a stdin sentinel here; git's revision
                    // parser rejects it outright.
                    return shortlog_unrecognized_argument(value);
                } else if value.starts_with('-') {
                    // A single-dash token is either the `-<n>` revision shorthand
                    // or a bundle of short flags (`-sne`, `-w50`, ...).
                    apply_shortlog_short_bundle(value, &mut options)?;
                } else {
                    options.setup_args.push(value.to_string());
                    options.has_input_specs = true;
                }
            }
        }
    }
    Ok(options)
}

/// Apply a single-dash token: either the `-<n>` revision shorthand or a bundle of
/// short flags. Mirrors git's parse_options short-option handling, including the
/// way `-w` and an embedded digit greedily consume the rest of the bundle as their
/// argument, and the diagnostics it emits on the way out.
fn apply_shortlog_short_bundle(value: &str, options: &mut ShortlogOptions) -> Result<()> {
    let body = value.strip_prefix('-').unwrap_or(value);
    let bytes = body.as_bytes();

    // A token whose first character is a digit is the revision `-<n>` shorthand and
    // is handled wholesale by the revision parser, not as a flag bundle.
    if bytes.first().is_some_and(u8::is_ascii_digit) {
        shortlog_parse_revision_number(body)?;
        options.setup_args.push(value.to_string());
        return Ok(());
    }

    let mut idx = 0;
    while idx < bytes.len() {
        match bytes[idx] {
            b'c' => options.groups = vec![ShortlogGroup::Committer],
            b'n' => options.numbered = true,
            b's' => options.summary = true,
            b'e' => options.email = true,
            b'i' => options.regexp_ignore_case = true,
            b'F' => options.regexp_mode = SimpleLogRegexMode::Fixed,
            b'E' | b'G' => options.regexp_mode = SimpleLogRegexMode::Basic,
            b'w' => {
                // `-w` takes an optional argument: the remainder of the bundle.
                options.wrap = Some(parse_shortlog_wrap(&body[idx + 1..])?);
                return Ok(());
            }
            byte if byte.is_ascii_digit() => {
                // A digit mid-bundle (e.g. `-sn2`) begins the max-count argument,
                // consuming the rest of the bundle.
                shortlog_parse_revision_number(&body[idx..])?;
                options.setup_args.push(format!("-{}", &body[idx..]));
                return Ok(());
            }
            _ => {
                // Unknown short option: git reports `-` plus the unconsumed tail.
                return shortlog_unknown_short_option(&body[idx..]);
            }
        }
        idx += 1;
    }
    Ok(())
}

/// Parse a revision `-<n>` count, emitting git's `fatal: '<v>': not an integer`
/// (exit 128) when the tail is not a plain non-negative integer.
fn shortlog_parse_revision_number(value: &str) -> Result<usize> {
    let parsed = if value.bytes().all(|byte| byte.is_ascii_digit()) {
        value.parse::<usize>().ok()
    } else {
        None
    };
    parsed.ok_or_else(|| {
        eprintln!("fatal: '{value}': not an integer");
        GitError::Exit(128)
    })
}

fn shortlog_unknown_short_option(tail: &str) -> Result<()> {
    eprint!("error: unknown option `-{tail}'\n{SHORTLOG_USAGE}");
    Err(GitError::Exit(129))
}

/// Resolve every revision argument, walk the graph, apply filters/limit, and
/// fold each commit's subject into its author (or committer) bucket.
fn read_shortlog_from_revisions(
    options: &ShortlogOptions,
    groups: &mut Vec<ShortlogEntry>,
    index: &mut HashMap<String, usize>,
) -> Result<()> {
    let repo = match RepositoryContext::discover_current() {
        Ok(repo) => repo,
        Err(err) => {
            if !options.setup_args.is_empty() {
                eprintln!("fatal: too many arguments");
                return Err(GitError::Exit(128));
            }
            return Err(err);
        }
    };
    let format = repo.format();
    let db = repo.objects();
    let setup = sley_rev::setup_revisions(
        &options.setup_args,
        &sley_rev::RevisionSetupContext {
            git_dir: repo.git_dir(),
            worktree_root: repo.worktree_root().ok(),
            cwd: repo.cwd(),
            format,
            reader: db,
            config: Some(repo.config()),
        },
    )?;
    if let Some(leftover) = setup.leftovers.first() {
        return Err(GitError::Command(format!(
            "unsupported shortlog option {leftover}"
        )));
    }
    if !setup.pathspecs.is_empty() {
        // Pathspec limiting needs the diff machinery to decide which commits
        // touched a path; rather than silently ignore it (and report wrong
        // counts) we surface an explicit, non-zero failure.
        eprintln!("fatal: shortlog pathspec limiting is not supported");
        return Err(GitError::Exit(128));
    }

    // git's shortlog *always* mailmaps the grouping identity (no flag needed).
    let mailmap = commands::utility::Mailmap::load_default(repo.git_dir(), format)?;

    let author_filters = parse_log_filter_patterns(&options.author_patterns, options.regexp_mode)?;
    let grep_filters = parse_log_filter_patterns(&options.grep_patterns, options.regexp_mode)?;

    let mut starts = Vec::new();
    for tip in &setup.options.positives {
        starts.push(sley_rev::peel_to_commit(db, format, &tip.oid)?);
    }

    // Everything reachable from a negative tip is removed from the result set.
    let mut excluded = HashSet::new();
    for oid in &setup.options.negatives {
        for record in rev_list_walk_commits(db, format, [*oid], setup.options.first_parent)? {
            excluded.insert(record.oid);
        }
    }

    // `walk_commits` yields newest-first; prepending into each bucket therefore
    // leaves subjects oldest-first, matching git's output ordering.
    let commits = rev_list_walk_commits(db, format, starts, setup.options.first_parent)?;
    let mut emitted = 0usize;
    for record in &commits {
        if excluded.contains(&record.oid) {
            continue;
        }
        if !log_author_filters_match(record, &author_filters, options.regexp_ignore_case) {
            continue;
        }
        if !log_grep_filters_match(
            record,
            &grep_filters,
            false,
            false,
            options.regexp_ignore_case,
        ) {
            continue;
        }
        if emitted < setup.options.skip {
            emitted += 1;
            continue;
        }
        if let Some(max_count) = setup.options.max_count
            && emitted.saturating_sub(setup.options.skip) >= max_count
        {
            break;
        }
        emitted += 1;
        let subject = shortlog_commit_subject(record, options, &mailmap)?;
        let mut seen_keys = HashSet::new();
        for key in shortlog_group_keys(record, options, &mailmap)? {
            if seen_keys.insert(key.clone()) {
                push_shortlog_commit_front(groups, index, key, subject.clone());
            }
        }
    }
    Ok(())
}

/// Parse `git log`-style records from standard input, matching git's
/// `read_from_stdin`. The scan is *identity-driven*: a line beginning with the
/// grouping prefix (`Author: `/`author ` for author grouping, `Commit: `/
/// `committer ` for committer grouping) opens a record. git then (1) skips the
/// remaining non-empty header lines up to the blank separator, (2) skips the blank
/// lines, and (3) takes the next line as the commit summary (leading/trailing
/// whitespace trimmed). Everything else — `commit <oid>`, `Date:`, `Merge:`, body
/// paragraphs — is ignored, and `--max-count`/`-<n>` has no effect here, exactly
/// as upstream (it is a revision-walk concept).
fn read_shortlog_from_stdin(
    options: &ShortlogOptions,
    groups: &mut Vec<ShortlogEntry>,
    index: &mut HashMap<String, usize>,
) -> Result<()> {
    if options.groups.len() > 1 {
        eprintln!("fatal: stdin shortlog does not support multiple groups");
        return Err(GitError::Exit(128));
    }
    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;

    // git's shortlog always mailmaps. The stdin path may run outside a repo
    // (`git log ... | git shortlog`), so prefer the repo mailmap when one is
    // discoverable and fall back to a cwd-relative `.mailmap` otherwise.
    let mailmap = match RepositoryContext::discover_current() {
        Ok(repo) => {
            let format = repo.format();
            commands::utility::Mailmap::load_default(repo.git_dir(), format)?
        }
        Err(_) => commands::utility::Mailmap::load_cwd()?,
    };

    // git matches both the human `git log` headers and the raw commit-object
    // headers, so a `git cat-file commit` stream works too.
    let (pretty_label, raw_label) = match options.groups.first().unwrap_or(&ShortlogGroup::Author) {
        ShortlogGroup::Author => ("Author: ", "author "),
        ShortlogGroup::Committer => ("Commit: ", "committer "),
        ShortlogGroup::Trailer(_) | ShortlogGroup::Format(_) => return Ok(()),
    };

    let lines: Vec<&str> = input.lines().collect();
    let mut idx = 0;
    while idx < lines.len() {
        let Some(identity) = lines[idx]
            .strip_prefix(pretty_label)
            .or_else(|| lines[idx].strip_prefix(raw_label))
        else {
            idx += 1;
            continue;
        };
        idx += 1;
        // Phase 1: consume the rest of the header block (non-empty lines) until the
        // blank separator. git's blank test is `oneline.len`, so a whitespace-only
        // line counts as content and ends this phase.
        while idx < lines.len() && !lines[idx].is_empty() {
            idx += 1;
        }
        // Phase 2: skip the blank separator lines.
        while idx < lines.len() && lines[idx].is_empty() {
            idx += 1;
        }
        // Phase 3: the next line (if any) is the summary.
        let summary = if idx < lines.len() {
            let line = lines[idx];
            idx += 1;
            line
        } else {
            ""
        };
        // The identity is taken verbatim after the prefix — git does NOT trim it,
        // so the extra alignment spaces in `--format=fuller` (`Author:     X`)
        // become part of the grouping key, matching upstream byte-for-byte. The
        // summary, by contrast, has both ends trimmed.
        if let Some(key) = shortlog_stdin_identity_key(identity, options.email, &mailmap) {
            // stdin records arrive newest-first (matching `git log`); prepend so
            // each group lists oldest-first like the revision-walk path.
            push_shortlog_commit_front(groups, index, key, summary.trim().to_string());
        }
    }
    Ok(())
}

/// Insert `subject` at the front of its bucket so a newest-first commit stream
/// ends up oldest-first within each group, mirroring git.
fn push_shortlog_commit_front(
    groups: &mut Vec<ShortlogEntry>,
    index: &mut HashMap<String, usize>,
    key: String,
    subject: String,
) {
    match index.get(&key) {
        Some(&pos) => groups[pos].subjects.insert(0, subject),
        None => {
            index.insert(key.clone(), groups.len());
            groups.push(ShortlogEntry {
                key,
                subjects: vec![subject],
            });
        }
    }
}

/// Order groups for display: alphabetically by key, then (for `--numbered`) a
/// stable re-sort by descending commit count. The stable pass preserves the
/// alphabetical tie-break git applies.
fn sort_shortlog_groups(groups: &mut [ShortlogEntry], numbered: bool) {
    groups.sort_by(|a, b| a.key.as_bytes().cmp(b.key.as_bytes()));
    if numbered {
        groups.sort_by_key(|group| std::cmp::Reverse(group.subjects.len()));
    }
}

fn print_shortlog(options: &ShortlogOptions, groups: &[ShortlogEntry]) -> Result<()> {
    if let Some(path) = &options.output {
        let mut out = fs::File::create(path)?;
        write_shortlog(&mut out, options, groups)?;
    } else {
        let stdout = io::stdout();
        let mut out = stdout.lock();
        write_shortlog(&mut out, options, groups)?;
    }
    Ok(())
}

fn write_shortlog(
    out: &mut impl Write,
    options: &ShortlogOptions,
    groups: &[ShortlogEntry],
) -> io::Result<()> {
    if options.summary {
        for entry in groups {
            writeln!(out, "{:6}\t{}", entry.subjects.len(), entry.key)?;
        }
        return Ok(());
    }
    for entry in groups {
        writeln!(out, "{} ({}):", entry.key, entry.subjects.len())?;
        for subject in &entry.subjects {
            match options.wrap {
                Some(wrap) if wrap.width > 0 => {
                    write_shortlog_wrapped(out, subject, wrap)?;
                }
                _ => writeln!(out, "      {subject}")?,
            }
        }
        writeln!(out)?;
    }
    Ok(())
}

/// Emit a single subject wrapped to `wrap.width`, indenting the first physical
/// line by `indent1` and continuation lines by `indent2` (git's `strbuf_add_wrapped_text`).
fn write_shortlog_wrapped(
    out: &mut impl Write,
    subject: &str,
    wrap: ShortlogWrap,
) -> io::Result<()> {
    for line in wrap_shortlog_text(subject, wrap.width, wrap.indent1, wrap.indent2) {
        writeln!(out, "{line}")?;
    }
    Ok(())
}

/// Greedy word-wrap matching git's behaviour: break on spaces, never exceed
/// `width` columns where possible, indent the first line by `indent1` and the
/// rest by `indent2`. A single word longer than the available width is emitted on
/// its own (over-long) line rather than split.
fn wrap_shortlog_text(text: &str, width: usize, indent1: usize, indent2: usize) -> Vec<String> {
    let words: Vec<&str> = text.split(' ').filter(|word| !word.is_empty()).collect();
    if words.is_empty() {
        return vec![" ".repeat(indent1)];
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_indent = indent1;
    let mut column = indent1;
    let mut first_word_on_line = true;
    for word in words {
        let word_len = word.chars().count();
        let needed = if first_word_on_line {
            current_indent + word_len
        } else {
            column + 1 + word_len
        };
        if !first_word_on_line && needed > width {
            lines.push(current);
            current = String::new();
            current_indent = indent2;
            column = indent2;
            first_word_on_line = true;
        }
        if first_word_on_line {
            current.push_str(&" ".repeat(current_indent));
            current.push_str(word);
            column = current_indent + word_len;
            first_word_on_line = false;
        } else {
            current.push(' ');
            current.push_str(word);
            column += 1 + word_len;
        }
    }
    if !current.is_empty() || lines.is_empty() {
        lines.push(current);
    }
    lines
}

/// Build the display key for the revision-walk path. `raw` is a full identity line
/// (`Name <email> <ts> <tz>`); we keep `Name` and, with `--email`, append the
/// address as ` <email>` — always including the angle brackets, even when the
/// address is empty, exactly as git renders `Name <>`.
fn shortlog_identity_key(raw: &[u8], email: bool, mailmap: &commands::utility::Mailmap) -> String {
    let (name, addr) = commit_identity_name_email(raw);
    let (name, addr) = mailmap.map_user(&name, &addr);
    if email {
        format!("{name} <{addr}>")
    } else {
        name
    }
}

fn shortlog_group_keys(
    record: &sley_rev::CommitRecord,
    options: &ShortlogOptions,
    mailmap: &commands::utility::Mailmap,
) -> Result<Vec<String>> {
    let mut keys = Vec::new();
    for group in &options.groups {
        match group {
            ShortlogGroup::Author => {
                keys.push(shortlog_identity_key(
                    &record.commit.author,
                    options.email,
                    mailmap,
                ));
            }
            ShortlogGroup::Committer => {
                keys.push(shortlog_identity_key(
                    &record.commit.committer,
                    options.email,
                    mailmap,
                ));
            }
            ShortlogGroup::Trailer(token) => {
                keys.extend(shortlog_trailer_group_keys(
                    &record.commit.message,
                    token,
                    options.email,
                    mailmap,
                )?);
            }
            ShortlogGroup::Format(format) => {
                keys.push(shortlog_render_format(
                    record,
                    format,
                    &options.date_mode,
                    options.abbrev_len,
                    mailmap,
                )?);
            }
        }
    }
    Ok(keys)
}

fn shortlog_commit_subject(
    record: &sley_rev::CommitRecord,
    options: &ShortlogOptions,
    mailmap: &commands::utility::Mailmap,
) -> Result<String> {
    match &options.format {
        Some(format) => {
            let rendered = shortlog_render_format(
                record,
                format,
                &options.date_mode,
                options.abbrev_len,
                mailmap,
            )?;
            Ok(shortlog_subject_from_text(&rendered))
        }
        None => Ok(shortlog_subject(&record.commit.message)),
    }
}

fn shortlog_render_format(
    record: &sley_rev::CommitRecord,
    format: &str,
    date_mode: &DateMode,
    abbrev_len: Option<usize>,
    mailmap: &commands::utility::Mailmap,
) -> Result<String> {
    let compiled = CompiledLogFormat::compile(format, LogFormatDialect::Log)?;
    let decorations = HashMap::new();
    let context = LogFormatContext {
        abbrev_len,
        decorations: &decorations,
        marker: '>',
        dialect: LogFormatDialect::Log,
        source: None,
        date_mode,
        source_oid: None,
        describe: None,
        signature: None,
        color: false,
        output_encoding: "UTF-8",
        mailmap,
        use_mailmap: false,
    };
    let mut out = Vec::with_capacity(compiled.estimated_line_capacity());
    emit_compiled_log_format(
        record,
        &compiled,
        &context,
        &mut out,
        0..compiled.tokens.len(),
    )?;
    Ok(String::from_utf8_lossy(&out).into_owned())
}

fn shortlog_trailer_group_keys(
    message: &[u8],
    token: &str,
    email: bool,
    mailmap: &commands::utility::Mailmap,
) -> Result<Vec<String>> {
    let opts = commands::for_each_ref::parse_for_each_ref_trailer_options(&format!(
        "key={token},valueonly,only,unfold"
    ))
    .map_err(|_| GitError::Command(format!("invalid trailer group {token}")))?;
    let rendered = commands::for_each_ref::for_each_ref_format_trailers(message, &opts);
    let text = String::from_utf8_lossy(&rendered);
    let mut keys = Vec::new();
    let mut seen = HashSet::new();
    for line in text.lines() {
        let key = shortlog_trailer_value_key(line.trim(), email, mailmap);
        if seen.insert(key.clone()) {
            keys.push(key);
        }
    }
    Ok(keys)
}

fn shortlog_trailer_value_key(
    value: &str,
    email: bool,
    mailmap: &commands::utility::Mailmap,
) -> String {
    if let Some(key) = shortlog_stdin_identity_key(value, email, mailmap) {
        key
    } else {
        value.to_string()
    }
}

/// Build the display key from a `git log` `Author:`/`Commit:` value (already a
/// `Name <email>` string). git only counts commits whose identity carries an
/// email, so identities lacking `<...>` are dropped (returns `None`).
fn shortlog_stdin_identity_key(
    identity: &str,
    email: bool,
    mailmap: &commands::utility::Mailmap,
) -> Option<String> {
    let (name, addr) = match identity.rsplit_once(" <") {
        Some((name, rest)) => (name.to_string(), rest.trim_end_matches('>').to_string()),
        None => return None,
    };
    let (name, addr) = mailmap.map_user(&name, &addr);
    if email {
        Some(format!("{name} <{addr}>"))
    } else {
        Some(name)
    }
}

/// Fold a commit message into its summary subject (revision-walk path). Mirrors
/// git's `format_subject`: skip leading blank lines, then join message lines up to
/// the first blank line with single spaces (trailing whitespace stripped per
/// line, the first line's leading whitespace trimmed off the joined result).
fn shortlog_subject(message: &[u8]) -> String {
    let text = String::from_utf8_lossy(message);
    shortlog_subject_from_text(&text)
}

fn shortlog_subject_from_text(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    shortlog_fold_subject_lines(&lines)
}

fn shortlog_fold_subject_lines<S: AsRef<str>>(lines: &[S]) -> String {
    let mut folded = String::new();
    let mut started = false;
    for line in lines {
        let line = line.as_ref();
        let trimmed_end = line.trim_end();
        if !started {
            if trimmed_end.trim().is_empty() {
                // Skip leading blank lines.
                continue;
            }
            started = true;
            folded.push_str(trimmed_end);
            continue;
        }
        if trimmed_end.trim().is_empty() {
            // First blank line after the subject terminates it.
            break;
        }
        folded.push(' ');
        folded.push_str(trimmed_end);
    }
    // The first subject line's leading whitespace is dropped; interior leading
    // whitespace on folded continuation lines is preserved.
    folded.trim_start().to_string()
}

fn parse_shortlog_group(value: &str) -> Result<ShortlogGroup> {
    match value {
        "author" => Ok(ShortlogGroup::Author),
        "committer" => Ok(ShortlogGroup::Committer),
        value if value.starts_with("trailer:") => Ok(ShortlogGroup::Trailer(
            value["trailer:".len()..].to_string(),
        )),
        value if value.starts_with("format:") => {
            Ok(ShortlogGroup::Format(value["format:".len()..].to_string()))
        }
        value if value.contains('%') => Ok(ShortlogGroup::Format(value.to_string())),
        other => {
            eprintln!("error: unknown group type: {other}");
            Err(GitError::Exit(129))
        }
    }
}

fn shortlog_pretty_format_value(value: &str) -> Result<String> {
    if let Some(rest) = value.strip_prefix("format:") {
        Ok(rest.to_string())
    } else if let Some(rest) = value.strip_prefix("tformat:") {
        Ok(rest.to_string())
    } else if value.contains('%') {
        Ok(value.to_string())
    } else {
        eprintln!("fatal: invalid --pretty format: {value}");
        Err(GitError::Exit(128))
    }
}

/// Parse the `-w` argument body (`width`, `width,indent1`, or
/// `width,indent1,indent2`). An empty body selects git's defaults (76/6/9). Any
/// malformed component yields git's terse `-w` syntax reminder and exit 129.
fn parse_shortlog_wrap(spec: &str) -> Result<ShortlogWrap> {
    let mut width = 76usize;
    let mut indent1 = 6usize;
    let mut indent2 = 9usize;
    if !spec.is_empty() {
        let mut parts = spec.split(',');
        if let Some(part) = parts.next()
            && !part.is_empty()
        {
            width = parse_shortlog_wrap_field(part)?;
        }
        if let Some(part) = parts.next()
            && !part.is_empty()
        {
            indent1 = parse_shortlog_wrap_field(part)?;
        }
        if let Some(part) = parts.next()
            && !part.is_empty()
        {
            indent2 = parse_shortlog_wrap_field(part)?;
        }
        if parts.next().is_some() {
            return Err(shortlog_wrap_syntax_error());
        }
    }
    Ok(ShortlogWrap {
        width,
        indent1,
        indent2,
    })
}

fn parse_shortlog_wrap_field(value: &str) -> Result<usize> {
    value
        .parse::<usize>()
        .map_err(|_| shortlog_wrap_syntax_error())
}

fn shortlog_wrap_syntax_error() -> GitError {
    eprintln!("error: -w[<width>[,<indent1>[,<indent2>]]]");
    GitError::Exit(129)
}

fn parse_shortlog_count(value: &str) -> Result<usize> {
    value
        .parse::<usize>()
        .map_err(|_| GitError::Command(format!("invalid max-count {value}")))
}

/// If `value` is a boolean long option written with an attached `=...` (e.g.
/// `--committer=foo`, `--numbered=`), return the bare option name so the caller
/// can report that it takes no value. Returns `None` otherwise.
fn shortlog_boolean_option_with_value(value: &str) -> Option<&'static str> {
    const BOOLEAN_OPTIONS: &[&str] = &[
        "committer",
        "no-committer",
        "numbered",
        "no-numbered",
        "summary",
        "no-summary",
        "email",
        "no-email",
        "regexp-ignore-case",
        "no-regexp-ignore-case",
        "fixed-strings",
        "extended-regexp",
        "basic-regexp",
        "no-group",
    ];
    let rest = value.strip_prefix("--")?;
    let name = rest.split_once('=')?.0;
    BOOLEAN_OPTIONS
        .iter()
        .copied()
        .find(|option| *option == name)
}

// git's parse_options distinguishes two error shapes: a *malformed value* for a
// known option prints only the one-line `error: ...` diagnostic, whereas an
// *unknown* option (or stray argument) prints the diagnostic followed by the full
// usage block. We mirror that split precisely.

fn shortlog_option_requires_value(option: &str) -> GitError {
    eprintln!("error: option `{option}' requires a value");
    GitError::Exit(129)
}

fn shortlog_option_takes_no_value(option: &str) -> Result<ShortlogOptions> {
    eprintln!("error: option `{option}' takes no value");
    Err(GitError::Exit(129))
}

fn shortlog_unknown_option(option: &str) -> Result<ShortlogOptions> {
    eprint!("error: unknown option `{option}'\n{SHORTLOG_USAGE}");
    Err(GitError::Exit(129))
}

fn shortlog_unrecognized_argument(value: &str) -> Result<ShortlogOptions> {
    eprint!("error: unrecognized argument: {value}\n{SHORTLOG_USAGE}");
    Err(GitError::Exit(129))
}

fn shortlog_usage_help() -> Result<ShortlogOptions> {
    print!("{SHORTLOG_USAGE}");
    Err(GitError::Exit(129))
}

const SHORTLOG_USAGE: &str = "usage: git shortlog [<options>] [<revision-range>] [[--] <path>...]\n   or: git log --pretty=short | git shortlog [<options>]\n\n    -c, --[no-]committer  group by committer rather than author\n    -n, --[no-]numbered   sort output according to the number of commits per author\n    -s, --[no-]summary    suppress commit descriptions, only provides commit count\n    -e, --[no-]email      show the email address of each author\n    -w[<w>[,<i1>[,<i2>]]] linewrap output\n    --[no-]group <field>  group by field\n\n";
