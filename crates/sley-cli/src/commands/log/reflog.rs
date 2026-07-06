use super::*;

pub(super) struct ReflogWalkOptions<'a> {
    pub(super) max_count: Option<usize>,
    pub(super) skip: usize,
    pub(super) output: &'a LogOutput,
    pub(super) reverse: bool,
    pub(super) date_mode: &'a DateMode,
}

pub(super) fn log_walk_reflogs(
    git_dir: &Path,
    format: ObjectFormat,
    revisions: &[String],
    opts: ReflogWalkOptions<'_>,
) -> Result<()> {
    let store = FileRefStore::new(git_dir, format);
    let mut db = FileObjectDatabase::from_git_dir(git_dir, format);
    let decorations = HashMap::new();
    let mailmap = commands::utility::Mailmap::load_default(git_dir, format)?;
    let mut stdout = io::stdout();
    let references = if revisions.is_empty() {
        vec![ReflogWalkTarget::new(None)?]
    } else {
        revisions
            .iter()
            .map(|revision| ReflogWalkTarget::new(Some(revision)))
            .collect::<Result<Vec<_>>>()?
    };
    let mut skipped = 0usize;
    let mut emitted = 0usize;
    for target in references {
        let mut entries = store.read_reflog(&target.reference)?;
        entries.reverse();
        if opts.reverse {
            entries.reverse();
        }
        for (index, entry) in entries.iter().enumerate() {
            if skipped < opts.skip {
                skipped += 1;
                continue;
            }
            if opts.max_count.is_some_and(|max_count| emitted >= max_count) {
                stdout.flush()?;
                return Ok(());
            }
            let emitted_entry = match opts.output {
                LogOutput::Compiled {
                    compiled,
                    final_newline,
                    ..
                } => {
                    let mut line = Vec::with_capacity(compiled.estimated_line_capacity());
                    let mut ctx = ReflogWalkFormatContext {
                        compiled,
                        db: &mut db,
                        format,
                        display_reference: &target.display_reference,
                        full_reference: &target.reference,
                        date_mode: opts.date_mode,
                        decorations: &decorations,
                        mailmap: &mailmap,
                    };
                    if !emit_compiled_reflog_walk_format(&mut ctx, entry, index, &mut line)? {
                        false
                    } else {
                        stdout.write_all(&line)?;
                        if *final_newline && !line.ends_with(b"\n") {
                            stdout.write_all(b"\n")?;
                        }
                        true
                    }
                }
                LogOutput::Default(_) => {
                    let display_selector = target.display_selector(entry, index, opts.date_mode);
                    emit_default_reflog_walk_format(
                        &mut db,
                        format,
                        entry,
                        &target.display_reference,
                        &display_selector,
                        opts.date_mode,
                        &mailmap,
                        &mut stdout,
                    )?
                }
            };
            if emitted_entry {
                emitted += 1;
            }
        }
    }
    stdout.flush()?;
    Ok(())
}

struct ReflogWalkTarget {
    reference: String,
    display_reference: String,
    date_selector: bool,
}

impl ReflogWalkTarget {
    fn new(revision: Option<&String>) -> Result<Self> {
        let original = revision.map(String::as_str);
        let reference = reflog_reference_name(original)?;
        Ok(Self {
            display_reference: reflog_walk_display_reference(&reference),
            reference,
            date_selector: original.is_some_and(reflog_revision_uses_date_selector),
        })
    }

    fn display_selector(&self, entry: &ReflogEntry, index: usize, date_mode: &DateMode) -> String {
        if self.date_selector {
            commit_identity_date(&entry.committer, date_mode)
        } else {
            index.to_string()
        }
    }
}

fn reflog_revision_uses_date_selector(revision: &str) -> bool {
    let Some(open) = revision.rfind("@{") else {
        return false;
    };
    let Some(inner) = revision.strip_suffix('}') else {
        return false;
    };
    let inner = &inner[open + 2..];
    !(inner.bytes().all(|byte| byte.is_ascii_digit())
        || inner.eq_ignore_ascii_case("u")
        || inner.eq_ignore_ascii_case("upstream")
        || inner.eq_ignore_ascii_case("push")
        || inner.starts_with('-'))
}

