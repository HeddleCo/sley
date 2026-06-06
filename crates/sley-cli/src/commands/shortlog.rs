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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShortlogGroup {
    Author,
    Committer,
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
    group: ShortlogGroup,
    numbered: bool,
    summary: bool,
    email: bool,
    wrap: Option<ShortlogWrap>,
    author_patterns: Vec<LogFilterPattern>,
    grep_patterns: Vec<LogFilterPattern>,
    regexp_mode: SimpleLogRegexMode,
    regexp_ignore_case: bool,
    max_count: Option<usize>,
    revisions: Vec<String>,
    paths_present: bool,
}

impl Default for ShortlogOptions {
    fn default() -> Self {
        Self {
            group: ShortlogGroup::Author,
            numbered: false,
            summary: false,
            email: false,
            wrap: None,
            author_patterns: Vec::new(),
            grep_patterns: Vec::new(),
            regexp_mode: SimpleLogRegexMode::Basic,
            regexp_ignore_case: false,
            max_count: None,
            revisions: Vec::new(),
            paths_present: false,
        }
    }
}

/// One author/committer bucket: the display key plus its subjects (oldest first).
struct ShortlogEntry {
    key: String,
    subjects: Vec<String>,
}

pub(crate) fn cmd_shortlog(args: &[String]) -> Result<()> {
    let options = parse_shortlog_args(args)?;

    if options.paths_present {
        // Pathspec limiting needs the diff machinery to decide which commits
        // touched a path; rather than silently ignore it (and report wrong
        // counts) we surface an explicit, non-zero failure.
        eprintln!("fatal: shortlog pathspec limiting is not supported");
        return Err(GitError::Exit(128));
    }

    let mut groups: Vec<ShortlogEntry> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();

    if options.revisions.is_empty() {
        read_shortlog_from_stdin(&options, &mut groups, &mut index)?;
    } else {
        read_shortlog_from_revisions(&options, &mut groups, &mut index)?;
    }

    sort_shortlog_groups(&mut groups, options.numbered);
    print_shortlog(&options, &groups);
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
            // Everything after `--` is a pathspec; presence is all we track.
            options.paths_present = true;
            continue;
        }
        match arg.as_str() {
            "--" => {
                no_more_options = true;
                options.paths_present = true;
            }
            "-h" | "--help" => return shortlog_usage_help(),
            "--committer" => options.group = ShortlogGroup::Committer,
            "--no-committer" => options.group = ShortlogGroup::Author,
            "--numbered" => options.numbered = true,
            "--no-numbered" => options.numbered = false,
            "--summary" => options.summary = true,
            "--no-summary" => options.summary = false,
            "--email" => options.email = true,
            "--no-email" => options.email = false,
            "--no-group" => options.group = ShortlogGroup::Author,
            "--group" => {
                let value = iter
                    .next()
                    .ok_or_else(|| shortlog_option_requires_value("group"))?;
                options.group = parse_shortlog_group(value)?;
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
                options.max_count = Some(parse_shortlog_count(value)?);
            }
            value => {
                if let Some(rest) = value.strip_prefix("--group=") {
                    options.group = parse_shortlog_group(rest)?;
                } else if let Some(rest) = value.strip_prefix("--author=") {
                    options
                        .author_patterns
                        .push(LogFilterPattern::new(rest, "header"));
                } else if let Some(rest) = value.strip_prefix("--grep=") {
                    options
                        .grep_patterns
                        .push(LogFilterPattern::new(rest, "command line"));
                } else if let Some(rest) = value.strip_prefix("--max-count=") {
                    options.max_count = Some(parse_shortlog_count(rest)?);
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
                    options.revisions.push(value.to_string());
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
        options.max_count = Some(shortlog_parse_revision_number(body)?);
        return Ok(());
    }

    let mut idx = 0;
    while idx < bytes.len() {
        match bytes[idx] {
            b'c' => options.group = ShortlogGroup::Committer,
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
                options.max_count = Some(shortlog_parse_revision_number(&body[idx..])?);
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
    let repo = RepositoryContext::discover_current()?;
    let format = repo.format();
    let db = repo.objects();

    let author_filters = parse_log_filter_patterns(&options.author_patterns, options.regexp_mode)?;
    let grep_filters = parse_log_filter_patterns(&options.grep_patterns, options.regexp_mode)?;

    // Split the revision arguments into positive tips, negative tips, and ranges
    // exactly as the rev-list/log machinery does, so `A..B`, `A...B`, and `^X`
    // forms all behave identically to upstream.
    let mut includes: Vec<String> = Vec::new();
    let mut excludes: Vec<String> = Vec::new();
    let mut linear_ranges: Vec<(String, String, bool)> = Vec::new();
    let mut symmetric_ranges: Vec<(String, String, bool)> = Vec::new();
    for rev in &options.revisions {
        add_rev_list_revision_arg(
            rev,
            false,
            &mut includes,
            &mut excludes,
            &mut linear_ranges,
            &mut symmetric_ranges,
        )?;
    }

    let mut starts = Vec::new();
    let mut symmetric_excludes = Vec::new();
    for rev in includes {
        let oid = repo.resolve_revision(&rev)?;
        starts.push(sley_rev::peel_to_commit(db, format, &oid)?);
    }
    for (left, right, not) in linear_ranges {
        let left_oid = repo.resolve_revision(&left)?;
        let left_oid = sley_rev::peel_to_commit(db, format, &left_oid)?;
        let right_oid = repo.resolve_revision(&right)?;
        let right_oid = sley_rev::peel_to_commit(db, format, &right_oid)?;
        if not {
            starts.push(left_oid);
            symmetric_excludes.push(right_oid);
        } else {
            symmetric_excludes.push(left_oid);
            starts.push(right_oid);
        }
    }
    for (left, right, not) in symmetric_ranges {
        let left_oid = repo.resolve_revision(&left)?;
        let left_oid = sley_rev::peel_to_commit(db, format, &left_oid)?;
        let right_oid = repo.resolve_revision(&right)?;
        let right_oid = sley_rev::peel_to_commit(db, format, &right_oid)?;
        let bases = merge_bases(db, format, &left_oid, &right_oid)?;
        if not {
            starts.extend(bases);
            symmetric_excludes.push(left_oid);
            symmetric_excludes.push(right_oid);
        } else {
            starts.push(left_oid);
            starts.push(right_oid);
            symmetric_excludes.extend(bases);
        }
    }

    // Everything reachable from a negative tip is removed from the result set.
    let mut excluded = HashSet::new();
    for oid in symmetric_excludes {
        for record in rev_list_walk_commits(db, format, [oid], false)? {
            excluded.insert(record.oid);
        }
    }
    for rev in excludes {
        let oid = repo.resolve_revision(&rev)?;
        let oid = sley_rev::peel_to_commit(db, format, &oid)?;
        for record in rev_list_walk_commits(db, format, [oid], false)? {
            excluded.insert(record.oid);
        }
    }

    // `walk_commits` yields newest-first; prepending into each bucket therefore
    // leaves subjects oldest-first, matching git's output ordering.
    let commits = rev_list_walk_commits(db, format, starts, false)?;
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
        if let Some(max_count) = options.max_count
            && emitted >= max_count
        {
            break;
        }
        emitted += 1;
        let identity = match options.group {
            ShortlogGroup::Author => &record.commit.author,
            ShortlogGroup::Committer => &record.commit.committer,
        };
        let key = shortlog_identity_key(identity, options.email);
        let subject = shortlog_subject(&record.commit.message);
        push_shortlog_commit_front(groups, index, key, subject);
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
    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;

    // git matches both the human `git log` headers and the raw commit-object
    // headers, so a `git cat-file commit` stream works too.
    let (pretty_label, raw_label) = match options.group {
        ShortlogGroup::Author => ("Author: ", "author "),
        ShortlogGroup::Committer => ("Commit: ", "committer "),
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
        if let Some(key) = shortlog_stdin_identity_key(identity, options.email) {
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
        groups.sort_by(|a, b| b.subjects.len().cmp(&a.subjects.len()));
    }
}

fn print_shortlog(options: &ShortlogOptions, groups: &[ShortlogEntry]) {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    // Best-effort writing: a closed pipe should not panic the process.
    let _ = write_shortlog(&mut out, options, groups);
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
fn shortlog_identity_key(raw: &[u8], email: bool) -> String {
    let (name, addr) = commit_identity_name_email(raw);
    if email {
        format!("{name} <{addr}>")
    } else {
        name
    }
}

/// Build the display key from a `git log` `Author:`/`Commit:` value (already a
/// `Name <email>` string). git only counts commits whose identity carries an
/// email, so identities lacking `<...>` are dropped (returns `None`).
fn shortlog_stdin_identity_key(identity: &str, email: bool) -> Option<String> {
    let (name, addr) = match identity.rsplit_once(" <") {
        Some((name, rest)) => (name.to_string(), rest.trim_end_matches('>').to_string()),
        None => return None,
    };
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
        // git also supports `--group=trailer:<token>` and `--group=format:<fmt>`;
        // those are not modelled here. Match git's diagnostic for an unknown
        // field rather than silently mis-grouping.
        other => {
            eprintln!("error: unknown group type: {other}");
            Err(GitError::Exit(129))
        }
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