fn reflog_walk_display_reference(reference: &str) -> String {
    reference
        .strip_prefix("refs/heads/")
        .unwrap_or(reference)
        .to_string()
}

fn emit_default_reflog_walk_format(
    db: &mut FileObjectDatabase,
    format: ObjectFormat,
    entry: &ReflogEntry,
    display_reference: &str,
    display_selector: &str,
    date_mode: &DateMode,
    mailmap: &commands::utility::Mailmap,
    out: &mut impl Write,
) -> Result<bool> {
    let Some(record) = reflog_walk_commit_record(db, format, entry)? else {
        return Ok(false);
    };
    writeln!(out, "commit {}", record.oid).map_err(io::Error::from)?;
    let (reflog_name, reflog_email) = commit_identity_name_email(&entry.committer);
    writeln!(
        out,
        "Reflog: {}@{{{}}} ({} <{}>)",
        display_reference, display_selector, reflog_name, reflog_email
    )
    .map_err(io::Error::from)?;
    out.write_all(b"Reflog message: ")?;
    out.write_all(&reflog_walk_subject(db, format, entry)?)?;
    out.write_all(b"\n")?;
    writeln!(
        out,
        "Author: {}",
        commit_identity_mailmapped(&record.commit.author, Some(mailmap))
    )
    .map_err(io::Error::from)?;
    writeln!(
        out,
        "Date:   {}",
        commit_identity_date(&record.commit.author, date_mode)
    )
    .map_err(io::Error::from)?;
    out.write_all(b"\n")?;
    let display_message = commit_message_for_commit_encoding(&record.commit, "UTF-8");
    for line in commit_message_lines(&display_message) {
        if line.is_empty() {
            out.write_all(b"\n")?;
        } else {
            out.write_all(b"    ")?;
            out.write_all(line)?;
            out.write_all(b"\n")?;
        }
    }
    Ok(true)
}

pub(super) fn compile_log_filter_matcher(
    patterns: &[String],
    kind: sley_grep::PatternKind,
    ignore_case: bool,
    error_context: &str,
) -> Result<Option<sley_grep::GrepMatcher>> {
    if patterns.is_empty() {
        return Ok(None);
    }
    sley_grep::GrepMatcher::compile_with_error_context(
        sley_grep::GrepCompileConfig {
            patterns,
            kind,
            ignore_case,
            word: false,
            line_regexp: false,
            diagnostic_verbosity: sley_grep::RegexDiagnosticVerbosity::Default,
        },
        error_context,
    )
    .map(Some)
}

pub(super) struct LogGrepColors {
    pub(super) enabled: bool,
    pub(super) selected: String,
    pub(super) matched: String,
    pub(super) reset: String,
}

impl LogGrepColors {
    pub(super) fn from_config(config: &GitConfig, enabled: bool) -> Self {
        let selected = config
            .get("color", Some("grep"), "selected")
            .map(|spec| git_color_spec_to_ansi(spec, enabled))
            .unwrap_or_default();
        let matched_spec = config
            .get("color", Some("grep"), "matchSelected")
            .or_else(|| config.get("color", Some("grep"), "match"))
            .unwrap_or("bold red");
        Self {
            enabled,
            selected,
            matched: git_color_spec_to_ansi(matched_spec, enabled),
            reset: git_color_spec_to_ansi("reset", enabled),
        }
    }
}

pub(super) fn log_highlight_matches(
    text: &[u8],
    matcher: Option<&sley_grep::GrepMatcher>,
    colors: &LogGrepColors,
) -> Vec<u8> {
    let Some(matcher) = matcher.filter(|_| colors.enabled && !colors.matched.is_empty()) else {
        return text.to_vec();
    };
    let spans = matcher.match_spans_expr(None, text);
    if spans.is_empty() {
        return text.to_vec();
    }
    let mut out = Vec::with_capacity(text.len() + spans.len() * 16);
    let mut pos = 0usize;
    if !colors.selected.is_empty() {
        out.extend_from_slice(colors.selected.as_bytes());
    }
    for (start, end) in spans {
        if start > pos {
            out.extend_from_slice(&text[pos..start]);
        }
        if !colors.selected.is_empty() {
            out.extend_from_slice(colors.reset.as_bytes());
        }
        out.extend_from_slice(colors.matched.as_bytes());
        out.extend_from_slice(&text[start..end]);
        out.extend_from_slice(colors.reset.as_bytes());
        if !colors.selected.is_empty() {
            out.extend_from_slice(colors.selected.as_bytes());
        }
        pos = end;
    }
    if pos < text.len() {
        out.extend_from_slice(&text[pos..]);
    }
    if !colors.selected.is_empty() {
        out.extend_from_slice(colors.reset.as_bytes());
    }
    out
}

pub(super) fn log_author_matcher_matches(
    record: &sley_rev::CommitRecord,
    filter: Option<&sley_grep::GrepMatcher>,
    mailmap: Option<&commands::utility::Mailmap>,
) -> bool {
    filter.is_none_or(|filter| {
        filter.matches_any(&log_mailmapped_identity_header(
            &record.commit.author,
            mailmap,
        ))
    })
}

pub(super) fn log_committer_matcher_matches(
    record: &sley_rev::CommitRecord,
    filter: Option<&sley_grep::GrepMatcher>,
    mailmap: Option<&commands::utility::Mailmap>,
) -> bool {
    filter.is_none_or(|filter| {
        filter.matches_any(&log_mailmapped_identity_header(
            &record.commit.committer,
            mailmap,
        ))
    })
}

/// git's `apply_mailmap_to_header`: when `--use-mailmap`/`log.mailmap` is active
/// the `--author`/`--committer` grep runs against the *mailmapped* identity
/// header. Rewrites `Name <email> <ts> <tz>` → `MappedName <mapped@email> ...`.
/// With no mailmap (or an empty one) the original header bytes are returned.
fn log_mailmapped_identity_header(
    raw: &[u8],
    mailmap: Option<&commands::utility::Mailmap>,
) -> Vec<u8> {
    let Some(mailmap) = mailmap.filter(|m| !m.is_empty()) else {
        return raw.to_vec();
    };
    let (name, email) = mailmap.rewrite_identity(raw);
    // Preserve the trailing ` <ts> <tz>` (everything after the closing `>`).
    let tail = raw
        .iter()
        .position(|&b| b == b'>')
        .map(|idx| &raw[idx + 1..])
        .unwrap_or(b"");
    let mut out = Vec::with_capacity(name.len() + email.len() + tail.len() + 4);
    out.extend_from_slice(&name);
    out.extend_from_slice(b" <");
    out.extend_from_slice(&email);
    out.push(b'>');
    out.extend_from_slice(tail);
    out
}

pub(super) fn log_grep_matcher_matches(
    record: &sley_rev::CommitRecord,
    filter: Option<&sley_grep::GrepMatcher>,
    all_match: bool,
    invert: bool,
    output_encoding: &str,
) -> bool {
    let Some(filter) = filter else {
        return true;
    };
    let message = commit_message_for_commit_encoding(&record.commit, output_encoding);
    let search_message = if encoding_is_utf8(output_encoding) {
        Cow::Borrowed(message.as_ref())
    } else {
        Cow::Owned(argv_string_from_bytes(message.as_ref()).into_bytes())
    };
    let matched = if all_match {
        filter.matches_all(&search_message)
    } else {
        filter.matches_any(&search_message)
    };
    matched != invert
}

struct ReflogWalkFormatContext<'a> {
    compiled: &'a CompiledLogFormat,
    db: &'a mut FileObjectDatabase,
    format: ObjectFormat,
    display_reference: &'a str,
    full_reference: &'a str,
    date_mode: &'a DateMode,
    decorations: &'a HashMap<ObjectId, Vec<String>>,
    mailmap: &'a commands::utility::Mailmap,
}

fn emit_compiled_reflog_walk_format(
    ctx: &mut ReflogWalkFormatContext<'_>,
    entry: &ReflogEntry,
    index: usize,
    out: &mut Vec<u8>,
) -> Result<bool> {
    let (reflog_name, reflog_email) = commit_identity_name_email(&entry.committer);
    let Some(commit_record) = reflog_walk_commit_record(ctx.db, ctx.format, entry)? else {
        return Ok(false);
    };
    let (author_name, author_email) = commit_identity_name_email(&commit_record.commit.author);
    let (committer_name, committer_email) =
        commit_identity_name_email(&commit_record.commit.committer);
    let commit_identity = (
        author_name,
        author_email,
        committer_name,
        committer_email,
        commit_identity_timestamp(&commit_record.commit.author),
        commit_identity_timestamp(&commit_record.commit.committer),
    );
    let log_context = LogFormatContext {
        abbrev_len: Some(7),
        decorations: ctx.decorations,
        marker: '>',
        dialect: LogFormatDialect::Log,
        source: None,
        date_mode: ctx.date_mode,
        source_oid: None,
        describe: None,
        signature: None,
        color: false,
        output_encoding: "UTF-8",
        mailmap: &CliMailmapAdapter(ctx.mailmap),
        use_mailmap: true,
    };
    for token in &ctx.compiled.tokens {
        match token {
            FormatToken::Literal(text) => out.extend_from_slice(text.as_bytes()),
            FormatToken::Percent => out.push(b'%'),
            FormatToken::ReflogGs => {
                out.extend_from_slice(&reflog_walk_subject(ctx.db, ctx.format, entry)?);
            }
            FormatToken::ReflogGd => {
                write!(out, "{}@{{{index}}}", ctx.display_reference).map_err(io::Error::from)?;
            }
            FormatToken::ReflogGD => {
                write!(out, "{}@{{{index}}}", ctx.full_reference).map_err(io::Error::from)?;
            }
            FormatToken::ReflogGn => out.extend_from_slice(reflog_name.as_bytes()),
            FormatToken::ReflogGe => out.extend_from_slice(reflog_email.as_bytes()),
            FormatToken::Newline => out.push(b'\n'),
            FormatToken::HexByte(byte) => out.push(*byte),
            _ => {
                emit_log_one_token(
                    token,
                    &commit_record,
                    &log_context,
                    out,
                    &commit_identity.0,
                    &commit_identity.1,
                    &commit_identity.2,
                    &commit_identity.3,
                    &commit_identity.4,
                    &commit_identity.5,
                )?;
            }
        }
    }
    Ok(true)
}

fn reflog_walk_commit_record(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    entry: &ReflogEntry,
) -> Result<Option<sley_rev::CommitRecord>> {
    let object = match db.read_object(&entry.new_oid) {
        Ok(object) if object.object_type == ObjectType::Commit => object,
        Ok(_) => return Ok(None),
        Err(_) => return Ok(None),
    };
    let commit = Commit::parse(format, &object.body)?;
    Ok(Some(sley_rev::CommitRecord {
        oid: entry.new_oid,
        parents: commit.parents.clone(),
        commit,
    }))
}

fn reflog_walk_subject(
    db: &mut FileObjectDatabase,
    format: ObjectFormat,
    entry: &ReflogEntry,
) -> Result<Vec<u8>> {
    let Some(rest) = entry.message.strip_prefix(b"commit: ") else {
        return Ok(entry.message.clone());
    };
    let object = match db.read_object(&entry.new_oid) {
        Ok(object) if object.object_type == ObjectType::Commit => object,
        _ => return Ok(entry.message.clone()),
    };
    let Ok(commit) = Commit::parse_ref(format, &object.body) else {
        return Ok(entry.message.clone());
    };
    if !commit.parents.is_empty() {
        return Ok(entry.message.clone());
    }
    let mut subject = b"commit (initial): ".to_vec();
    subject.extend_from_slice(rest);
    Ok(subject)
}
