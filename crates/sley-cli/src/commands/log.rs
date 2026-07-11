//! Extracted from the crate root (sley#8 phase 1) — code motion only.
#![allow(clippy::expect_used)]

use sley::plumbing::{sley_diff_merge, sley_rev, sley_worktree};
// A glob of the crate root brings every shared helper/type into scope via
// descendant-privacy; see commands::stash for the rationale.
use crate::*;
use sley_notes::{NotesRef, read_note_bytes};
use sley_pathspec::normalized_revwalk_pathspec;

mod diff;
mod graph;
mod line_log;
mod pickaxe;
mod reflog;

use diff::{
    LogDiffContext, LogDiffMerges, LogDiffOptions, log_parse_diff_merges,
    log_parse_diff_merges_config, log_prefix_display_width,
};
use graph::{graph_show_commit, graph_show_commit_msg, graph_show_oneline, graph_show_padding};
use line_log::{LineLogOutputCtx, run_line_log_output};
use pickaxe::{
    CompiledPickaxe, DiffFilterMatchOptions, PickaxeSpec, compile_pickaxe_regex,
    diff_filter_commit_matches, diff_filter_entry_matches, log_follow_single_path,
    parse_diff_filter_arg, pickaxe_commit_matches, pickaxe_filter_entries,
    resolve_diff_filter_mask,
};
use reflog::{
    LogGrepColors, ReflogWalkOptions, compile_log_filter_matcher, log_author_matcher_matches,
    log_committer_matcher_matches, log_grep_matcher_matches, log_highlight_matches,
    log_walk_reflogs,
};

/// Resolve a file-backed config value across Git's system, global, and local
/// layers. `RepositoryContext` intentionally carries the repository config and
/// command-scoped injections; callers use this only when those higher
/// precedence layers did not define the key.
fn log_effective_file_config_value(
    git_dir: &Path,
    cwd: &Path,
    section: &str,
    variable: &str,
) -> Result<Option<String>> {
    let config = commands::remote::read_effective_repo_config(git_dir, cwd)
        .map_err(report_config_setup_error)?;
    Ok(config
        .get(section, None, variable)
        .map(str::to_string)
        .or_else(|| {
            matches!(config.get_all(section, None, variable).last(), Some(None))
                .then(|| "true".to_string())
        }))
}

/// Tracks `git log`'s notes-display state (`--notes`, `--show-notes[=ref]`,
/// `--no-notes`, `--standard-notes`, `--no-standard-notes`), mirroring git's
/// `display_notes_opt` / `show_notes` resolution.
#[derive(Default, Clone)]
struct NotesDisplay {
    /// Whether any notes flag was given (git's `show_notes_given`).
    given: bool,
    /// Whether notes display is currently enabled (git's `show_notes`).
    enabled: bool,
    /// Tri-state `use_default_notes`: None = unset (-1), Some(true) = forced on,
    /// Some(false) = standard refs suppressed.
    use_default: Option<bool>,
    /// Extra refs from `--notes=<ref>` / `--show-notes=<ref>`, expanded.
    extra_refs: Vec<String>,
}

impl NotesDisplay {
    /// `--notes` / `--show-notes`: enable display using the standard refs.
    fn add_default(&mut self) {
        self.use_default = Some(true);
        self.enabled = true;
        self.given = true;
    }
    /// `--notes=<ref>`: add a specific ref without forcing the standard refs on
    /// (only `--show-notes=<ref>` re-enables the defaults).
    fn add_ref(&mut self, reff: &str) {
        self.extra_refs
            .push(NotesRef::expand(reff).as_str().to_string());
        self.enabled = true;
        self.given = true;
    }
    /// `--show-notes=<ref>`: like `add_ref`, but additionally turns the standard
    /// refs back on when they were unset (matches git's `--show-notes=` path).
    fn add_show_ref(&mut self, reff: &str) {
        if self.use_default.is_none() {
            self.use_default = Some(true);
        }
        self.add_ref(reff);
    }
    /// `--no-notes`: clear all display state and turn notes off.
    fn disable(&mut self) {
        self.use_default = Some(false);
        self.extra_refs.clear();
        self.enabled = false;
        self.given = true;
    }
    /// `--no-standard-notes`: suppress the standard refs but keep any extra refs
    /// (does not by itself disable display).
    fn no_standard(&mut self) {
        self.use_default = Some(false);
        self.given = true;
    }
    /// `--standard-notes`: re-enable the standard refs (keeps extra refs).
    fn add_standard(&mut self) {
        self.use_default = Some(true);
        self.given = true;
    }

    /// Resolve whether notes display is active. When no flag was given, notes
    /// show only for the default (no-`--pretty`) format. When a flag was given,
    /// the explicit `enabled` state wins.
    fn is_active(&self, default_format: bool) -> bool {
        if self.given {
            self.enabled
        } else {
            default_format
        }
    }

    /// Compute the ordered, de-duplicated list of notes refs to display,
    /// mirroring git's `load_display_notes`: the standard refs (default notes
    /// ref + `GIT_NOTES_DISPLAY_REF` env or `notes.displayRef` config, glob
    /// expanded) come first when `use_default` is set or unset-with-no-extras,
    /// then the `--notes=<ref>` extras (glob expanded). A `notes.displayRef`
    /// with no value is a fatal error.
    fn resolve_refs(&self, git_dir: &Path, store: &FileRefStore) -> Result<Vec<String>> {
        let mut refs: Vec<String> = Vec::new();
        let load_standard = matches!(self.use_default, Some(true))
            || (self.use_default.is_none() && self.extra_refs.is_empty());
        if load_standard {
            // git's default_notes_ref takes GIT_NOTES_REF verbatim when set —
            // even when empty, which yields a no-op (no default note shown).
            let default_ref = match env::var("GIT_NOTES_REF") {
                Ok(value) => value,
                Err(_) => crate::commands::notes::raw_notes_ref(git_dir, None),
            };
            if !default_ref.is_empty() {
                push_unique(&mut refs, default_ref);
            }
            // A command-line `-c notes.displayRef` with no value is a parse
            // error (the key is a string, not a bool). Detect the bool-true
            // marker the `-c key` form injects and reject it, as git does.
            if matches!(global_config_value("notes.displayRef"), Ok(Some(v)) if v == "true") {
                eprintln!("error: missing value for 'notes.displayref'");
                eprintln!("fatal: unable to parse 'notes.displayref' from command-line config");
                return Err(GitError::Exit(128));
            }
            if let Ok(env_value) = env::var("GIT_NOTES_DISPLAY_REF") {
                for part in env_value.split(':').filter(|s| !s.is_empty()) {
                    for expanded in expand_notes_glob(store, part)? {
                        push_unique(&mut refs, expanded);
                    }
                }
            } else if let Ok(config) = read_repo_config(git_dir) {
                for value in config
                    .get_all("notes", None, "displayRef")
                    .into_iter()
                    .flatten()
                {
                    if value.is_empty() {
                        eprintln!(
                            "fatal: unable to parse 'notes.displayref' from command-line config"
                        );
                        return Err(GitError::Exit(128));
                    }
                    for expanded in expand_notes_glob(store, value)? {
                        push_unique(&mut refs, expanded);
                    }
                }
            }
        }
        for extra in &self.extra_refs {
            for expanded in expand_notes_glob(store, extra)? {
                push_unique(&mut refs, expanded);
            }
        }
        Ok(refs)
    }
}

/// Push `value` to `refs` only if it is not already present (preserve order).
fn push_unique(refs: &mut Vec<String>, value: String) {
    if !refs.contains(&value) {
        refs.push(value);
    }
}

fn log_output_indicator_byte(value: &str, fallback: u8) -> u8 {
    value.as_bytes().first().copied().unwrap_or(fallback)
}

/// Expand a single notes-ref spec: a `*`-containing glob matches existing refs
/// by prefix (ref-name sorted); an exact ref is returned as-is.
pub(crate) fn expand_notes_glob(store: &FileRefStore, glob: &str) -> Result<Vec<String>> {
    if !glob.contains('*') {
        return Ok(vec![glob.to_string()]);
    }
    let prefix = glob.trim_end_matches('*');
    let mut matched: Vec<String> = store
        .list_refs()?
        .into_iter()
        .map(|entry| entry.name)
        .filter(|name| name.starts_with(prefix))
        .collect();
    matched.sort();
    Ok(matched)
}

/// Resolve the standard notes display refs and render the notes block for
/// `oid`, for callers (e.g. `git show`) that always use the default display set.
/// Returns the bytes to append after the commit message (empty when none).
pub(crate) fn render_standard_notes(
    git_dir: &Path,
    format: ObjectFormat,
    oid: &ObjectId,
) -> Result<Vec<u8>> {
    let store = FileRefStore::new(git_dir, format);
    let display = NotesDisplay {
        use_default: Some(true),
        ..NotesDisplay::default()
    };
    let refs = display.resolve_refs(git_dir, &store)?;
    render_notes_block(git_dir, format, &store, &refs, oid)
}

/// Resolve the standard notes display refs without rendering. git loads the
/// display notes trees at revision setup; a valueless `-c notes.displayRef` is a
/// fatal parse error surfaced there. Callers (e.g. `diff-tree --notes`) invoke
/// this early so that error fires before any output.
pub(crate) fn resolve_standard_notes_refs(
    git_dir: &Path,
    format: ObjectFormat,
) -> Result<Vec<String>> {
    let store = FileRefStore::new(git_dir, format);
    let display = NotesDisplay {
        use_default: Some(true),
        ..NotesDisplay::default()
    };
    display.resolve_refs(git_dir, &store)
}

/// Render the `Notes:` / `Notes (<name>):` block(s) for `oid` across the
/// resolved display refs, matching git's `format_note`: a leading blank line,
/// the label, then each note line indented by four spaces. Returns the bytes to
/// append after the commit message (empty when no notes exist).
fn render_notes_block(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    display_refs: &[String],
    oid: &ObjectId,
) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for reff in display_refs {
        let handle = NotesRef::expand(reff);
        let Some(mut body) = read_note_bytes(git_dir, format, store, &handle, oid)? else {
            continue;
        };
        // git drops a single trailing newline before indenting.
        if body.last() == Some(&b'\n') {
            body.pop();
        }
        // Label: bare `Notes:` only for the literal default ref.
        if handle.as_str() == sley_notes::DEFAULT_NOTES_REF {
            out.extend_from_slice(b"\nNotes:\n");
        } else {
            let name = handle
                .as_str()
                .strip_prefix("refs/")
                .and_then(|s| s.strip_prefix("notes/"))
                .unwrap_or(handle.as_str());
            out.extend_from_slice(format!("\nNotes ({name}):\n").as_bytes());
        }
        // An empty note prints just the label (git's loop runs over zero bytes).
        if !body.is_empty() {
            for line in body.split(|b| *b == b'\n') {
                out.extend_from_slice(b"    ");
                out.extend_from_slice(line);
                out.push(b'\n');
            }
        }
    }
    Ok(out)
}

fn render_pretty_notes(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    display_refs: &[String],
    oid: &ObjectId,
) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for reff in display_refs {
        let handle = NotesRef::expand(reff);
        if let Some(body) = read_note_bytes(git_dir, format, store, &handle, oid)? {
            out.extend_from_slice(&body);
        }
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn emit_compiled_log_format_with_notes(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    display_refs: &[String],
    record: &sley_rev::CommitRecord,
    compiled: &CompiledLogFormat,
    context: &LogFormatContext<'_>,
    out: &mut Vec<u8>,
) -> Result<()> {
    let (author_name, author_email) = commit_identity_name_email(&record.commit.author);
    let (committer_name, committer_email) = commit_identity_name_email(&record.commit.committer);
    let author_timestamp = commit_identity_timestamp(&record.commit.author);
    let committer_timestamp = commit_identity_timestamp(&record.commit.committer);

    let mut wrap_width = 0i32;
    let mut wrap_indent1 = 0i32;
    let mut wrap_indent2 = 0i32;
    let mut wrap_start = out.len();
    let mut resolver = LogFormatNoteResolver {
        git_dir,
        format,
        store,
        display_refs,
        record,
        context,
        author_name: &author_name,
        author_email: &author_email,
        committer_name: &committer_name,
        committer_email: &committer_email,
        author_timestamp: &author_timestamp,
        committer_timestamp: &committer_timestamp,
        auto_color: false,
    };
    let segment_range = compiled.segment_range_for_tokens(0..compiled.tokens.len());
    compiled.expand.append_range_to_with_atom(
        out,
        segment_range,
        &mut resolver,
        |out, token, value| {
            if let FormatToken::Wrap(spec) = token {
                let new_w = spec.width as i32;
                let new_i1 = spec.indent1 as i32;
                let new_i2 = spec.indent2 as i32;
                if (new_w, new_i1, new_i2) != (wrap_width, wrap_indent1, wrap_indent2) {
                    if wrap_start < out.len() {
                        log_rewrap(out, wrap_start, wrap_width, wrap_indent1, wrap_indent2);
                    }
                    wrap_start = out.len();
                    wrap_width = new_w;
                    wrap_indent1 = new_i1;
                    wrap_indent2 = new_i2;
                }
            } else {
                out.extend_from_slice(value);
            }
            Ok(())
        },
    )?;
    if (wrap_width, wrap_indent1, wrap_indent2) != (0, 0, 0) && wrap_start < out.len() {
        log_rewrap(out, wrap_start, wrap_width, wrap_indent1, wrap_indent2);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn emit_encoded_compiled_log_format_with_notes(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    display_refs: &[String],
    record: &sley_rev::CommitRecord,
    compiled: &CompiledLogFormat,
    context: &LogFormatContext<'_>,
    out: &mut Vec<u8>,
) -> Result<()> {
    let mut line = Vec::with_capacity(compiled.estimated_line_capacity());
    emit_compiled_log_format_with_notes(
        git_dir,
        format,
        store,
        display_refs,
        record,
        compiled,
        context,
        &mut line,
    )?;
    let encoded = log_reencode_message(&line, "UTF-8", context.output_encoding);
    out.extend_from_slice(&encoded);
    Ok(())
}

fn emit_encoded_compiled_log_format_no_notes(
    record: &sley_rev::CommitRecord,
    compiled: &CompiledLogFormat,
    context: &LogFormatContext<'_>,
    out: &mut Vec<u8>,
) -> Result<()> {
    let mut line = Vec::with_capacity(compiled.estimated_line_capacity());
    emit_compiled_log_format(
        record,
        compiled,
        context,
        &mut line,
        0..compiled.tokens.len(),
    )?;
    let encoded = log_reencode_message(&line, "UTF-8", context.output_encoding);
    out.extend_from_slice(&encoded);
    Ok(())
}

/// Render a user (`--format=`) spec, expanding `%N` from the given or standard
/// notes display refs when `show_notes` is set.
pub(crate) fn format_commit_pretty_with_notes(
    git_dir: &Path,
    format: ObjectFormat,
    record: &sley_rev::CommitRecord,
    compiled: &CompiledLogFormat,
    context: &LogFormatContext<'_>,
    show_notes: bool,
    notes_refs: &[String],
) -> Result<Vec<u8>> {
    let mut line = Vec::with_capacity(compiled.estimated_line_capacity());
    if show_notes && compiled_format_uses_notes(compiled) {
        let store = FileRefStore::new(git_dir, format);
        let display_refs = if notes_refs.is_empty() {
            resolve_standard_notes_refs(git_dir, format)?
        } else {
            notes_refs.to_vec()
        };
        emit_compiled_log_format_with_notes(
            git_dir,
            format,
            &store,
            &display_refs,
            record,
            compiled,
            context,
            &mut line,
        )?;
    } else {
        emit_compiled_log_format(
            record,
            compiled,
            context,
            &mut line,
            0..compiled.tokens.len(),
        )?;
    }
    Ok(log_reencode_message(&line, "UTF-8", context.output_encoding).into_owned())
}

/// Render a user (`--format=`) spec to stdout for `git show`, expanding `%N`
/// from the standard notes display refs when `show_notes` is set. git computes
/// `ctx.notes_message` (raw) for userformats and `%N` injects it; the plain
/// atom resolver no-ops `%N`, so route through the notes-aware emitter when the
/// format actually references notes. Returns the number of bytes written.
pub(crate) fn print_log_custom_format_with_notes(
    git_dir: &Path,
    format: ObjectFormat,
    record: &sley_rev::CommitRecord,
    compiled: &CompiledLogFormat,
    context: &LogFormatContext<'_>,
    show_notes: bool,
) -> Result<usize> {
    let mut line = Vec::with_capacity(compiled.estimated_line_capacity());
    if show_notes && compiled_format_uses_notes(compiled) {
        let store = FileRefStore::new(git_dir, format);
        let display = NotesDisplay {
            use_default: Some(true),
            ..NotesDisplay::default()
        };
        let refs = display.resolve_refs(git_dir, &store)?;
        emit_compiled_log_format_with_notes(
            git_dir, format, &store, &refs, record, compiled, context, &mut line,
        )?;
    } else {
        emit_compiled_log_format(
            record,
            compiled,
            context,
            &mut line,
            0..compiled.tokens.len(),
        )?;
    }
    let out = log_reencode_message(&line, "UTF-8", context.output_encoding);
    let emitted = out.len();
    io::stdout().write_all(&out)?;
    io::stdout().flush()?;
    Ok(emitted)
}

struct LogFormatNoteResolver<'a, 'b> {
    git_dir: &'a Path,
    format: ObjectFormat,
    store: &'a FileRefStore,
    display_refs: &'a [String],
    record: &'a sley_rev::CommitRecord,
    context: &'a LogFormatContext<'b>,
    author_name: &'a str,
    author_email: &'a str,
    committer_name: &'a str,
    committer_email: &'a str,
    author_timestamp: &'a str,
    committer_timestamp: &'a str,
    auto_color: bool,
}

impl sley_strbuf_expand::AtomResolver<FormatToken> for LogFormatNoteResolver<'_, '_> {
    fn resolve_atom(&mut self, out: &mut Vec<u8>, atom: &FormatToken) -> Result<()> {
        if matches!(atom, FormatToken::NoteName)
            && matches!(self.context.dialect, LogFormatDialect::Log)
        {
            let notes = render_pretty_notes(
                self.git_dir,
                self.format,
                self.store,
                self.display_refs,
                &self.record.oid,
            )?;
            out.extend_from_slice(&notes);
            return Ok(());
        }
        if self.auto_color && matches!(atom, FormatToken::OidFull | FormatToken::OidAbbrev) {
            self.auto_color = false;
            if self.context.color {
                out.extend_from_slice(b"\x1b[33m");
                emit_log_one_token(
                    atom,
                    self.record,
                    self.context,
                    out,
                    self.author_name,
                    self.author_email,
                    self.committer_name,
                    self.committer_email,
                    self.author_timestamp,
                    self.committer_timestamp,
                )?;
                out.extend_from_slice(b"\x1b[m");
                return Ok(());
            }
        }
        if matches!(atom, FormatToken::ColorAuto) {
            self.auto_color = self.context.color;
            return Ok(());
        }
        emit_log_one_token(
            atom,
            self.record,
            self.context,
            out,
            self.author_name,
            self.author_email,
            self.committer_name,
            self.committer_email,
            self.author_timestamp,
            self.committer_timestamp,
        )
    }

    fn is_modifier_atom(&self, atom: &FormatToken) -> bool {
        matches!(
            atom,
            FormatToken::ColorParen(_) | FormatToken::ColorName(_) | FormatToken::ColorAuto
        )
    }
}

pub(crate) fn cmd_log(cli_session: &session::CliSession, args: &[String]) -> Result<()> {
    cmd_log_impl(cli_session, args, false)
}

/// `git whatchanged --i-still-use-this`: log with raw diff output by default
/// and `always_show_header = 0` semantics (commits whose diff comes out empty
/// — e.g. merges — are omitted entirely).
pub(crate) fn cmd_whatchanged(cli_session: &session::CliSession, args: &[String]) -> Result<()> {
    let mut acknowledged = false;
    let mut filtered = Vec::with_capacity(args.len());
    for arg in args {
        if arg == "--i-still-use-this" {
            acknowledged = true;
        } else {
            filtered.push(arg.clone());
        }
    }
    if !acknowledged {
        eprintln!(
            "fatal: git whatchanged is nominated for removal.\nIf you still use this command, add an extra option, '--i-still-use-this',\non the command line and let us know you still use it by sending an e-mail\nto <git@vger.kernel.org>.  Thanks."
        );
        return Err(GitError::Exit(128));
    }
    cmd_log_impl(cli_session, &filtered, true)
}

fn log_limited_commit_format_supported(compiled: &CompiledLogFormat) -> bool {
    !compiled.tokens.is_empty()
        && !compiled.uses_decorations()
        && !compiled.uses_source()
        && compiled.tokens.iter().all(|token| {
            matches!(
                token,
                FormatToken::Literal(_)
                    | FormatToken::Percent
                    | FormatToken::OidFull
                    | FormatToken::OidAbbrev
                    | FormatToken::ParentsFull
                    | FormatToken::ParentsAbbrev
                    | FormatToken::Marker
                    | FormatToken::Subject
                    | FormatToken::SanitizedSubject
                    | FormatToken::NoteName
                    | FormatToken::ColorParen(_)
                    | FormatToken::ColorName(_)
                    | FormatToken::Newline
                    | FormatToken::HexByte(_)
            )
        })
}

fn log_plain_oneline_format(compiled: &CompiledLogFormat) -> bool {
    matches!(
        compiled.tokens.as_slice(),
        [
            FormatToken::OidAbbrev,
            FormatToken::Literal(space),
            FormatToken::Subject
        ] if space == " "
    )
}

fn log_config_color_is_always(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "always" | "true" | "yes" | "on" | "1"
    )
}

pub(crate) fn compiled_format_uses_notes(compiled: &CompiledLogFormat) -> bool {
    compiled
        .tokens
        .iter()
        .any(|token| matches!(token, FormatToken::NoteName))
}

fn compiled_format_uses_mailmap(compiled: &CompiledLogFormat) -> bool {
    compiled.tokens.iter().any(|token| {
        matches!(
            token,
            FormatToken::AuthorNameMapped
                | FormatToken::AuthorEmailMapped
                | FormatToken::AuthorEmailLocalMapped
                | FormatToken::CommitterNameMapped
                | FormatToken::CommitterEmailMapped
                | FormatToken::CommitterEmailLocalMapped
        )
    })
}

fn log_output_needs_mailmap(output: &LogOutput, use_mailmap: bool) -> bool {
    match output {
        LogOutput::Default(kind) => {
            use_mailmap
                && matches!(
                    kind,
                    LogDefaultKind::Medium
                        | LogDefaultKind::Short
                        | LogDefaultKind::Full
                        | LogDefaultKind::Fuller
                )
        }
        LogOutput::Compiled { compiled, .. } => compiled_format_uses_mailmap(compiled),
    }
}

fn log_cached_mailmap<'a>(
    cache: &'a mut Option<commands::utility::Mailmap>,
    git_dir: &Path,
    format: ObjectFormat,
    replace_objects: bool,
) -> Result<&'a commands::utility::Mailmap> {
    if cache.is_none() {
        *cache = Some(commands::utility::Mailmap::load_default(
            git_dir,
            format,
            replace_objects,
        )?);
    }
    Ok(cache.as_ref().expect("mailmap cache was just initialized"))
}

pub(crate) fn render_log_raw_pretty(record: &sley_rev::CommitRecord, expand_tabs: i32) -> Vec<u8> {
    let mut out = Vec::new();
    writeln!(out, "commit {}", record.oid).expect("write to Vec cannot fail");
    writeln!(out, "tree {}", record.commit.tree).expect("write to Vec cannot fail");
    for parent in &record.parents {
        writeln!(out, "parent {parent}").expect("write to Vec cannot fail");
    }
    out.extend_from_slice(b"author ");
    out.extend_from_slice(&record.commit.author);
    out.push(b'\n');
    out.extend_from_slice(b"committer ");
    out.extend_from_slice(&record.commit.committer);
    out.extend_from_slice(b"\n\n");
    render_log_raw_message(&record.commit.message, expand_tabs, &mut out);
    out
}

fn render_log_raw_message(message: &[u8], expand_tabs: i32, out: &mut Vec<u8>) {
    let mut lines = message.split(|byte| *byte == b'\n').peekable();
    while let Some(line) = lines.next() {
        // `str::lines`, used by the old implementation, omits the synthetic
        // empty field after a final newline. Preserve that line structure while
        // operating on bytes so `--format=raw` does not replace malformed
        // commit bytes with UTF-8 replacement characters.
        if line.is_empty() && lines.peek().is_none() && message.ends_with(b"\n") {
            break;
        }
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        out.extend_from_slice(b"    ");
        out.extend_from_slice(&log_expand_tabs(line, expand_tabs));
        out.push(b'\n');
    }
}

#[cfg(test)]
mod raw_message_tests {
    use super::render_log_raw_message;

    #[test]
    fn raw_message_preserves_malformed_utf8() {
        let message = b"Th\xf8\x9d\x84\x9es\n";
        let mut out = Vec::new();
        render_log_raw_message(message, 0, &mut out);
        assert_eq!(out, [b"    ".as_slice(), message].concat());
    }
}

/// Display width of a message segment for tab-stop computation, mirroring
/// upstream pretty.c's `pp_utf8_width`: returns `None` when the segment is not
/// well-formed UTF-8 or carries a control character with undefined width, in
/// which case the caller stops trying to align the rest of the line.
fn log_segment_width(bytes: &[u8]) -> Option<usize> {
    let text = std::str::from_utf8(bytes).ok()?;
    let mut width = 0usize;
    for ch in text.chars() {
        let cp = ch as u32;
        if cp < 0x20 || cp == 0x7f {
            return None;
        }
        width += 1;
    }
    Some(width)
}

/// Expand tabs in a single log-message line, mirroring upstream pretty.c's
/// `strbuf_add_tabexpand`. Each tab is replaced with enough spaces to reach the
/// next column that is a multiple of `tabwidth`, measured from the start of the
/// current segment (the de-tab column counter resets after every tab, and the
/// surrounding indent prefix is emitted separately so it is not counted here).
/// A non-positive `tabwidth`, or a line without tabs, is returned unchanged.
pub(crate) fn log_expand_tabs(line: &[u8], tabwidth: i32) -> Vec<u8> {
    if tabwidth <= 0 || !line.contains(&b'\t') {
        return line.to_vec();
    }
    let tabwidth = tabwidth as usize;
    let mut out = Vec::with_capacity(line.len() + tabwidth);
    let mut seg = line;
    while let Some(pos) = seg.iter().position(|&b| b == b'\t') {
        let before = &seg[..pos];
        let Some(width) = log_segment_width(before) else {
            // Badly formed UTF-8 / undefined-width char: give up on aligning
            // and emit the remainder verbatim, as upstream does.
            out.extend_from_slice(seg);
            return out;
        };
        out.extend_from_slice(before);
        let spaces = tabwidth - (width % tabwidth);
        out.resize(out.len() + spaces, b' ');
        seg = &seg[pos + 1..];
    }
    out.extend_from_slice(seg);
    out
}

/// The default tab-expansion width for a built-in output kind, matching
/// upstream pretty.c's `builtin_formats[]` table (`medium`/`full`/`fuller`
/// expand to 8; `short`/`raw` do not expand).
fn log_default_expand_tabs(kind: LogDefaultKind) -> i32 {
    match kind {
        LogDefaultKind::Medium | LogDefaultKind::Full | LogDefaultKind::Fuller => 8,
        LogDefaultKind::Short | LogDefaultKind::Raw => 0,
    }
}

fn log_source_label<'a>(
    oid: &ObjectId,
    source: Option<&'a str>,
    source_oid: Option<&'a HashMap<ObjectId, String>>,
) -> Option<&'a str> {
    source_oid
        .and_then(|map| map.get(oid).map(String::as_str))
        .or(source)
}

fn log_source_labels_for_selected(
    selected: &[&sley_rev::CommitRecord],
    source_starts: &[(ObjectId, String)],
    first_parent: bool,
) -> HashMap<ObjectId, String> {
    let shown = selected
        .iter()
        .map(|record| record.oid)
        .collect::<HashSet<_>>();
    let mut pending = HashMap::<ObjectId, String>::new();
    for (oid, label) in source_starts {
        pending.insert(*oid, label.clone());
    }
    let mut labels = HashMap::new();
    for record in selected {
        let Some(label) = pending.get(&record.oid).cloned() else {
            continue;
        };
        labels.insert(record.oid, label.clone());
        let parents = if first_parent {
            &record.parents[..record.parents.len().min(1)]
        } else {
            record.parents.as_slice()
        };
        for parent in parents {
            if shown.contains(parent) {
                pending.entry(*parent).or_insert_with(|| label.clone());
            }
        }
    }
    labels
}

fn log_unborn_head_branch(git_dir: &Path) -> Option<String> {
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let target = head.trim().strip_prefix("ref: ")?;
    target.strip_prefix("refs/heads/").map(str::to_string)
}

fn log_pathspec_magic(value: &str) -> Option<(&str, &str)> {
    let rest = value.strip_prefix(":(")?;
    let (magic, path) = rest.split_once(')')?;
    Some((magic, path))
}

fn log_follow_unsupported_pathspec_magic(value: &str) -> Option<String> {
    let (magic, _) = log_pathspec_magic(value)?;
    let unsupported = magic
        .split(',')
        .filter(|part| matches!(*part, "glob" | "icase"))
        .collect::<Vec<_>>();
    (!unsupported.is_empty()).then(|| {
        unsupported
            .into_iter()
            .map(|part| format!("'{part}'"))
            .collect::<Vec<_>>()
            .join(", ")
    })
}

fn emit_plain_oneline_limited_commit(
    db: &FileObjectDatabase,
    record: &sley_rev::CommitMetadata,
    abbrev_len: Option<usize>,
    output_encoding: &str,
    output_encoding_is_utf8: bool,
    out: &mut Vec<u8>,
) -> Result<()> {
    let object = db.read_object(&record.oid)?;
    if object.object_type != ObjectType::Commit {
        return Err(GitError::InvalidObject(format!(
            "expected commit {}, found {}",
            record.oid,
            object.object_type.as_str()
        )));
    }
    append_log_oid(out, &record.oid, abbrev_len);
    out.push(b' ');
    let (message, encoding) = commit_object_message_and_optional_encoding(&object.body);
    if encoding_is_none(output_encoding) {
        out.extend_from_slice(commit_subject_bytes(message));
        return Ok(());
    }
    if encoding.is_none() && output_encoding_is_utf8 {
        out.extend_from_slice(commit_subject_bytes(message));
        return Ok(());
    }
    let utf8_message = match encoding {
        Some(encoding) => log_reencode_message(message, encoding.as_ref(), "UTF-8"),
        None => std::borrow::Cow::Borrowed(message),
    };
    out.extend_from_slice(commit_subject_bytes(&utf8_message));
    if !output_encoding_is_utf8 {
        let reencoded = log_reencode_message(out, "UTF-8", output_encoding).into_owned();
        out.clear();
        out.extend_from_slice(&reencoded);
    }
    Ok(())
}

fn cmd_log_impl(
    cli_session: &session::CliSession,
    args: &[String],
    whatchanged: bool,
) -> Result<()> {
    let lazy_fetch = cli_session.lazy_fetch();
    let mut setup_args = Vec::new();
    let mut setup_not = false;
    let mut default_revision_given = false;
    let mut output = LogOutput::Default(LogDefaultKind::Medium);
    let mut notes_display = NotesDisplay::default();
    let mut preset_oneline: Option<bool> = None;
    let mut plain_oneline = false;
    // Raw `--pretty=`/`--format=` spec captured during arg parse and resolved
    // after config is loaded (aliases live in `pretty.<name>`). The bool is the
    // "format kind" flag: `--format=`/`tformat:` terminate each entry with a
    // newline; `--pretty=format:` separates entries instead.
    let mut pretty_spec: Option<(String, bool)> = None;
    let mut output_encoding_override: Option<String> = None;
    // `--expand-tabs[=<n>]` / `--no-expand-tabs`. `None` means the CLI didn't
    // decide, so the per-format default (`log_default_expand_tabs`) is used.
    let mut expand_tabs_explicit: Option<i32> = None;
    let mut walk_reflogs = false;
    let mut min_parents = None;
    let mut max_parents = None;
    let mut show_parents = false;
    let mut show_children = false;
    let mut abbrev_commit = false;
    let mut abbrev_commit_explicit = false;
    let mut abbrev_len = Some(7usize);
    let mut abbrev_len_explicit = false;
    // `--use-mailmap`/`--mailmap` (and their `--no-` forms). `None` means the CLI
    // didn't decide, so `log.mailmap` config (default true) is consulted.
    let mut use_mailmap_explicit: Option<bool> = None;
    let mut decoration = LogDecorationMode::Off;
    // Whether `--decorate`/`--no-decorate`/`--decorate=<mode>` was given on the
    // command line (a CLI flag overrides `log.decorate` config).
    let mut decoration_explicit = false;
    // `--decorate-refs=<glob>` (include-only) and
    // `--decorate-refs-exclude=<glob>` plus `--clear-decorations`.
    let mut decorate_refs_include: Vec<String> = Vec::new();
    let mut decorate_refs_exclude: Vec<String> = Vec::new();
    let mut clear_decorations = false;
    // `--simplify-by-decoration`: retain commits with decorations plus roots,
    // then apply the normal skip/count/reverse output limiting.
    let mut simplify_by_decoration = false;
    let mut read_stdin = false;
    let mut author_patterns = Vec::new();
    let mut committer_patterns = Vec::new();
    let mut grep_patterns = Vec::new();
    let mut grep_all_match = false;
    let mut invert_grep = false;
    let mut regexp_ignore_case = false;
    let mut pattern_kind = sley_grep::PatternKind::Basic;
    // Whether a CLI pattern-type flag (`-F`/`-E`/`-P`/`--basic-regexp`) was
    // given; if not, `grep.patternType` config supplies the default.
    let mut pattern_kind_explicit = false;
    let mut date_mode = DateMode::Default;
    let mut date_explicit = false;
    // `-z` / `--null`: separate/terminate compiled-format entries with NUL
    // instead of newline.
    let mut null_terminate = false;
    let mut graph = false;
    let mut boundary = false;
    let mut show_linear_break = false;
    let mut show_source = false;
    let mut ignored_missing_input = false;
    let mut revision_input_with_ignore_missing = false;
    let mut end_of_options_revs: Vec<String> = Vec::new();
    let mut inserted_default_head = false;
    // Diff-output options (`-p`, `--stat`, ...): rendered per commit against
    // its first parent, mirroring git's log diff machinery.
    let mut diff_opts = LogDiffOptions::default();
    // Tracks whether `--indent-heuristic` / `--no-indent-heuristic` was given on
    // the command line, so a CLI flag wins over `diff.indentHeuristic` config.
    let mut indent_heuristic_explicit = false;
    // `-L<start>,<end>:<file>` / `-L:<funcname>:<file>` line-log arguments (the
    // raw `<range>:<file>` strings). When non-empty, the log runs the line-log
    // engine instead of the ordinary walk.
    let mut line_log_args: Vec<crate::commands::line_log::LineLogArg> = Vec::new();
    // `--follow` (incompatible with `-L`).
    let mut saw_follow = false;
    let mut follow_explicit = false;
    let mut follow_config_allowed = true;
    // Whether a diff *output format* was explicitly requested (`-p`/`-s`/
    // `--stat`/`--raw`/...). Mirrors git's `revs->diffopt.output_format`: `-L`
    // forces `DIFF_FORMAT_PATCH` only when this is still unset at setup time, so
    // an explicit `-s`/`--no-patch` wins regardless of where it sits relative to
    // `-L` on the command line (revision.c: `if (!revs->diffopt.output_format)`).
    // Note `--format`/`--pretty` set the *commit* format, NOT a diff output
    // format, so they do not count here (`-L --format=%s` still defaults to a
    // patch).
    let mut diff_format_explicit = false;
    let mut diff_merges_on_requested = false;
    let mut diff_merges_from_m = false;
    let mut first_parent_requested = false;
    // Diff-format presentation options forwarded to the `-L` patch renderer.
    // The ordinary log path threads these through `setup_args` to the diff
    // machinery; the line-log path renders its restricted patch directly, so it
    // captures the same options here. `None`/default == git's defaults.
    let mut line_log_src_prefix: Option<String> = None;
    let mut line_log_dst_prefix: Option<String> = None;
    let mut line_log_full_index = false;
    let mut log_output_path: Option<String> = None;
    let mut diff_reverse = false;
    let mut line_log_dirstat_requested = false;
    let mut line_log_full_diff_requested = false;
    let mut line_log_color_moved_mode: Option<Option<sley_diff_merge::render::ColorMovedMode>> =
        None;
    let mut line_log_color_moved_ws: Option<sley_diff_merge::render::ColorMovedWs> = None;
    // Raw `-I<regex>` (`--ignore-matching-lines`) patterns, compiled after the
    // option scan so a malformed regex fails like git's diff_opt_ignore_regex.
    let mut ignore_regex_patterns: Vec<String> = Vec::new();
    // Pickaxe filtering: `-S<string>` (string-count change), `-G<regex>`
    // (added/removed line matches regex), `--find-object=<oid>`. Only the LAST
    // of these wins (git overwrites pickaxe/objfind each time), with
    // `--pickaxe-regex` switching `-S` to a regex needle and `--pickaxe-all`
    // showing the whole changeset when any filepair matches.
    let mut pickaxe: Option<PickaxeSpec> = None;
    let mut pickaxe_regex = false;
    let mut pickaxe_all = false;
    let mut find_object_patterns: Vec<String> = Vec::new();
    // `--diff-filter=<bits>`: accumulated positive bits and negated bits, git's
    // `filter` / `filter_not`. Resolved into a single mask after the scan.
    let mut diff_filter_bits: u32 = 0;
    let mut diff_filter_not_bits: u32 = 0;
    let mut diff_filter_given = false;
    // Explicit rename/copy detection overrides from `-M`/`-C`/`--no-renames`
    // (the command-line wins over `diff.renames` config for pickaxe/diff-filter
    // commit selection). `None` = defer to config.
    let mut renames_override: Option<bool> = None;
    let mut copies_override: Option<bool> = None;
    let mut find_copies_harder = false;
    // Track which pickaxe *kinds* were requested (git OR-s the bits and rejects
    // any combination of -G / -S / --find-object). `-S`/`-G` overwrite the
    // needle but each still records its kind-bit for the conflict check.
    let mut saw_s = false;
    let mut saw_g = false;
    // `--root` flag; falls back to the log.showRoot config (default true).
    let mut show_root_flag: Option<bool> = None;
    let mut line_prefix: Option<String> = None;
    let mut color_always = false;
    let mut color_explicit = false;
    let mut show_signature: Option<bool> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--" => {
                setup_args.push(arg.clone());
                setup_args.extend(iter.cloned());
                break;
            }
            "--end-of-options" => {
                end_of_options_revs.extend(iter.cloned());
                default_revision_given = true;
                break;
            }
            "--not" => {
                setup_not = !setup_not;
                setup_args.push(arg.clone());
            }
            "--stdin" => read_stdin = true,
            "--default" => {
                default_revision_given = true;
                setup_args.push(arg.clone());
                setup_args.push(
                    iter.next()
                        .ok_or_else(|| GitError::Command("--default requires a value".into()))?
                        .clone(),
                );
            }
            "--first-parent" => {
                first_parent_requested = true;
                setup_args.push(arg.clone());
            }
            "--full-history"
            | "--sparse"
            | "--dense"
            | "--remove-empty"
            | "--simplify-merges"
            | "--show-pulls"
            | "--ancestry-path"
            | "--exclude-first-parent-only"
            | "--no-exclude-first-parent-only"
            | "--reverse"
            | "--topo-order"
            | "--date-order"
            | "--author-date-order"
            | "--no-walk"
            | "--no-walk=sorted"
            | "--no-walk=unsorted"
            | "--do-walk"
            | "--all"
            | "--reflog"
            | "--no-reflog"
            | "--branches"
            | "--tags"
            | "--remotes"
            | "--no-ignore-missing" => setup_args.push(arg.clone()),
            "--boundary" => boundary = true,
            "-t" => {}
            "--ignore-missing" => {
                ignored_missing_input = true;
                setup_args.push(arg.clone());
            }
            "--parents" => show_parents = true,
            "--children" => show_children = true,
            "--abbrev-commit" => {
                abbrev_commit = true;
                abbrev_commit_explicit = true;
            }
            "--no-abbrev-commit" => {
                abbrev_commit = false;
                abbrev_commit_explicit = true;
            }
            "--abbrev" => {
                abbrev_len = Some(7);
                abbrev_len_explicit = true;
            }
            "--no-abbrev" => {
                abbrev_len = None;
                abbrev_len_explicit = true;
            }
            "--glob" | "--exclude" | "--exclude-hidden" => {
                setup_args.push(arg.clone());
                setup_args.push(
                    iter.next()
                        .ok_or_else(|| GitError::Command(format!("{arg} requires a value")))?
                        .clone(),
                );
            }
            value
                if value.starts_with("--glob=")
                    || value.starts_with("--exclude=")
                    || value.starts_with("--exclude-hidden=")
                    || value.starts_with("--branches=")
                    || value.starts_with("--tags=")
                    || value.starts_with("--remotes=") =>
            {
                setup_args.push(arg.clone());
            }
            "--author" => {
                let value = iter.next().ok_or_else(log_author_requires_value_error)?;
                author_patterns.push(value.to_string());
            }
            value if value.starts_with("--author=") => {
                author_patterns.push(value["--author=".len()..].to_string());
            }
            "--committer" => {
                let value = iter.next().ok_or_else(log_committer_requires_value_error)?;
                committer_patterns.push(value.to_string());
            }
            value if value.starts_with("--committer=") => {
                committer_patterns.push(value["--committer=".len()..].to_string());
            }
            "--grep" => {
                let value = iter.next().ok_or_else(log_grep_requires_value_error)?;
                grep_patterns.push(value.to_string());
            }
            value if value.starts_with("--grep=") => {
                grep_patterns.push(value["--grep=".len()..].to_string());
            }
            "--all-match" => grep_all_match = true,
            "--invert-grep" => invert_grep = true,
            "-i" | "--regexp-ignore-case" => regexp_ignore_case = true,
            "-F" | "--fixed-strings" => {
                pattern_kind = sley_grep::PatternKind::Fixed;
                pattern_kind_explicit = true;
            }
            "--basic-regexp" => {
                pattern_kind = sley_grep::PatternKind::Basic;
                pattern_kind_explicit = true;
            }
            "-E" | "--extended-regexp" => {
                pattern_kind = sley_grep::PatternKind::Extended;
                pattern_kind_explicit = true;
            }
            "-P" | "--perl-regexp" => {
                pattern_kind = sley_grep::PatternKind::Perl;
                pattern_kind_explicit = true;
            }
            // Pickaxe: `-S<string>`, `-G<regex>`, `--find-object=<oid>`. git's
            // parse-options treats a bare `-S`/`-G` (no value) as a "switch
            // requires a value" error (exit 129); an empty value is a distinct
            // `error: -S requires a non-empty argument` (also 129).
            "-S" => {
                let value = iter
                    .next()
                    .ok_or_else(|| log_pickaxe_requires_value_error("S"))?;
                if value.is_empty() {
                    return Err(log_pickaxe_empty_error("S"));
                }
                saw_s = true;
                pickaxe = Some(PickaxeSpec::String(value.to_string()));
            }
            value if value.starts_with("-S") => {
                if value.len() == 2 {
                    return Err(log_pickaxe_empty_error("S"));
                }
                saw_s = true;
                pickaxe = Some(PickaxeSpec::String(value[2..].to_string()));
            }
            "-G" => {
                let value = iter
                    .next()
                    .ok_or_else(|| log_pickaxe_requires_value_error("G"))?;
                if value.is_empty() {
                    return Err(log_pickaxe_empty_error("G"));
                }
                saw_g = true;
                pickaxe = Some(PickaxeSpec::Grep(value.to_string()));
            }
            value if value.starts_with("-G") => {
                if value.len() == 2 {
                    return Err(log_pickaxe_empty_error("G"));
                }
                saw_g = true;
                pickaxe = Some(PickaxeSpec::Grep(value[2..].to_string()));
            }
            "--find-object" => {
                let value = iter
                    .next()
                    .ok_or_else(|| log_option_requires_value_error("find-object"))?;
                find_object_patterns.push(value.to_string());
            }
            value if value.starts_with("--find-object=") => {
                find_object_patterns.push(value["--find-object=".len()..].to_string());
            }
            "--pickaxe-regex" => pickaxe_regex = true,
            "--pickaxe-all" => pickaxe_all = true,
            "-a" | "--text" => diff_opts.text = true,
            "-O" => {
                let value = iter
                    .next()
                    .ok_or_else(|| log_option_requires_value_error("O"))?;
                diff_opts.order_file = Some(value.to_string());
            }
            value if value.starts_with("-O") => {
                diff_opts.order_file = Some(value[2..].to_string());
            }
            "--rotate-to" => {
                let value = iter
                    .next()
                    .ok_or_else(|| log_option_requires_value_error("rotate-to"))?;
                diff_opts.rotate_to = Some(value.to_string());
                diff_opts.rotate_skip = false;
            }
            value if value.starts_with("--rotate-to=") => {
                diff_opts.rotate_to = Some(value["--rotate-to=".len()..].to_string());
                diff_opts.rotate_skip = false;
            }
            "--skip-to" => {
                let value = iter
                    .next()
                    .ok_or_else(|| log_option_requires_value_error("skip-to"))?;
                diff_opts.rotate_to = Some(value.to_string());
                diff_opts.rotate_skip = true;
            }
            value if value.starts_with("--skip-to=") => {
                diff_opts.rotate_to = Some(value["--skip-to=".len()..].to_string());
                diff_opts.rotate_skip = true;
            }
            "--diff-filter" => {
                let value = iter
                    .next()
                    .ok_or_else(|| log_option_requires_value_error("diff-filter"))?;
                parse_diff_filter_arg(value, &mut diff_filter_bits, &mut diff_filter_not_bits)?;
                diff_filter_given = true;
            }
            value if let Some(arg) = value.strip_prefix("--diff-filter=") => {
                parse_diff_filter_arg(arg, &mut diff_filter_bits, &mut diff_filter_not_bits)?;
                diff_filter_given = true;
            }
            "--no-pickaxe-regex" => {
                eprintln!("fatal: unrecognized argument: --no-pickaxe-regex");
                return Err(GitError::Exit(128));
            }
            "-g" | "--walk-reflogs" => walk_reflogs = true,
            "--no-walk-reflogs" => walk_reflogs = false,
            "--max-age" => {
                setup_args.push(arg.clone());
                setup_args.push(
                    iter.next()
                        .ok_or_else(log_max_age_requires_value_error)?
                        .clone(),
                );
            }
            "--min-age" => {
                setup_args.push(arg.clone());
                setup_args.push(
                    iter.next()
                        .ok_or_else(log_min_age_requires_value_error)?
                        .clone(),
                );
            }
            "--since" | "--after" | "--until" | "--before" => {
                setup_args.push(arg.clone());
                setup_args.push(
                    iter.next()
                        .ok_or_else(|| log_date_cutoff_requires_value_error(arg))?
                        .clone(),
                );
            }
            value
                if value.starts_with("--max-age=")
                    || value.starts_with("--min-age=")
                    || value.starts_with("--since=")
                    || value.starts_with("--after=")
                    || value.starts_with("--until=")
                    || value.starts_with("--before=") =>
            {
                setup_args.push(arg.clone());
            }
            "--merges" => min_parents = Some(2),
            "--no-merges" => max_parents = Some(1),
            "--no-min-parents" => min_parents = None,
            "--no-max-parents" => max_parents = None,
            "--use-mailmap" | "--mailmap" => use_mailmap_explicit = Some(true),
            "--no-use-mailmap" | "--no-mailmap" => use_mailmap_explicit = Some(false),
            "--show-signature" => show_signature = Some(true),
            "--no-show-signature" => show_signature = Some(false),
            "-q"
            | "--quiet"
            | "--no-quiet"
            | "--unpacked"
            | "--relative"
            | "--no-relative"
            | "--ext-diff"
            | "--no-ext-diff"
            | "--no-find-copies-harder"
            | "--function-context"
            | "--default-prefix"
            | "--break-rewrites"
            | "--binary"
            | "--no-binary"
            | "--irreversible-delete"
            | "--textconv"
            | "--no-textconv"
            | "--submodule"
            | "--ignore-submodules"
            | "--ita-visible-in-index"
            | "--ita-invisible-in-index"
            | "-B"
            | "-D"
            | "-W" => {}
            "--full-diff" => line_log_full_diff_requested = true,
            "--source" => show_source = true,
            "--no-source" => show_source = false,
            "--no-renames" => {
                renames_override = Some(false);
                copies_override = Some(false);
            }
            "--find-renames" | "-M" => renames_override = Some(true),
            "--find-copies" | "-C" => {
                renames_override = Some(true);
                if copies_override == Some(true) {
                    find_copies_harder = true;
                }
                copies_override = Some(true);
            }
            "--find-copies-harder" => {
                renames_override = Some(true);
                copies_override = Some(true);
                find_copies_harder = true;
            }
            "--indent-heuristic" => {
                diff_opts.indent_heuristic = true;
                indent_heuristic_explicit = true;
            }
            "--no-indent-heuristic" => {
                diff_opts.indent_heuristic = false;
                indent_heuristic_explicit = true;
            }
            "--minimal" => diff_opts.diff_algorithm = sley_diff_merge::DiffAlgorithm::Minimal,
            "--patience" => diff_opts.diff_algorithm = sley_diff_merge::DiffAlgorithm::Patience,
            "--histogram" => diff_opts.diff_algorithm = sley_diff_merge::DiffAlgorithm::Histogram,
            "--ignore-all-space" | "-w" => diff_opts.ws_ignore.all_space = true,
            "--ignore-space-change" | "-b" => diff_opts.ws_ignore.space_change = true,
            "-bw" | "-wb" => diff_opts.ws_ignore.all_space = true,
            "-R" => diff_reverse = true,
            "--ignore-space-at-eol" => diff_opts.ws_ignore.space_at_eol = true,
            "--ignore-cr-at-eol" => diff_opts.ws_ignore.cr_at_eol = true,
            "--ignore-blank-lines" => diff_opts.ignore_blank_lines = true,
            "--decorate" | "--decorate=short" | "--decorate=true" | "--decorate=1"
            | "--decorate=on" | "--decorate=yes" => {
                decoration = LogDecorationMode::Short;
                decoration_explicit = true;
            }
            "--decorate=full" => {
                decoration = LogDecorationMode::Full;
                decoration_explicit = true;
            }
            "--decorate=auto" => {
                // `auto` means "decorate iff stdout is a tty"; tests redirect
                // to a file, so this resolves to off.
                decoration = LogDecorationMode::Off;
                decoration_explicit = true;
            }
            "--no-decorate" | "--decorate=no" | "--decorate=" | "--decorate=false"
            | "--decorate=0" | "--decorate=off" => {
                decoration = LogDecorationMode::Off;
                decoration_explicit = true;
            }
            value if value.starts_with("--decorate=") => {
                return Err(GitError::Command(format!(
                    "invalid --decorate option {value}"
                )));
            }
            "--clear-decorations" => {
                clear_decorations = true;
                decorate_refs_include.clear();
                decorate_refs_exclude.clear();
            }
            "--no-decorate-refs" => decorate_refs_include.clear(),
            "--no-decorate-refs-exclude" => decorate_refs_exclude.clear(),
            "--simplify-by-decoration" => simplify_by_decoration = true,
            value if value.starts_with("-M") => {
                log_validate_similarity_option(&value[2..], "find-renames")?;
                renames_override = Some(true);
            }
            value if value.starts_with("-C") => {
                log_validate_similarity_option(&value[2..], "find-copies")?;
                renames_override = Some(true);
                if copies_override == Some(true) {
                    find_copies_harder = true;
                }
                copies_override = Some(true);
            }
            value if value.starts_with("-B") => {
                log_validate_break_rewrites_option(&value[2..])?;
            }
            value if value.starts_with("--relative=") => {}
            value if value.starts_with("--find-renames=") => {
                log_validate_similarity_option(&value["--find-renames=".len()..], "find-renames")?;
            }
            value if value.starts_with("--find-copies=") => {
                log_validate_similarity_option(&value["--find-copies=".len()..], "find-copies")?;
                if copies_override == Some(true) {
                    find_copies_harder = true;
                }
                renames_override = Some(true);
                copies_override = Some(true);
            }
            "--diff-merges" => {
                let value = iter
                    .next()
                    .ok_or_else(log_diff_merges_requires_value_error)?;
                let mode = log_parse_diff_merges(value)?;
                diff_merges_on_requested = value == "on";
                diff_opts.merges = Some(mode);
                diff_opts.merges_imply_patch = mode != LogDiffMerges::Off;
            }
            value if value.starts_with("--diff-merges=") => {
                let raw = &value["--diff-merges=".len()..];
                let mode = log_parse_diff_merges(raw)?;
                diff_merges_on_requested = raw == "on";
                diff_opts.merges = Some(mode);
                diff_opts.merges_imply_patch = mode != LogDiffMerges::Off;
            }
            value if value.starts_with("--no-walk=") => {
                return log_no_walk_invalid_argument(value);
            }
            value if value.starts_with("--min-parents=") => {
                min_parents = Some(log_parse_parent_count(&value["--min-parents=".len()..])?);
            }
            value if value.starts_with("--max-parents=") => {
                max_parents = Some(log_parse_parent_count(&value["--max-parents=".len()..])?);
            }
            value if value.starts_with("--abbrev=") => {
                abbrev_len = Some(log_parse_abbrev_width(&value["--abbrev=".len()..]));
                abbrev_len_explicit = true;
            }
            value if value.starts_with("--unpacked=") => {
                eprintln!("fatal: --unpacked=<packfile> no longer supported");
                return Err(GitError::Exit(128));
            }
            "--min-parents" | "--max-parents" => {
                return log_fatal_unrecognized_argument(arg);
            }
            value
                if value.starts_with("--merges=")
                    || value.starts_with("--no-merges=")
                    || value.starts_with("--no-min-parents=")
                    || value.starts_with("--no-max-parents=")
                    || value.starts_with("--parents=")
                    || value.starts_with("--no-parents=")
                    || value.starts_with("--children=")
                    || value.starts_with("--no-children=")
                    || value.starts_with("--abbrev-commit=")
                    || value.starts_with("--no-abbrev-commit=")
                    || value.starts_with("--topo-order=")
                    || value.starts_with("--date-order=")
                    || value.starts_with("--author-date-order=")
                    || value.starts_with("--sparse=")
                    || value.starts_with("--dense=")
                    || value.starts_with("--remove-empty=")
                    || value.starts_with("--full-history=")
                    || value.starts_with("--simplify-merges=")
                    || value.starts_with("--show-pulls=")
                    || value.starts_with("--all=")
                    || value.starts_with("--no-all=")
                    || value.starts_with("--no-branches=")
                    || value.starts_with("--no-tags=")
                    || value.starts_with("--no-remotes=")
                    || value.starts_with("--no-author=")
                    || value.starts_with("--no-committer=")
                    || value.starts_with("--no-max-age=")
                    || value.starts_with("--no-min-age=")
                    || value.starts_with("--no-since=")
                    || value.starts_with("--no-after=")
                    || value.starts_with("--no-until=")
                    || value.starts_with("--no-before=") =>
            {
                return log_fatal_unrecognized_argument(value);
            }
            "--no-parents" | "--no-children" | "--no-all" | "--no-branches" | "--no-tags"
            | "--no-remotes" | "--no-author" | "--no-committer" | "--no-max-age"
            | "--no-min-age" | "--no-since" | "--no-after" | "--no-until" | "--no-before" => {
                return log_fatal_unrecognized_argument(arg);
            }
            value
                if value == "--no-grep"
                    || value.starts_with("--no-grep=")
                    || value.starts_with("--all-match=")
                    || value.starts_with("--no-all-match")
                    || value.starts_with("--invert-grep=")
                    || value.starts_with("--no-invert-grep")
                    || value.starts_with("--regexp-ignore-case=")
                    || value.starts_with("--no-regexp-ignore-case")
                    || value.starts_with("--fixed-strings=")
                    || value.starts_with("--no-fixed-strings")
                    || value.starts_with("--basic-regexp=")
                    || value.starts_with("--no-basic-regexp")
                    || value.starts_with("--extended-regexp=")
                    || value.starts_with("--no-extended-regexp") =>
            {
                return log_fatal_unrecognized_argument(value);
            }
            "--no-first-parent" => {
                return log_fatal_unrecognized_argument(arg);
            }
            value
                if value.starts_with("--first-parent=")
                    || value.starts_with("--no-first-parent=") =>
            {
                return log_fatal_unrecognized_argument(value);
            }
            "--date" => {
                let value = iter.next().ok_or_else(log_date_requires_value_error)?;
                date_mode = log_date_mode(value)?;
                date_explicit = true;
            }
            value if value.starts_with("--date=") => {
                date_mode = log_date_mode(&value["--date=".len()..])?;
                date_explicit = true;
            }
            "--diff-algorithm" => {
                let value = iter
                    .next()
                    .ok_or_else(|| log_option_requires_value_error("diff-algorithm"))?;
                log_validate_diff_algorithm(value)?;
                diff_opts.diff_algorithm = log_parse_diff_algorithm(value);
            }
            value if value.starts_with("--diff-algorithm=") => {
                let algo = &value["--diff-algorithm=".len()..];
                log_validate_diff_algorithm(algo)?;
                diff_opts.diff_algorithm = log_parse_diff_algorithm(algo);
            }
            "--anchored" => {
                iter.next()
                    .ok_or_else(|| log_option_requires_value_error("anchored"))?;
            }
            value if value.starts_with("--anchored=") => {}
            "--ignore-matching-lines" | "-I" => {
                let value = iter
                    .next()
                    .ok_or_else(|| log_option_requires_value_error("ignore-matching-lines"))?;
                ignore_regex_patterns.push(value.to_string());
            }
            value if value.starts_with("--ignore-matching-lines=") => {
                ignore_regex_patterns.push(value["--ignore-matching-lines=".len()..].to_string());
            }
            value if value.starts_with("-I") && value.len() > 2 => {
                ignore_regex_patterns.push(value[2..].to_string());
            }
            "--inter-hunk-context" => {
                let value = iter
                    .next()
                    .ok_or_else(|| log_option_requires_value_error("inter-hunk-context"))?;
                log_validate_inter_hunk_context(value)?;
            }
            "--inter-hunk-context=" => {
                return log_inter_hunk_context_requires_number_error();
            }
            value if value.starts_with("--inter-hunk-context=") => {
                log_validate_inter_hunk_context(&value["--inter-hunk-context=".len()..])?;
            }
            "--no-prefix" => {
                line_log_src_prefix = Some(String::new());
                line_log_dst_prefix = Some(String::new());
            }
            "--full-index" => line_log_full_index = true,
            "--src-prefix" => {
                let value = iter
                    .next()
                    .ok_or_else(|| log_option_requires_value_error("src-prefix"))?;
                line_log_src_prefix = Some(value.clone());
            }
            "--dst-prefix" => {
                let value = iter
                    .next()
                    .ok_or_else(|| log_option_requires_value_error("dst-prefix"))?;
                line_log_dst_prefix = Some(value.clone());
            }
            value if let Some(p) = value.strip_prefix("--src-prefix=") => {
                line_log_src_prefix = Some(p.to_string());
            }
            value if let Some(p) = value.strip_prefix("--dst-prefix=") => {
                line_log_dst_prefix = Some(p.to_string());
            }
            value if value.starts_with("--break-rewrites=") => {
                log_validate_break_rewrites_option(&value["--break-rewrites=".len()..])?;
            }
            value if value.starts_with("--submodule=") => {
                log_validate_submodule_format(&value["--submodule=".len()..])?;
            }
            value if value.starts_with("--ignore-submodules=") => {
                log_validate_ignore_submodules(&value["--ignore-submodules=".len()..])?;
            }
            value if value.starts_with("--color-moved=") => {
                let mode = &value["--color-moved=".len()..];
                log_validate_color_moved(mode)?;
                line_log_color_moved_mode =
                    Some(sley_rev::diff_options::parse_color_moved_mode(mode)?);
            }
            "--color-moved" => {
                line_log_color_moved_mode =
                    Some(sley_rev::diff_options::parse_color_moved_mode("")?);
            }
            "--no-color-moved" => {
                line_log_color_moved_mode = Some(None);
            }
            "--graph" => graph = true,
            "--no-graph" => graph = false,
            "--show-linear-break" => show_linear_break = true,
            value if value.starts_with("--show-linear-break=") => show_linear_break = true,
            "--color" => {
                color_always = true;
                color_explicit = true;
            }
            "--no-color" => {
                color_always = false;
                color_explicit = true;
            }
            "--line-prefix" => {
                let value = iter
                    .next()
                    .ok_or_else(|| log_option_requires_value_error("line-prefix"))?;
                line_prefix = Some(value.to_string());
            }
            value if value.starts_with("--line-prefix=") => {
                line_prefix = Some(value["--line-prefix=".len()..].to_string());
            }
            value if value.starts_with("--color=") => {
                log_validate_color(&value["--color=".len()..])?;
                color_always = value["--color=".len()..].eq_ignore_ascii_case("always");
                color_explicit = true;
            }
            "--output" => {
                let value = iter
                    .next()
                    .ok_or_else(|| log_option_requires_value_error("output"))?;
                log_output_path = Some(value.to_string());
            }
            value if let Some(path) = value.strip_prefix("--output=") => {
                log_output_path = Some(path.to_string());
            }
            "--color-moved-ws" => {
                let value = iter
                    .next()
                    .ok_or_else(|| log_option_requires_value_error("color-moved-ws"))?;
                log_validate_color_moved_ws(value)?;
                line_log_color_moved_ws =
                    Some(sley_rev::diff_options::parse_color_moved_ws(value)?);
            }
            value if value.starts_with("--color-moved-ws=") => {
                let mode = &value["--color-moved-ws=".len()..];
                log_validate_color_moved_ws(mode)?;
                line_log_color_moved_ws = Some(sley_rev::diff_options::parse_color_moved_ws(mode)?);
            }
            "--ws-error-highlight" => {
                let value = iter
                    .next()
                    .ok_or_else(|| log_option_requires_value_error("ws-error-highlight"))?;
                log_validate_ws_error_highlight(value)?;
            }
            value if value.starts_with("--ws-error-highlight=") => {
                log_validate_ws_error_highlight(&value["--ws-error-highlight=".len()..])?;
            }
            "--output-indicator-new" => {
                let value = iter
                    .next()
                    .ok_or_else(|| log_option_requires_value_error("output-indicator-new"))?;
                log_validate_output_indicator_for_log("output-indicator-new", value)?;
                diff_opts.line_indicators.new =
                    log_output_indicator_byte(value, diff_opts.line_indicators.new);
            }
            "--output-indicator-old" => {
                let value = iter
                    .next()
                    .ok_or_else(|| log_option_requires_value_error("output-indicator-old"))?;
                log_validate_output_indicator_for_log("output-indicator-old", value)?;
                diff_opts.line_indicators.old =
                    log_output_indicator_byte(value, diff_opts.line_indicators.old);
            }
            "--output-indicator-context" => {
                let value = iter
                    .next()
                    .ok_or_else(|| log_option_requires_value_error("output-indicator-context"))?;
                log_validate_output_indicator_for_log("output-indicator-context", value)?;
                diff_opts.line_indicators.context =
                    log_output_indicator_byte(value, diff_opts.line_indicators.context);
            }
            value if value.starts_with("--output-indicator-new=") => {
                let indicator = &value["--output-indicator-new=".len()..];
                log_validate_output_indicator_for_log("output-indicator-new", indicator)?;
                diff_opts.line_indicators.new =
                    log_output_indicator_byte(indicator, diff_opts.line_indicators.new);
            }
            value if value.starts_with("--output-indicator-old=") => {
                let indicator = &value["--output-indicator-old=".len()..];
                log_validate_output_indicator_for_log("output-indicator-old", indicator)?;
                diff_opts.line_indicators.old =
                    log_output_indicator_byte(indicator, diff_opts.line_indicators.old);
            }
            value if value.starts_with("--output-indicator-context=") => {
                let indicator = &value["--output-indicator-context=".len()..];
                log_validate_output_indicator_for_log("output-indicator-context", indicator)?;
                diff_opts.line_indicators.context =
                    log_output_indicator_byte(indicator, diff_opts.line_indicators.context);
            }
            value if value.starts_with("--no-renames=") => {
                return log_option_takes_no_value_error("no-renames");
            }
            value if value.starts_with("--no-patch=") => {
                return log_option_takes_no_value_error("no-patch");
            }
            value if value.starts_with("--no-diff-merges=") => {
                return log_fatal_unrecognized_argument(value);
            }
            value if value.starts_with("--no-prefix=") => {
                return log_option_takes_no_value_error("no-prefix");
            }
            value if value.starts_with("--default-prefix=") => {
                return log_option_takes_no_value_error("default-prefix");
            }
            value if value.starts_with("--full-index=") => {
                return log_option_takes_no_value_error("full-index");
            }
            value if value.starts_with("--no-abbrev=") => {
                return log_option_takes_no_value_error("no-abbrev");
            }
            value if value.starts_with("--irreversible-delete=") => {
                return log_option_takes_no_value_error("irreversible-delete");
            }
            value if value.starts_with("--textconv=") => {
                return log_option_takes_no_value_error("textconv");
            }
            value if value.starts_with("--no-textconv=") => {
                return log_option_takes_no_value_error("no-textconv");
            }
            value if value.starts_with("--no-color-moved=") => {
                return log_option_takes_no_value_error("no-color-moved");
            }
            value if value.starts_with("--no-color=") => {
                return log_option_takes_no_value_error("no-color");
            }
            value if value.starts_with("--ita-visible-in-index=") => {
                return log_option_takes_no_value_error("ita-visible-in-index");
            }
            value if value.starts_with("--ita-invisible-in-index=") => {
                return log_option_takes_no_value_error("ita-invisible-in-index");
            }
            value if value.starts_with("--pickaxe-all=") => {
                return log_option_takes_no_value_error("pickaxe-all");
            }
            value if value.starts_with("--pickaxe-regex=") => {
                return log_option_takes_no_value_error("pickaxe-regex");
            }
            value if value.starts_with("--find-copies-harder=") => {
                return log_option_takes_no_value_error("find-copies-harder");
            }
            value if value.starts_with("--no-find-copies-harder=") => {
                return log_option_takes_no_value_error("no-find-copies-harder");
            }
            value if value.starts_with("--indent-heuristic=") => {
                return log_option_takes_no_value_error("indent-heuristic");
            }
            value if value.starts_with("--no-indent-heuristic=") => {
                return log_option_takes_no_value_error("no-indent-heuristic");
            }
            value if value.starts_with("--ignore-space-at-eol=") => {
                return log_option_takes_no_value_error("ignore-space-at-eol");
            }
            value if value.starts_with("--ignore-cr-at-eol=") => {
                return log_option_takes_no_value_error("ignore-cr-at-eol");
            }
            value if value.starts_with("--ignore-space-change=") => {
                return log_option_takes_no_value_error("ignore-space-change");
            }
            value if value.starts_with("--ignore-all-space=") => {
                return log_option_takes_no_value_error("ignore-all-space");
            }
            value if value.starts_with("--ignore-blank-lines=") => {
                return log_option_takes_no_value_error("ignore-blank-lines");
            }
            value if value.starts_with("--function-context=") => {
                return log_option_takes_no_value_error("function-context");
            }
            value if value.starts_with("--no-relative=") => {
                return log_option_takes_no_value_error("no-relative");
            }
            value if value.starts_with("--ext-diff=") => {
                return log_option_takes_no_value_error("ext-diff");
            }
            value if value.starts_with("--no-ext-diff=") => {
                return log_option_takes_no_value_error("no-ext-diff");
            }
            value if value.starts_with("--clear-decorations=") => {
                return log_option_takes_no_value_error("clear-decorations");
            }
            value if value.starts_with("--no-decorate-refs=") => {
                return log_option_takes_no_value_error("no-decorate-refs");
            }
            value if value.starts_with("--no-decorate-refs-exclude=") => {
                return log_option_takes_no_value_error("no-decorate-refs-exclude");
            }
            value if value.starts_with("--do-walk=") => {
                return log_fatal_unrecognized_argument(value);
            }
            "--decorate-refs" => {
                let value = iter
                    .next()
                    .ok_or_else(|| log_option_requires_value_error("decorate-refs"))?;
                decorate_refs_include.push(value.to_string());
            }
            "--decorate-refs-exclude" => {
                let value = iter
                    .next()
                    .ok_or_else(|| log_option_requires_value_error("decorate-refs-exclude"))?;
                decorate_refs_exclude.push(value.to_string());
            }
            value if value.starts_with("--decorate-refs=") => {
                decorate_refs_include.push(value["--decorate-refs=".len()..].to_string());
            }
            value if value.starts_with("--decorate-refs-exclude=") => {
                decorate_refs_exclude.push(value["--decorate-refs-exclude=".len()..].to_string());
            }
            value if value.starts_with("--use-mailmap=") => {
                return log_option_takes_no_value_error("use-mailmap");
            }
            value if value.starts_with("--no-use-mailmap=") => {
                return log_option_takes_no_value_error("no-use-mailmap");
            }
            value if value.starts_with("--mailmap=") => {
                return log_option_takes_no_value_error("mailmap");
            }
            value if value.starts_with("--no-mailmap=") => {
                return log_option_takes_no_value_error("no-mailmap");
            }
            value if value.starts_with("--encoding=") => {
                output_encoding_override = Some(value["--encoding=".len()..].to_string());
            }
            "--notes" | "--show-notes" => notes_display.add_default(),
            value if value.starts_with("--notes=") => {
                notes_display.add_ref(&value["--notes=".len()..]);
            }
            value if value.starts_with("--show-notes=") => {
                notes_display.add_show_ref(&value["--show-notes=".len()..]);
            }
            "--no-notes" => notes_display.disable(),
            "--no-standard-notes" => notes_display.no_standard(),
            "--standard-notes" => notes_display.add_standard(),
            value if value.starts_with("--no-notes=") => {
                return log_fatal_unrecognized_argument(value);
            }
            value if value.starts_with("--no-show-signature=") => {
                return log_fatal_unrecognized_argument(value);
            }
            "-z" | "--null" => null_terminate = true,
            "--no-null" => null_terminate = false,
            "--oneline" => {
                preset_oneline = Some(false);
                pretty_spec = None;
                plain_oneline = true;
            }
            // Built-in `short`/`medium` map to the default-output kinds (short
            // omits the `Date:` line); other named/custom formats fall through
            // to the compiled `pretty_spec` path below.
            "--pretty=short" | "--format=short" => {
                output = LogOutput::Default(LogDefaultKind::Short);
                pretty_spec = None;
                preset_oneline = None;
                plain_oneline = false;
            }
            "--pretty=medium" | "--format=medium" => {
                output = LogOutput::Default(LogDefaultKind::Medium);
                pretty_spec = None;
                preset_oneline = None;
                plain_oneline = false;
            }
            // Bare `--pretty` is shorthand for `--pretty=medium` (it does not
            // consume the following argument). Bare `--format` keeps requiring a
            // value below.
            "--pretty" => {
                output = LogOutput::Default(LogDefaultKind::Medium);
                pretty_spec = None;
                preset_oneline = None;
                plain_oneline = false;
            }
            "--format" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command(format!("{arg} requires a value")))?;
                pretty_spec = Some((value.to_string(), true));
                preset_oneline = None;
                plain_oneline = false;
            }
            "--expand-tabs" => expand_tabs_explicit = Some(8),
            "--no-expand-tabs" => expand_tabs_explicit = Some(0),
            value if value.starts_with("--expand-tabs=") => {
                let raw = &value["--expand-tabs=".len()..];
                let n: i32 = raw.parse().map_err(|_| {
                    GitError::Command(format!("could not parse expand-tabs value '{raw}'"))
                })?;
                expand_tabs_explicit = Some(n.max(0));
            }
            value if value.starts_with("--pretty=") => {
                pretty_spec = Some((value["--pretty=".len()..].to_string(), false));
                preset_oneline = None;
                plain_oneline = false;
            }
            "-n" | "--max-count" => {
                setup_args.push(arg.clone());
                setup_args.push(
                    iter.next()
                        .ok_or_else(|| GitError::Command(format!("{arg} requires a value")))?
                        .clone(),
                );
            }
            value if value.starts_with("--max-count=") => {
                setup_args.push(arg.clone());
            }
            "--skip" => {
                setup_args.push(arg.clone());
                setup_args.push(
                    iter.next()
                        .ok_or_else(|| GitError::Command("--skip requires a value".into()))?
                        .clone(),
                );
            }
            value if value.starts_with("--skip=") => {
                setup_args.push(arg.clone());
            }
            value if value.starts_with("--format=") => {
                pretty_spec = Some((value["--format=".len()..].to_string(), true));
                preset_oneline = None;
                plain_oneline = false;
            }
            value if value.starts_with("-n") && value.len() > 2 => {
                setup_args.push(arg.clone());
            }
            value
                if value.starts_with('-')
                    && value[1..].bytes().all(|byte| byte.is_ascii_digit()) =>
            {
                setup_args.push(arg.clone());
            }
            "-p" | "-u" | "--patch" => {
                diff_opts.patch = true;
                diff_format_explicit = true;
            }
            "--word-diff" => {
                if diff_opts.word_diff_mode.is_none() {
                    diff_opts.word_diff_mode = Some(commands::diff_words::WordDiffMode::Plain);
                }
                diff_opts.patch = true;
                diff_format_explicit = true;
            }
            value if let Some(mode) = value.strip_prefix("--word-diff=") => {
                diff_opts.word_diff_mode = match mode {
                    "plain" => Some(commands::diff_words::WordDiffMode::Plain),
                    "color" => Some(commands::diff_words::WordDiffMode::Color),
                    "porcelain" => Some(commands::diff_words::WordDiffMode::Porcelain),
                    "none" => None,
                    _ => {
                        eprintln!("error: bad --word-diff argument: {mode}");
                        return Err(GitError::Exit(129));
                    }
                };
                diff_opts.patch = true;
                diff_format_explicit = true;
            }
            "--word-diff-regex" => {
                let value = iter
                    .next()
                    .ok_or_else(|| log_option_requires_value_error("word-diff-regex"))?;
                diff_opts.word_diff_regex = Some(value.clone());
                if diff_opts.word_diff_mode.is_none() {
                    diff_opts.word_diff_mode = Some(commands::diff_words::WordDiffMode::Plain);
                }
                diff_opts.patch = true;
                diff_format_explicit = true;
            }
            value if let Some(regex) = value.strip_prefix("--word-diff-regex=") => {
                diff_opts.word_diff_regex = Some(regex.to_string());
                if diff_opts.word_diff_mode.is_none() {
                    diff_opts.word_diff_mode = Some(commands::diff_words::WordDiffMode::Plain);
                }
                diff_opts.patch = true;
                diff_format_explicit = true;
            }
            "-U" | "--unified" => {
                diff_opts.context = Some(3);
                diff_opts.patch = true;
                diff_format_explicit = true;
            }
            value if value.starts_with("-U") && value.len() > 2 => {
                let raw = &value[2..];
                patch_validate_unified_context(raw, true)?;
                diff_opts.context = Some(sley_rev::diff_options::parse_unified_count(raw));
                diff_opts.patch = true;
                diff_format_explicit = true;
            }
            "--unified=" => {
                return commit_unified_expects_numerical_value_error(false);
            }
            value if value.starts_with("--unified=") => {
                let raw = &value["--unified=".len()..];
                patch_validate_unified_context(raw, false)?;
                diff_opts.context = Some(sley_rev::diff_options::parse_unified_count(raw));
                diff_opts.patch = true;
                diff_format_explicit = true;
            }
            "-s" | "--no-patch" => {
                diff_opts = LogDiffOptions::default();
                diff_format_explicit = true;
            }
            "--stat" => {
                diff_opts.stat = true;
                diff_format_explicit = true;
            }
            value
                if value.starts_with("--stat=")
                    || value.starts_with("--stat-width=")
                    || value.starts_with("--stat-name-width=")
                    || value.starts_with("--stat-graph-width=")
                    || value.starts_with("--stat-count=") =>
            {
                diff_opts.stat = true;
                diff_format_explicit = true;
                diff_stat_parse_width_option(value, &mut diff_opts.stat_widths)?;
                if let Some(count) = diff_stat_count_option(value)? {
                    diff_opts.stat_count = count;
                }
            }
            "--compact-summary" => {
                diff_opts.compact_summary = true;
                diff_format_explicit = true;
            }
            "--numstat" => {
                diff_opts.numstat = true;
                diff_format_explicit = true;
            }
            "--shortstat" => {
                diff_opts.shortstat = true;
                diff_format_explicit = true;
            }
            "--dirstat" => {
                line_log_dirstat_requested = true;
                diff_format_explicit = true;
            }
            value if value.starts_with("--dirstat=") => {
                line_log_dirstat_requested = true;
                diff_format_explicit = true;
            }
            "--summary" => {
                diff_opts.summary = true;
                diff_format_explicit = true;
            }
            "--patch-with-stat" => {
                diff_opts.patch = true;
                diff_opts.stat = true;
                diff_format_explicit = true;
            }
            "--patch-with-raw" => {
                diff_opts.patch = true;
                diff_opts.raw = true;
                diff_format_explicit = true;
            }
            "--raw" => {
                diff_opts.raw = true;
                diff_format_explicit = true;
            }
            "--name-only" => {
                diff_opts.name_only = true;
                diff_format_explicit = true;
            }
            "--name-status" => {
                diff_opts.name_status = true;
                diff_format_explicit = true;
            }
            "-m" => {
                diff_opts.merges = Some(LogDiffMerges::Separate);
                diff_merges_from_m = true;
            }
            // `-c`/`--cc` select combined merge output and imply a *global*
            // patch (git's `merges_imply_patch` sets `output_format=PATCH`, so
            // non-merge commits get an ordinary patch too).
            "-c" => {
                diff_opts.merges = Some(LogDiffMerges::Combined { dense: false });
                diff_opts.merges_imply_patch = true;
                diff_opts.patch = true;
            }
            "--cc" => {
                diff_opts.merges = Some(LogDiffMerges::Combined { dense: true });
                diff_opts.merges_imply_patch = true;
                diff_opts.patch = true;
            }
            // `--dd` is first-parent merges that also imply a global patch
            // (git's `set_first_parent` + `merges_imply_patch`).
            "--dd" => {
                diff_opts.merges = Some(LogDiffMerges::FirstParent);
                diff_opts.merges_imply_patch = true;
                diff_opts.patch = true;
            }
            "--no-diff-merges" => diff_opts.merges = Some(LogDiffMerges::Off),
            "--root" => show_root_flag = Some(true),
            "--follow" => {
                saw_follow = true;
                follow_explicit = true;
            }
            "--no-follow" => {
                saw_follow = false;
                follow_explicit = true;
            }
            // `-L<range>:<file>` (attached) or `-L <range>:<file>` (separate).
            "-L" => {
                let value = iter.next().ok_or_else(|| {
                    eprintln!("error: switch `L' requires a value");
                    GitError::Exit(129)
                })?;
                line_log_args.push(crate::commands::line_log::LineLogArg { raw: value.clone() });
                // `-L` does NOT eagerly force a patch here; the default is
                // applied after the option scan only when no explicit diff
                // output format was requested (see `diff_format_explicit`).
            }
            value if let Some(arg) = value.strip_prefix("-L") => {
                line_log_args.push(crate::commands::line_log::LineLogArg {
                    raw: arg.to_string(),
                });
            }
            value if value.starts_with('-') => {
                return Err(GitError::Command(format!("unsupported log option {value}")));
            }
            value => {
                if ignored_missing_input {
                    revision_input_with_ignore_missing = true;
                }
                if let Some(unsupported) = log_follow_unsupported_pathspec_magic(value) {
                    if saw_follow {
                        eprintln!("fatal: pathspec magic not supported by --follow: {unsupported}");
                        return Err(GitError::Exit(128));
                    }
                    follow_config_allowed = false;
                }
                if let Some((_, path)) = log_pathspec_magic(value) {
                    setup_args.push(path.to_string());
                } else if value == ".." && Path::new(value).is_dir() {
                    if !setup_args.iter().any(|arg| arg == "--") {
                        setup_args.push("--".to_string());
                    }
                    setup_args.push(value.to_string());
                } else {
                    setup_args.push(value.to_string());
                }
            }
        }
    }
    // `-L` defaults to a patch only when no explicit diff output format was
    // requested (revision.c: `if (!revs->diffopt.output_format) output_format =
    // DIFF_FORMAT_PATCH`). This runs after the full option scan so `-s`/`-p`
    // win regardless of their position relative to `-L`.
    if !line_log_args.is_empty() && !diff_format_explicit {
        diff_opts.patch = true;
    }
    if read_stdin {
        let mut input = String::new();
        io::stdin().read_to_string(&mut input)?;
        if ignored_missing_input && input.lines().any(|line| !line.is_empty()) {
            revision_input_with_ignore_missing = true;
        }
        let stdin_args = input
            .lines()
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
        crate::commands::rev_list::merge_setup_args_from_stdin(
            &mut setup_args,
            stdin_args,
            setup_not,
        );
    }
    if show_parents && show_children {
        eprintln!("fatal: options '--parents' and '--children' cannot be used together");
        return Err(GitError::Exit(128));
    }
    if whatchanged && !diff_opts.any() {
        diff_opts.raw = true;
    }
    let repository = crate::repository::RepositoryContext::from_session(cli_session)?;
    let git_dir = repository.git_dir().to_path_buf();
    let format = repository.format();
    let config = repository.config().clone();
    let db = repository.repository().objects_mut();
    let has_commit_grafts = !sley_rev::revlist::load_commit_grafts(&db, format).is_empty();
    let cwd = repository.cwd().to_path_buf();
    let worktree_root = repository.worktree_root().ok().map(Path::to_path_buf);
    for rev in end_of_options_revs {
        if rev.starts_with('-') {
            match repository.resolve_revision(&rev) {
                Ok(oid) => setup_args.push(oid.to_hex()),
                Err(_) => {
                    let head_ref = format!("refs/heads/{rev}");
                    let tag_ref = format!("refs/tags/{rev}");
                    match repository
                        .resolve_revision(&head_ref)
                        .or_else(|_| repository.resolve_revision(&tag_ref))
                    {
                        Ok(oid) => setup_args.push(oid.to_hex()),
                        Err(_) => setup_args.push(rev),
                    }
                }
            }
        } else {
            setup_args.push(rev);
        }
    }
    if !default_revision_given && !revision_input_with_ignore_missing {
        setup_args.splice(0..0, ["--default".to_string(), "HEAD".to_string()]);
        inserted_default_head = true;
    }
    // `diff.indentHeuristic` sets the default; a CLI
    // `--indent-heuristic`/`--no-indent-heuristic` (tracked above) overrides it.
    if !indent_heuristic_explicit {
        diff_opts.indent_heuristic = config
            .get_bool("diff", None, "indentheuristic")
            .unwrap_or(true);
    }
    // `log.diffMerges` provides the default merge-diff mode and is validated
    // unconditionally at startup (git rejects a wrong value with exit 128 even
    // for a plain `git log` with no diff output).
    let config_diff_merges = match config.get("log", None, "diffMerges") {
        Some(value) => Some(log_parse_diff_merges_config(value)?),
        None => None,
    };
    if diff_merges_on_requested {
        diff_opts.merges = Some(config_diff_merges.unwrap_or(LogDiffMerges::Separate));
    } else if diff_merges_from_m
        && (first_parent_requested || config_diff_merges == Some(LogDiffMerges::FirstParent))
    {
        diff_opts.merges = Some(LogDiffMerges::FirstParent);
    } else if diff_opts.merges.is_none() {
        diff_opts.merges = config_diff_merges;
    }
    let output_encoding = output_encoding_override.unwrap_or_else(|| log_output_encoding(&config));
    let color_config = match global_config_value("color.diff")? {
        Some(value) => Some(value),
        None => global_config_value("color.ui")?,
    }
    .or_else(|| config.get("color", None, "diff").map(str::to_string))
    .or_else(|| config.get("color", None, "ui").map(str::to_string));
    if !color_explicit
        && !color_always
        && color_config
            .as_deref()
            .is_some_and(log_config_color_is_always)
    {
        color_always = true;
    }
    let line_log_color_moved_mode = match line_log_color_moved_mode {
        Some(mode) => mode,
        None => match config.get("diff", None, "colormoved").map(str::to_string) {
            Some(value) => sley_rev::diff_options::parse_color_moved_mode(&value)?,
            None => None,
        },
    };
    let line_log_color_moved_ws = match line_log_color_moved_ws {
        Some(ws) => ws,
        None => match config.get("diff", None, "colormovedws").map(str::to_string) {
            Some(value) => sley_rev::diff_options::parse_color_moved_ws(&value)?,
            None => sley_diff_merge::render::ColorMovedWs::default(),
        },
    };
    let line_log_color_moved =
        line_log_color_moved_mode.map(|mode| sley_diff_merge::render::ColorMoved {
            mode,
            ws: line_log_color_moved_ws,
        });
    if !abbrev_commit_explicit
        && config
            .get_bool("log", None, "abbrevcommit")
            .unwrap_or(false)
    {
        abbrev_commit = true;
    }
    if abbrev_commit
        && !abbrev_len_explicit
        && matches!(
            config
                .get("core", None, "abbrev")
                .map(|value| value.trim().to_ascii_lowercase())
                .as_deref(),
            Some("false") | Some("no") | Some("off") | Some("0")
        )
    {
        abbrev_len = None;
        abbrev_len_explicit = true;
    }
    // Mailmap: git's `log.mailmap` defaults to true (`use_mailmap_config = 1`);
    // `--use-mailmap`/`--no-use-mailmap` (and `--mailmap`/`--no-mailmap` aliases)
    // override. When enabled, the *whole* identity is mapped (default formats and
    // the lower-case `%an`/… atoms); the upper-case `%aN`/… atoms always map.
    let use_mailmap = use_mailmap_explicit
        .unwrap_or_else(|| config.get_bool("log", None, "mailmap").unwrap_or(true));
    let show_signature = show_signature.unwrap_or_else(|| {
        config
            .get_bool("log", None, "showsignature")
            .or_else(|| config.get_bool("log", None, "showSignature"))
            .unwrap_or(false)
    });
    let empty_mailmap = commands::utility::Mailmap::default();
    let mut mailmap_cache = None;
    let setup = match sley_rev::setup_revisions(
        &setup_args,
        &sley_rev::RevisionSetupContext {
            git_dir: &git_dir,
            worktree_root: worktree_root.as_deref(),
            cwd: &cwd,
            format,
            reader: &db,
            config: Some(&config),
        },
    ) {
        Ok(setup) => setup,
        Err(err) if inserted_default_head => {
            if repository.resolve_revision("HEAD").is_err()
                && let Some(branch) = log_unborn_head_branch(&git_dir)
            {
                eprintln!("fatal: your current branch '{branch}' does not have any commits yet");
                return Err(GitError::Exit(128));
            }
            return Err(err);
        }
        Err(err) => return Err(err),
    };
    if let Some(leftover) = setup.leftovers.first() {
        return Err(GitError::Command(format!(
            "unsupported log option {leftover}"
        )));
    }
    let mut revision_options = setup.options;
    if revision_options.ignore_missing {
        revision_options
            .positives
            .retain(|tip| db.read_object(&tip.oid).is_ok());
    }
    let max_count = revision_options.max_count;
    let skip = revision_options.skip;
    let max_age = revision_options.date_window.min_time;
    let min_age = revision_options.date_window.max_time;
    let reverse = revision_options.reverse;
    let ordering = match revision_options.order {
        sley_rev::RevisionOrder::Default => RevListOrdering::Default,
        sley_rev::RevisionOrder::Topo => RevListOrdering::Topo,
        sley_rev::RevisionOrder::Date => RevListOrdering::Date,
        sley_rev::RevisionOrder::AuthorDate => RevListOrdering::AuthorDate,
    };
    let (walk, no_walk_unsorted) = match revision_options.no_walk {
        sley_rev::NoWalkMode::Walk => (true, true),
        sley_rev::NoWalkMode::Sorted => (false, false),
        sley_rev::NoWalkMode::Unsorted => (false, true),
    };
    let first_parent = revision_options.first_parent;
    let pathspecs = setup.pathspecs;
    if !follow_explicit
        && !saw_follow
        && follow_config_allowed
        && pathspecs.len() == 1
        && config.get_bool("log", None, "follow").unwrap_or(false)
    {
        saw_follow = true;
    }
    let full_history = revision_options.full_history;
    if graph && reverse {
        eprintln!("fatal: options '--reverse' and '--graph' cannot be used together");
        return Err(GitError::Exit(128));
    }
    if graph && show_linear_break {
        eprintln!("fatal: options '--show-linear-break' and '--graph' cannot be used together");
        return Err(GitError::Exit(128));
    }
    if graph && !walk {
        eprintln!("fatal: options '--no-walk' and '--graph' cannot be used together");
        return Err(GitError::Exit(128));
    }
    if graph && walk_reflogs {
        eprintln!("fatal: options '--walk-reflogs' and '--graph' cannot be used together");
        return Err(GitError::Exit(128));
    }
    // Compile any `-I<regex>` patterns now (a malformed regex fails like git's
    // diff_opt_ignore_regex, exit 129).
    diff_opts.ignore_regexes = crate::compile_ignore_matching_regexes(&ignore_regex_patterns)?;
    if diff_opts.any() || diff_opts.merges_imply_patch {
        diff_opts.context = Some(sley_rev::diff_options::resolve_diff_context(
            diff_opts.context,
            Some(&config),
        )?);
    }
    // Resolve and validate pickaxe (`-S`/`-G`/`--find-object`). git OR-s the
    // kind bits and rejects any combination of the three kinds; `-G` cannot be
    // combined with `--pickaxe-regex`; `--pickaxe-all` cannot be combined with
    // `--find-object`.
    let has_find_object = !find_object_patterns.is_empty();
    {
        let kind_count = (saw_s as u8) + (saw_g as u8) + (has_find_object as u8);
        if kind_count > 1 {
            return Err(log_pickaxe_kinds_conflict_error());
        }
        if saw_g && pickaxe_regex {
            return Err(log_pickaxe_g_regex_conflict_error());
        }
        if pickaxe_all && has_find_object {
            return Err(log_pickaxe_all_objfind_conflict_error());
        }
    }
    let compiled_pickaxe = if has_find_object {
        let mut oids = HashSet::new();
        for pat in &find_object_patterns {
            let oid = repository.resolve_revision(pat).map_err(|_| {
                eprintln!("error: unable to resolve '{pat}'");
                GitError::Exit(128)
            })?;
            oids.insert(oid);
        }
        Some(CompiledPickaxe::FindObject { oids })
    } else if let Some(spec) = &pickaxe {
        match spec {
            PickaxeSpec::Grep(pattern) => Some(CompiledPickaxe::Grep {
                regex: compile_pickaxe_regex(pattern, regexp_ignore_case)?,
            }),
            PickaxeSpec::String(needle) if pickaxe_regex => Some(CompiledPickaxe::StringRegex {
                regex: compile_pickaxe_regex(needle, regexp_ignore_case)?,
            }),
            PickaxeSpec::String(needle) => Some(CompiledPickaxe::StringLiteral {
                needle: if regexp_ignore_case {
                    needle.to_ascii_lowercase().into_bytes()
                } else {
                    needle.clone().into_bytes()
                },
            }),
            PickaxeSpec::FindObject(_) => unreachable!("find-object handled above"),
        }
    } else {
        None
    };
    let pickaxe_ignore_case = regexp_ignore_case;
    // Rename/copy detection for the commit-selection filters (pickaxe,
    // diff-filter): a command-line `-M`/`-C`/`--no-renames` wins, else
    // `diff.renames` config (git's default is rename-on, copy-off).
    let config_detect_renames = !matches!(
        config
            .get("diff", None, "renames")
            .map(|value| value.trim().to_ascii_lowercase())
            .as_deref(),
        Some("false") | Some("no") | Some("off") | Some("0")
    );
    let config_detect_copies = matches!(
        config
            .get("diff", None, "renames")
            .map(|value| value.trim().to_ascii_lowercase())
            .as_deref(),
        Some("copies") | Some("copy")
    );
    let filter_detect_renames = renames_override.unwrap_or(config_detect_renames);
    let filter_detect_copies = copies_override.unwrap_or(config_detect_copies);
    let pickaxe_detect_renames = filter_detect_renames;
    let pickaxe_text = diff_opts.text;
    // Resolve the `--diff-filter` mask now that the full option scan is done.
    let diff_filter_mask = if diff_filter_given {
        Some(resolve_diff_filter_mask(
            diff_filter_bits,
            diff_filter_not_bits,
        ))
    } else {
        None
    };
    // Userdiff/attribute resolution is only needed for patch rendering and
    // pickaxe filters. Plain commit-log output should not pay for it.
    let log_userdiff =
        if diff_opts.any() || diff_opts.merges_imply_patch || compiled_pickaxe.is_some() {
            let attributes = worktree_root
                .as_deref()
                .map(sley_worktree::StandardAttributeMatcher::from_worktree_root)
                .transpose()?;
            Some(commands::userdiff::UserdiffResolver::with_attributes(
                attributes,
                Some(config.clone()),
            ))
        } else {
            None
        };
    // Per-commit diff rendering context (only consulted when a diff-output
    // option was given).
    let log_diff = if diff_opts.any() || diff_opts.merges_imply_patch {
        let show_root = show_root_flag
            .unwrap_or_else(|| config.get_bool("log", None, "showroot").unwrap_or(true));
        // diff.renames: false disables detection, "copies"/"copy" adds copy
        // detection, anything else (or unset) means rename detection.
        // A command-line `-M`/`-C`/`--no-renames` overrides `diff.renames`.
        let (detect_renames, detect_copies) = (filter_detect_renames, filter_detect_copies);
        let diff_pathspec = if pathspecs.is_empty() {
            None
        } else {
            let worktree_root = repository.worktree_root()?;
            Some(DiffPathspec::new(
                &cwd,
                worktree_root,
                &pathspecs,
                effective_pathspec_flags(cli_session),
            )?)
        };
        let repo_abbrev = repository_abbrev_from_config(&git_dir, format, &config)?;
        Some(LogDiffContext {
            db: &db,
            lazy_fetch,
            format,
            config: &config,
            userdiff: log_userdiff
                .as_ref()
                .expect("log diff context requires userdiff resolver"),
            opts: &diff_opts,
            merges: diff_opts.merges.unwrap_or(if first_parent {
                LogDiffMerges::FirstParent
            } else {
                LogDiffMerges::Off
            }),
            show_root,
            detect_renames,
            detect_copies,
            pathspec: diff_pathspec,
            patch_abbrev: repo_abbrev.unwrap_or(7).min(format.hex_len()),
            raw_abbrev: repo_abbrev,
            pickaxe: compiled_pickaxe.as_ref(),
            pickaxe_ignore_case,
            pickaxe_text,
            pickaxe_all,
        })
    } else {
        None
    };
    // `log.decorate` config sets the default decoration mode when no
    // `--decorate*` flag was given. Resolve it before compiling presets such as
    // `--oneline`, which bake `%d` into the format when decoration is active.
    if !decoration_explicit {
        let decorate_config = config
            .get("log", None, "decorate")
            .map(str::to_string)
            .or_else(|| {
                matches!(config.get_all("log", None, "decorate").last(), Some(None))
                    .then(|| "true".to_string())
            });
        let decorate_config = match decorate_config {
            Some(value) => Some(value),
            None => log_effective_file_config_value(&git_dir, &cwd, "log", "decorate")?,
        };
        if let Some(value) = decorate_config {
            match value.trim().to_ascii_lowercase().as_str() {
                "short" | "true" | "yes" | "on" | "1" | "" => decoration = LogDecorationMode::Short,
                "full" => decoration = LogDecorationMode::Full,
                "no" | "false" | "off" | "0" | "auto" => decoration = LogDecorationMode::Off,
                _ => decoration = LogDecorationMode::Short,
            }
        }
    }
    // Resolve the captured `--pretty=`/`--format=` spec now that config (and its
    // `pretty.<name>` aliases) is available.
    if let Some((spec, format_kind)) = pretty_spec.take() {
        plain_oneline = false;
        match resolve_pretty_spec(&spec, format_kind, &config)? {
            ResolvedPretty::Oneline => preset_oneline = Some(true),
            ResolvedPretty::Default => output = LogOutput::Default(LogDefaultKind::Medium),
            ResolvedPretty::Short => output = LogOutput::Default(LogDefaultKind::Short),
            ResolvedPretty::Full => output = LogOutput::Default(LogDefaultKind::Full),
            ResolvedPretty::Fuller => output = LogOutput::Default(LogDefaultKind::Fuller),
            ResolvedPretty::Raw => output = LogOutput::Default(LogDefaultKind::Raw),
            ResolvedPretty::Reference => {
                // reference defaults the date to short; an explicit --date wins.
                if !date_explicit {
                    date_mode = DateMode::Short;
                }
                output = LogOutput::Compiled {
                    compiled: CompiledLogFormat::compile(
                        "%C(auto)%h (%s, %ad)",
                        LogFormatDialect::Log,
                    )?,
                    final_newline: true,
                    suppress_extra_final_newline: false,
                    show_children: false,
                    inline_children: false,
                };
            }
            ResolvedPretty::Compiled {
                compiled,
                final_newline,
                suppress_extra_final_newline,
            } => {
                output = LogOutput::Compiled {
                    compiled,
                    final_newline,
                    suppress_extra_final_newline,
                    show_children: false,
                    inline_children: false,
                };
            }
        }
    }
    if let Some(pretty_oneline) = preset_oneline {
        if matches!(output, LogOutput::Default(_)) {
            let use_full_oid = match pretty_oneline {
                true => !abbrev_commit,
                false => abbrev_len.is_none(),
            };
            let compiled = if walk_reflogs {
                let oid = if use_full_oid { "%H" } else { "%h" };
                CompiledLogFormat::compile(&format!("{oid} %gD: %gs"), LogFormatDialect::Log)?
            } else if show_source {
                let oid = if use_full_oid { "%H" } else { "%h" };
                let parents = if show_parents { " %P" } else { "" };
                let decorations = if decoration != LogDecorationMode::Off {
                    "%d"
                } else {
                    ""
                };
                CompiledLogFormat::compile(
                    &format!("{oid}{parents}%x09%S{decorations} %s"),
                    LogFormatDialect::Log,
                )?
            } else {
                presets::log_oneline(
                    decoration != LogDecorationMode::Off,
                    use_full_oid,
                    show_parents,
                )?
            };
            output = LogOutput::Compiled {
                compiled,
                final_newline: true,
                suppress_extra_final_newline: false,
                show_children,
                inline_children: !show_source,
            };
        }
    } else if let LogOutput::Compiled {
        show_children: compiled_children,
        ..
    } = &mut output
    {
        *compiled_children = show_children;
    }
    if !abbrev_len_explicit && log_output_needs_abbrev(&output, abbrev_commit, show_children) {
        abbrev_len = repository_abbrev_from_config(&git_dir, format, &config)?;
    }
    // When no CLI pattern-type flag was given, `grep.patternType` config
    // supplies the default (git's `grep_config`). `default` means "fall back to
    // the basic/extended toggle", which for log is BRE.
    if !pattern_kind_explicit && let Some(value) = config.get("grep", None, "patterntype") {
        pattern_kind = match value.trim().to_ascii_lowercase().as_str() {
            "fixed" => sley_grep::PatternKind::Fixed,
            "basic" => sley_grep::PatternKind::Basic,
            "extended" => sley_grep::PatternKind::Extended,
            "perl" => sley_grep::PatternKind::Perl,
            _ => pattern_kind,
        };
    }
    let author_filters =
        compile_log_filter_matcher(&author_patterns, pattern_kind, regexp_ignore_case, "header")?;
    let committer_filters = compile_log_filter_matcher(
        &committer_patterns,
        pattern_kind,
        regexp_ignore_case,
        "header",
    )?;
    let grep_filters = compile_log_filter_matcher(
        &grep_patterns,
        pattern_kind,
        regexp_ignore_case,
        "command line",
    )?;
    let grep_colors = LogGrepColors::from_config(&config, color_always);
    if walk_reflogs {
        let reflog_revisions = revision_options
            .positives
            .iter()
            .filter_map(|tip| {
                tip.source_name
                    .clone()
                    .map(|source| (source, tip.from_ref_selector))
            })
            .collect::<Vec<_>>();
        return log_walk_reflogs(
            &git_dir,
            format,
            &reflog_revisions,
            ReflogWalkOptions {
                max_count,
                skip,
                output: &output,
                reverse,
                date_mode: &date_mode,
                replace_objects: cli_session.replace_objects(),
            },
        );
    }
    let log_format_source =
        if !revision_options.had_ref_selector && revision_options.positives.len() == 1 {
            revision_options.positives[0].source_name.clone()
        } else {
            None
        };
    let mut starts = Vec::new();
    // `(start_commit_oid, source_label)` pairs in command-line order, used to
    // build the `%S` per-commit source map (later starts override earlier ones).
    let mut source_starts: Vec<(ObjectId, String)> = Vec::new();
    for tip in &revision_options.positives {
        let commit = match sley_rev::peel_to_commit(&db, format, &tip.oid) {
            Ok(commit) => commit,
            Err(err) if tip.from_ref_selector => {
                let Ok(object) = db.read_object(&tip.oid) else {
                    return Err(err);
                };
                if matches!(object.object_type, ObjectType::Blob | ObjectType::Tree) {
                    continue;
                }
                return Err(err);
            }
            Err(err) => return Err(err),
        };
        if let Some(source_name) = &tip.source_name {
            source_starts.push((commit, source_name.clone()));
        }
        starts.push(commit);
    }
    // git's `--exclude-first-parent-only`: the negative (`^`-excluded) tips'
    // history is marked UNINTERESTING following only first parents, even when the
    // positive walk follows all parents. So a merge brought in by an excluded
    // tip's side branch stays interesting.
    let exclude_first_parent = first_parent || revision_options.exclude_first_parent_only;
    let mut excluded = HashSet::new();
    for oid in &revision_options.negatives {
        for record in rev_list_walk_commits(&db, format, [*oid], exclude_first_parent)? {
            excluded.insert(record.oid);
        }
    }
    // `log -L`: the line-log engine owns its own walk + restricted-patch output.
    if !line_log_args.is_empty() {
        // `-L` cannot be combined with a pathspec or `--follow` (cell: basic
        // command line parsing). git checks pathspec first.
        if !pathspecs.is_empty() {
            eprintln!("fatal: -L<range>:<file> cannot be used with pathspec");
            return Err(GitError::Exit(128));
        }
        if saw_follow {
            eprintln!("fatal: --follow cannot be used with -L");
            return Err(GitError::Exit(128));
        }
        if starts.len() != 1 {
            eprintln!("fatal: only one rev expected with -L");
            return Err(GitError::Exit(128));
        }
        if diff_opts.stat
            || diff_opts.numstat
            || diff_opts.shortstat
            || diff_opts.compact_summary
            || line_log_dirstat_requested
            || line_log_full_diff_requested
        {
            eprintln!("fatal: -L does not yet support the requested diff format");
            return Err(GitError::Exit(128));
        }
        return run_line_log_output(LineLogOutputCtx {
            git_dir: &git_dir,
            db: &db,
            lazy_fetch,
            replace_objects: cli_session.replace_objects(),
            format,
            config: &config,
            tip: starts[0],
            args: &line_log_args,
            output: &output,
            diff_opts: &diff_opts,
            date_mode: &date_mode,
            abbrev_len,
            abbrev_commit,
            detect_renames: filter_detect_renames,
            first_parent,
            max_count,
            reverse,
            show_parents,
            decoration,
            output_encoding: &output_encoding,
            src_prefix: line_log_src_prefix.as_deref(),
            dst_prefix: line_log_dst_prefix.as_deref(),
            full_index: line_log_full_index,
            abbrev_len_explicit,
            max_age,
            min_age,
            pickaxe: compiled_pickaxe.as_ref(),
            pickaxe_ignore_case,
            pickaxe_text,
            pickaxe_detect_renames,
            diff_filter_mask,
            reverse_diff: diff_reverse,
            graph,
            color_always,
            color_moved: line_log_color_moved,
            userdiff: log_userdiff.as_ref(),
            output_path: log_output_path.as_deref(),
        });
    }
    if let Some(path) = log_output_path {
        return Err(GitError::Command(format!(
            "unsupported log option --output={path}"
        )));
    }
    if line_log_dirstat_requested {
        return Err(GitError::Command("unsupported log option --dirstat".into()));
    }
    if plain_oneline
        && walk
        && !graph
        && line_prefix.is_none()
        && ordering == RevListOrdering::Default
        && pathspecs.is_empty()
        && !full_history
        && matches!(
            &output,
            LogOutput::Compiled {
                compiled,
                final_newline: true,
                suppress_extra_final_newline: false,
                show_children: false,
                inline_children: true
            }
            if log_plain_oneline_format(compiled))
        && decoration == LogDecorationMode::Off
        && !show_parents
        && !show_children
        && excluded.is_empty()
        && starts.len() == 1
        && !first_parent
        && !reverse
        && skip == 0
        && author_filters.is_none()
        && committer_filters.is_none()
        && grep_filters.is_none()
        && compiled_pickaxe.is_none()
        && diff_filter_mask.is_none()
        && max_age.is_none()
        && min_age.is_none()
        && min_parents.is_none()
        && max_parents.is_none()
        && !null_terminate
        && !abbrev_len_explicit
        && !has_commit_grafts
        && let Some(max_count) = max_count
        && max_count > 0
    {
        let stdout = io::stdout();
        let mut stdout = io::BufWriter::new(stdout.lock());
        let mut line = Vec::with_capacity(128);
        let output_encoding_is_utf8 = encoding_is_utf8(&output_encoding);
        let mut walk = sley_rev::RevWalk::new(&git_dir, format, &db, starts)
            .order(sley_rev::RevWalkOrder::CommitDate)
            .max_count(Some(max_count));
        while let Some(metadata) = walk.try_next()? {
            line.clear();
            emit_plain_oneline_limited_commit(
                &db,
                &metadata,
                abbrev_len,
                &output_encoding,
                output_encoding_is_utf8,
                &mut line,
            )?;
            stdout.write_all(&line)?;
            stdout.write_all(b"\n")?;
        }
        stdout.flush()?;
        return Ok(());
    }
    if walk
        && !graph
        && line_prefix.is_none()
        && ordering == RevListOrdering::Default
        && pathspecs.is_empty()
        && !full_history
        && matches!(&output, LogOutput::Compiled { compiled, show_children: false, .. }
            if compiled.is_metadata_emitable()
                && compiled.uses_oid()
                && !compiled.uses_decorations()
                && !compiled_format_uses_notes(compiled))
        && decoration == LogDecorationMode::Off
        && !show_children
        && excluded.is_empty()
        && starts.len() == 1
        && !revision_options.had_ref_selector
        && author_filters.is_none()
        && committer_filters.is_none()
        && grep_filters.is_none()
        && compiled_pickaxe.is_none()
        && diff_filter_mask.is_none()
        && max_age.is_none()
        && min_age.is_none()
        && min_parents.is_none()
        && max_parents.is_none()
        && !has_commit_grafts
    {
        let limit = max_count.map(|max| skip.saturating_add(max));
        let metadata = if let Some(limit) = limit.filter(|limit| *limit > 0) {
            sley_rev::walk_commit_metadata_date_ordered_limited(
                &git_dir,
                format,
                &db,
                starts.clone(),
                first_parent,
                limit,
            )?
        } else {
            sley_rev::walk_commit_metadata(&git_dir, format, &db, starts.clone(), first_parent)?
        };
        let mut selected = metadata
            .into_iter()
            .filter(|record| !excluded.contains(&record.oid))
            .collect::<Vec<_>>();
        if limit.is_none() {
            selected = rev_list_metadata_date_order(selected);
        }
        if skip > 0 {
            selected = selected.into_iter().skip(skip).collect();
        }
        if let Some(max_count) = max_count {
            selected.truncate(max_count);
        }
        if reverse {
            selected.reverse();
        }
        let (compiled, final_newline, suppress_extra_final_newline) = match &output {
            LogOutput::Compiled {
                compiled,
                final_newline,
                suppress_extra_final_newline,
                ..
            } => (compiled, *final_newline, *suppress_extra_final_newline),
            _ => unreachable!("metadata fast path requires compiled output"),
        };
        let stdout = io::stdout();
        let mut stdout = io::BufWriter::new(stdout.lock());
        let term: &[u8] = if null_terminate { b"\0" } else { b"\n" };
        let mut line = Vec::with_capacity(compiled.estimated_line_capacity());
        for (index, record) in selected.iter().enumerate() {
            // `--pretty=format:` separates entries with a newline (none trailing);
            // `--format=`/`tformat:`/oneline terminate each entry with one.
            if index > 0 && !final_newline {
                stdout.write_all(term)?;
            }
            line.clear();
            emit_compiled_log_format_metadata(
                record,
                compiled,
                &LogFormatContext {
                    abbrev_len,
                    decorations: &HashMap::new(),
                    marker: '>',
                    dialect: LogFormatDialect::Log,
                    source: log_format_source.as_deref(),
                    date_mode: &date_mode,
                    source_oid: None,
                    describe: None,
                    signature: None,
                    color: color_always,
                    output_encoding: &output_encoding,
                    mailmap: &CliMailmapAdapter(&empty_mailmap),
                    use_mailmap,
                },
                &mut line,
            )?;
            stdout.write_all(&line)?;
            if final_newline
                && (!suppress_extra_final_newline
                    || null_terminate
                    || index + 1 < selected.len()
                    || line.last() != Some(&term[0]))
            {
                stdout.write_all(term)?;
            }
        }
        stdout.flush()?;
        return Ok(());
    }
    if walk
        && !graph
        && line_prefix.is_none()
        && ordering == RevListOrdering::Default
        && pathspecs.is_empty()
        && !full_history
        && matches!(&output, LogOutput::Compiled { compiled, show_children: false, .. }
            if log_limited_commit_format_supported(compiled) && !compiled_format_uses_notes(compiled))
        && decoration == LogDecorationMode::Off
        && !show_children
        && excluded.is_empty()
        && starts.len() == 1
        && !revision_options.had_ref_selector
        && author_filters.is_none()
        && committer_filters.is_none()
        && grep_filters.is_none()
        && compiled_pickaxe.is_none()
        && diff_filter_mask.is_none()
        && max_age.is_none()
        && min_age.is_none()
        && min_parents.is_none()
        && max_parents.is_none()
        && !has_commit_grafts
        && let Some(limit) = max_count.map(|max| skip.saturating_add(max))
        && limit > 0
    {
        let (compiled, final_newline, suppress_extra_final_newline) = match &output {
            LogOutput::Compiled {
                compiled,
                final_newline,
                suppress_extra_final_newline,
                ..
            } => (compiled, *final_newline, *suppress_extra_final_newline),
            _ => unreachable!("limited commit fast path requires compiled output"),
        };
        let mut stdout = io::stdout();
        let term: &[u8] = if null_terminate { b"\0" } else { b"\n" };
        let context = LogFormatContext {
            abbrev_len,
            decorations: &HashMap::new(),
            marker: '>',
            dialect: LogFormatDialect::Log,
            source: log_format_source.as_deref(),
            date_mode: &date_mode,
            source_oid: None,
            describe: None,
            signature: None,
            color: color_always,
            output_encoding: &output_encoding,
            mailmap: &CliMailmapAdapter(&empty_mailmap),
            use_mailmap,
        };
        let metadata = sley_rev::walk_commit_metadata_date_ordered_limited(
            &git_dir,
            format,
            &db,
            starts.clone(),
            first_parent,
            limit,
        )?;
        let mut selected = metadata.into_iter().collect::<Vec<_>>();
        if skip > 0 {
            selected = selected.into_iter().skip(skip).collect();
        }
        selected.truncate(max_count.expect("limited log path requires max-count"));
        if reverse {
            selected.reverse();
        }
        let mut line = Vec::with_capacity(compiled.estimated_line_capacity());
        for (index, metadata) in selected.iter().enumerate() {
            if index > 0 && !final_newline {
                stdout.write_all(term)?;
            }
            line.clear();
            emit_compiled_log_format_limited_commit(&db, metadata, compiled, &context, &mut line)?;
            let out = log_reencode_message(&line, "UTF-8", context.output_encoding);
            stdout.write_all(&out)?;
            if final_newline
                && (!suppress_extra_final_newline
                    || null_terminate
                    || index + 1 < selected.len()
                    || out.last() != Some(&term[0]))
            {
                stdout.write_all(term)?;
            }
        }
        stdout.flush()?;
        return Ok(());
    }
    let commits = if walk {
        rev_list_walk_commits(&db, format, starts, first_parent)?
    } else {
        rev_list_no_walk_commits(&db, format, starts)?
    };
    let mut child_oids = HashMap::<ObjectId, Vec<ObjectId>>::new();
    if show_children {
        for record in &commits {
            for parent in &record.parents {
                child_oids.entry(*parent).or_default().push(record.oid);
            }
        }
        for children in child_oids.values_mut() {
            children.reverse();
        }
    }
    let mut selected = Vec::new();
    let filter_mailmap = if use_mailmap && (author_filters.is_some() || committer_filters.is_some())
    {
        Some(log_cached_mailmap(
            &mut mailmap_cache,
            &git_dir,
            format,
            cli_session.replace_objects(),
        )?)
    } else {
        None
    };
    for record in &commits {
        if excluded.contains(&record.oid)
            || min_parents.is_some_and(|min| record.parents.len() < min)
            || max_parents.is_some_and(|max| record.parents.len() > max)
            || !log_age_filters_match(record, max_age, min_age)?
            || !log_author_matcher_matches(record, author_filters.as_ref(), filter_mailmap)
            || !log_committer_matcher_matches(record, committer_filters.as_ref(), filter_mailmap)
            || !log_grep_matcher_matches(
                record,
                grep_filters.as_ref(),
                grep_all_match,
                invert_grep,
                &output_encoding,
            )
        {
            continue;
        }
        selected.push(record);
    }
    // Pickaxe (`-S`/`-G`/`--find-object`): keep only commits whose first-parent
    // diff contains a matching filepair. Applied after the cheap header filters
    // so we read blobs for as few commits as possible.
    if let Some(pickaxe) = &compiled_pickaxe {
        let pickaxe_pathspec = if pathspecs.is_empty() {
            None
        } else {
            let worktree_root = repository.worktree_root()?;
            Some(DiffPathspec::new(
                &cwd,
                worktree_root,
                &pathspecs,
                effective_pathspec_flags(cli_session),
            )?)
        };
        let mut kept = Vec::with_capacity(selected.len());
        for record in selected {
            if pickaxe_commit_matches(
                &db,
                format,
                record,
                pickaxe,
                pickaxe_ignore_case,
                pickaxe_text,
                pickaxe_detect_renames,
                pickaxe_pathspec.as_ref(),
                log_userdiff.as_ref(),
            )? {
                kept.push(record);
            }
        }
        selected = kept;
    }
    // `--diff-filter`: keep only commits whose first-parent diff has a filepair
    // whose status is in the requested mask.
    if let Some(mask) = diff_filter_mask {
        let filter_pathspec = if pathspecs.is_empty() {
            None
        } else {
            let worktree_root = repository.worktree_root()?;
            Some(DiffPathspec::new(
                &cwd,
                worktree_root,
                &pathspecs,
                effective_pathspec_flags(cli_session),
            )?)
        };
        let mut kept = Vec::with_capacity(selected.len());
        for record in selected {
            let filter_opts = DiffFilterMatchOptions {
                mask,
                detect_renames: filter_detect_renames,
                detect_copies: filter_detect_copies,
                find_copies_harder,
                pathspec: filter_pathspec.as_ref(),
            };
            if diff_filter_commit_matches(&db, format, record, filter_opts)? {
                kept.push(record);
            }
        }
        selected = kept;
    }
    selected = match ordering {
        // `--graph` implies topological ordering (upstream sets
        // `revs->topo_order = 1`); `--date-order`/`--author-date-order` pick
        // the date-keyed topo variants, which the helpers below already are.
        RevListOrdering::Default if graph => rev_list_topo_order(selected)?,
        RevListOrdering::Default if walk => rev_list_date_order(selected)?,
        RevListOrdering::Default if !no_walk_unsorted => {
            // `--no-walk[=sorted]`: a plain stable commit-time sort (upstream
            // `commit_list_sort_by_date`), newest first. A missing/unparsable
            // committer date sorts as the epoch (git's `commit->date == 0`), so
            // never abort the walk over it.
            let mut keyed = selected
                .iter()
                .map(|record| {
                    let timestamp =
                        commit_identity_timestamp_i64(&record.commit.committer).unwrap_or(0);
                    (timestamp, *record)
                })
                .collect::<Vec<_>>();
            keyed.sort_by_key(|(timestamp, _)| std::cmp::Reverse(*timestamp));
            keyed.into_iter().map(|(_, record)| record).collect()
        }
        RevListOrdering::Default => selected,
        RevListOrdering::Topo => rev_list_topo_order(selected)?,
        RevListOrdering::Date => rev_list_date_order(selected)?,
        RevListOrdering::AuthorDate => rev_list_author_date_order(selected)?,
    };
    // `--ancestry-path`: keep only commits on a path from a `^`-excluded boundary
    // (bottom) commit up to the tips (git's `limit_to_ancestry`). Runs before
    // simplification.
    if revision_options.ancestry_path && !revision_options.negatives.is_empty() {
        let on_path = sley_rev::ancestry_path_on_set(
            selected.iter().map(|r| (r.oid, r.parents.clone())),
            &revision_options.negatives,
        );
        selected.retain(|r| on_path.contains(&r.oid));
    }
    if saw_follow && !pathspecs.is_empty() {
        let pathspec = normalized_revwalk_pathspec(
            &cwd,
            worktree_root.as_deref(),
            &pathspecs,
            effective_pathspec_flags(cli_session),
        )?;
        let ordered_owned: Vec<sley_rev::CommitRecord> = commits.clone();
        let bottoms: HashSet<ObjectId> = revision_options.negatives.iter().copied().collect();
        let _ = sley_rev::simplify_history_with_bottoms(
            &db,
            format,
            ordered_owned,
            &pathspec,
            sley_rev::SimplifyOptions {
                full_history,
                first_parent,
                simplify_merges: revision_options.simplify_merges,
                show_pulls: revision_options.show_pulls,
                ancestry_path: revision_options.ancestry_path,
                want_ancestry: show_parents
                    || show_children
                    || graph
                    || revision_options.simplify_merges,
            },
            &bottoms,
        )?;
    }
    let follow_applied =
        saw_follow && pathspecs.len() == 1 && !full_history && !revision_options.simplify_merges;
    if follow_applied {
        selected = log_follow_single_path(&db, format, selected, pathspecs[0].as_bytes(), true)?;
    }
    // Pathspec-limited / --full-history simplification (TREESAME prune + parent
    // rewriting). Owned binding outlives `selected` (a Vec of references).
    let simplified_storage;
    if (!pathspecs.is_empty() && !follow_applied)
        || full_history
        || revision_options.simplify_merges
    {
        let pathspec = normalized_revwalk_pathspec(
            &cwd,
            worktree_root.as_deref(),
            &pathspecs,
            effective_pathspec_flags(cli_session),
        )?;
        let ordered_owned: Vec<sley_rev::CommitRecord> =
            selected.iter().map(|r| (*r).clone()).collect();
        // The `^`-excluded boundary tips are git's BOTTOM commits: relevant for
        // topology-keep decisions even though they aren't shown.
        let bottoms: HashSet<ObjectId> = revision_options.negatives.iter().copied().collect();
        simplified_storage = sley_rev::simplify_history_with_bottoms(
            &db,
            format,
            ordered_owned,
            &pathspec,
            sley_rev::SimplifyOptions {
                full_history,
                first_parent,
                simplify_merges: revision_options.simplify_merges,
                show_pulls: revision_options.show_pulls,
                ancestry_path: revision_options.ancestry_path,
                // git's `want_ancestry` = `rewrite_parents || children`.
                // `--ancestry-path` alone does NOT set rewrite_parents, so a bare
                // `--ancestry-path` still drops TREESAME merges.
                want_ancestry: show_parents
                    || show_children
                    || graph
                    || revision_options.simplify_merges,
            },
            &bottoms,
        )?;
        selected = simplified_storage.iter().collect();
    }
    // Build the decoration ref filter: `--decorate-refs` (include-only globs),
    // `--decorate-refs-exclude`, and `log.excludeDecoration` config (a missing
    // value is reported but non-fatal).
    let mut exclude_config: Vec<String> = Vec::new();
    for entry in config.get_all("log", None, "excludedecoration") {
        match entry {
            Some(pattern) => exclude_config.push(pattern.to_string()),
            None => {
                eprintln!("error: missing value for 'log.excludeDecoration'");
                // git still produces output (exit 0) but with no excludes.
            }
        }
    }
    // git's set_default_decoration_filter: when no `--decorate-refs*`,
    // `--clear-decorations`, or `log.excludeDecoration` was given, restrict
    // decorations to the standard decorating namespaces (so refs/prefetch,
    // refs/rebase-merge, refs/bundle, &c. are not shown). `--clear-decorations`
    // disables this default so all refs decorate.
    let initial_decoration_set_all = config
        .get("log", None, "initialdecorationset")
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("all"));
    let mut include = decorate_refs_include.clone();
    if !clear_decorations
        && !initial_decoration_set_all
        && include.is_empty()
        && decorate_refs_exclude.is_empty()
        && exclude_config.is_empty()
    {
        include.extend(
            [
                "HEAD",
                "refs/heads/",
                "refs/tags/",
                "refs/remotes/",
                "refs/stash",
                "refs/replace/",
            ]
            .map(str::to_string),
        );
    }
    let decoration_filter =
        DecorationFilter::new(&include, &decorate_refs_exclude, &exclude_config);
    let custom_decoration_mode = match &output {
        LogOutput::Compiled { compiled, .. } if compiled.uses_decorations() => {
            Some(if decoration == LogDecorationMode::Full {
                LogDecorationMode::Full
            } else {
                LogDecorationMode::Short
            })
        }
        _ => None,
    };
    let display_decorations =
        decoration != LogDecorationMode::Off || custom_decoration_mode.is_some();
    let mut decorations = if !display_decorations && !simplify_by_decoration {
        HashMap::new()
    } else {
        let map_mode = custom_decoration_mode.unwrap_or(if decoration == LogDecorationMode::Off {
            LogDecorationMode::Short
        } else {
            decoration
        });
        log_decoration_map(&git_dir, &db, format, map_mode, &decoration_filter)?
    };
    if simplify_by_decoration {
        selected
            .retain(|record| decorations.contains_key(&record.oid) || record.parents.is_empty());
    }
    // `--boundary` (with `--graph`): the uninteresting commits directly adjacent
    // to the shown set are git's BOUNDARY commits. Emit them as leaf nodes (`o`)
    // so a merge whose excluded parent sits on the range boundary still renders
    // its fork. Owned in `boundary_storage` so `selected` (a Vec of refs) can
    // borrow them.
    let mut boundary_oids: HashSet<ObjectId> = HashSet::new();
    let boundary_storage: Vec<sley_rev::CommitRecord>;
    if boundary && graph {
        let shown_set: HashSet<ObjectId> = selected.iter().map(|record| record.oid).collect();
        let mut seen = HashSet::new();
        let mut records: Vec<sley_rev::CommitRecord> = Vec::new();
        for record in &selected {
            for parent in &record.parents {
                if !shown_set.contains(parent) && seen.insert(*parent) {
                    if let Ok(rec) =
                        sley_rev::revlist::read_rev_list_commit_record(&db, format, *parent)
                    {
                        boundary_oids.insert(*parent);
                        records.push(rec);
                    }
                }
            }
        }
        // Boundary commits are ancestors of the shown set; emit them after it,
        // newest-first among themselves (matching the topo-ordered output).
        records.sort_by_key(|record| {
            std::cmp::Reverse(commit_identity_timestamp_i64(&record.commit.committer).unwrap_or(0))
        });
        boundary_storage = records;
        selected.extend(boundary_storage.iter());
    } else {
        boundary_storage = Vec::new();
    }
    let _ = &boundary_storage;
    // For `--graph`, a parent is "interesting" iff it will be shown — judged
    // against the full selection BEFORE `--skip`/`-n` truncation (matching
    // upstream `get_commit_action`, which is truncation-blind).
    let graph_shown: Option<HashSet<ObjectId>> =
        graph.then(|| selected.iter().map(|record| record.oid).collect());
    if skip > 0 {
        selected = selected.into_iter().skip(skip).collect();
    }
    if let Some(max_count) = max_count {
        selected.truncate(max_count);
    }
    if reverse {
        selected.reverse();
    }
    if !display_decorations {
        decorations.clear();
    }
    // Object access for `%(describe)`.
    let describe_ctx = CliLogDescribeContext {
        git_dir: &git_dir,
        db: &db,
        format,
    };
    let source_tag_signatures = if matches!(
        &output,
        LogOutput::Compiled { compiled, .. } if compiled.uses_signature()
    ) {
        source_tag_signatures_for_revision_tips(
            &git_dir,
            &db,
            format,
            &config,
            &revision_options.positives,
        )?
    } else {
        HashMap::new()
    };
    let signature_ctx = CliLogSignatureContext {
        git_dir: &git_dir,
        db: &db,
        config: &config,
        source_tag_signatures: &source_tag_signatures,
    };
    // `%S` source labels: each commit is tagged with the start ref from which it
    // is reachable; when several starts reach it, the last one (command-line
    // order) wins — matching git's `revision.c` source naming.
    let format_uses_source =
        matches!(&output, LogOutput::Compiled { compiled, .. } if compiled.uses_source());
    let source_labels: Option<HashMap<ObjectId, String>> =
        if (format_uses_source || show_source) && !source_starts.is_empty() {
            Some(log_source_labels_for_selected(
                &selected,
                &source_starts,
                first_parent,
            ))
        } else {
            None
        };
    // Resolve the notes-display refs once, but only for output modes that can
    // display notes. The empty list short-circuits all per-commit note lookups.
    let pretty_format_uses_notes = matches!(&output, LogOutput::Compiled { compiled, .. } if compiled_format_uses_notes(compiled));
    let notes_default_format =
        matches!(output, LogOutput::Default(LogDefaultKind::Medium)) || pretty_format_uses_notes;
    let display_notes = if notes_display.is_active(notes_default_format) {
        let notes_store = FileRefStore::new(&git_dir, format);
        let refs = notes_display.resolve_refs(&git_dir, &notes_store)?;
        Some((notes_store, refs))
    } else {
        None
    };
    let output_mailmap = if log_output_needs_mailmap(&output, use_mailmap) {
        log_cached_mailmap(
            &mut mailmap_cache,
            &git_dir,
            format,
            cli_session.replace_objects(),
        )?
    } else {
        &empty_mailmap
    };
    // Resolve `--expand-tabs` (upstream revision.c): an explicit CLI value wins,
    // otherwise fall back to the per-format default (medium/full/fuller expand
    // to 8; everything else, including compiled/oneline formats, defaults off).
    let expand_tabs_in_log: i32 = expand_tabs_explicit.unwrap_or_else(|| match &output {
        LogOutput::Default(kind) => log_default_expand_tabs(*kind),
        LogOutput::Compiled { .. } => 0,
    });

    if let Some(shown) = &graph_shown {
        let palette = log_graph_color_palette(&config);
        let mut graph_state = sley_rev::graph::Graph::new(palette, color_always);
        let prefix: &str = line_prefix.as_deref().unwrap_or("");
        let mut out = io::stdout();
        // Whether the previous entry's message ended without a newline
        // (upstream `opt->missing_newline`), for the separator decision.
        let mut prev_missing_newline = false;
        let mut diff_block = Vec::new();
        for (index, record) in selected.iter().enumerate() {
            let is_boundary = boundary_oids.contains(&record.oid);
            let mut interesting: Vec<ObjectId> = if is_boundary {
                // Boundary commits are leaves in the drawn graph.
                Vec::new()
            } else {
                record
                    .parents
                    .iter()
                    .filter(|parent| shown.contains(*parent))
                    .copied()
                    .collect()
            };
            if first_parent {
                interesting.truncate(1);
            }
            graph_state.update_boundary(record.oid, &interesting, is_boundary);
            match &output {
                LogOutput::Compiled {
                    compiled,
                    final_newline,
                    suppress_extra_final_newline,
                    ..
                } => {
                    if index > 0 && !*final_newline {
                        // `--pretty=format:` separator semantics.
                        if !prev_missing_newline {
                            graph_show_padding(&mut graph_state, prefix, &mut out)?;
                        }
                        out.write_all(b"\n")?;
                    }
                    graph_show_commit(&mut graph_state, prefix, &mut out)?;
                    let format_context = LogFormatContext {
                        abbrev_len,
                        decorations: &decorations,
                        marker: '>',
                        dialect: LogFormatDialect::Log,
                        source: log_format_source.as_deref(),
                        date_mode: &date_mode,
                        source_oid: source_labels.as_ref(),
                        describe: Some(&CliLogDescribeAdapter(&describe_ctx)),
                        signature: Some(&CliLogSignatureAdapter(&signature_ctx)),
                        color: color_always,
                        output_encoding: &output_encoding,
                        mailmap: &CliMailmapAdapter(output_mailmap),
                        use_mailmap,
                    };
                    let mut msg = Vec::with_capacity(compiled.estimated_line_capacity());
                    if let Some((notes_store, display_notes_refs)) = display_notes.as_ref() {
                        emit_encoded_compiled_log_format_with_notes(
                            &git_dir,
                            format,
                            notes_store,
                            display_notes_refs,
                            record,
                            compiled,
                            &format_context,
                            &mut msg,
                        )?;
                    } else {
                        emit_encoded_compiled_log_format_no_notes(
                            record,
                            compiled,
                            &format_context,
                            &mut msg,
                        )?;
                    }
                    if let Some(log_diff) = &log_diff {
                        let mut padding = String::new();
                        graph_state.padding_line(&mut padding);
                        let prefix_width =
                            log_prefix_display_width(&padding) + log_prefix_display_width(prefix);
                        log_diff.render(record, prefix_width, &mut diff_block)?;
                        if !diff_block.is_empty() {
                            if msg.last() != Some(&b'\n') {
                                msg.push(b'\n');
                            }
                            msg.extend_from_slice(diff_opts.block_separator_for(record));
                            msg.extend_from_slice(&diff_block);
                        }
                    }
                    graph_show_commit_msg(&mut graph_state, prefix, &msg, &mut out)?;
                    let newline_terminated = msg.last() == Some(&b'\n');
                    prev_missing_newline = !newline_terminated;
                    if *final_newline
                        && (!*suppress_extra_final_newline
                            || index + 1 < selected.len()
                            || !newline_terminated)
                    {
                        if newline_terminated {
                            graph_show_padding(&mut graph_state, prefix, &mut out)?;
                        }
                        out.write_all(b"\n")?;
                        prev_missing_newline = false;
                    }
                }
                LogOutput::Default(kind) => {
                    if *kind == LogDefaultKind::Raw {
                        if index > 0 {
                            graph_show_padding(&mut graph_state, prefix, &mut out)?;
                            out.write_all(b"\n")?;
                        }
                        graph_show_commit(&mut graph_state, prefix, &mut out)?;
                        let mut msg = render_log_raw_pretty(record, expand_tabs_in_log);
                        if let Some((notes_store, display_notes_refs)) = display_notes.as_ref()
                            && !display_notes_refs.is_empty()
                        {
                            let notes = render_notes_block(
                                &git_dir,
                                format,
                                notes_store,
                                display_notes_refs,
                                &record.oid,
                            )?;
                            msg.extend_from_slice(&notes);
                        }
                        if let Some(log_diff) = &log_diff {
                            let mut padding = String::new();
                            graph_state.padding_line(&mut padding);
                            let prefix_width = log_prefix_display_width(&padding)
                                + log_prefix_display_width(prefix);
                            log_diff.render(record, prefix_width, &mut diff_block)?;
                            if !diff_block.is_empty() {
                                msg.extend_from_slice(diff_opts.block_separator_for(record));
                                msg.extend_from_slice(&diff_block);
                            }
                        }
                        graph_show_commit_msg(&mut graph_state, prefix, &msg, &mut out)?;
                        prev_missing_newline = false;
                        continue;
                    }
                    if index > 0 {
                        graph_show_padding(&mut graph_state, prefix, &mut out)?;
                        out.write_all(b"\n")?;
                    }
                    graph_show_commit(&mut graph_state, prefix, &mut out)?;
                    write!(
                        out,
                        "commit {}",
                        format_log_commit_header_oid(&record.oid, abbrev_commit, abbrev_len)
                    )?;
                    if show_source
                        && let Some(source) = log_source_label(
                            &record.oid,
                            log_format_source.as_deref(),
                            source_labels.as_ref(),
                        )
                    {
                        write!(out, "\t{source}")?;
                    }
                    if let Some(labels) = decorations.get(&record.oid)
                        && !labels.is_empty()
                    {
                        write!(out, " ({})", labels.join(", "))?;
                    }
                    out.write_all(b"\n")?;
                    graph_show_oneline(&mut graph_state, prefix, &mut out)?;
                    let mut msg: Vec<u8> = Vec::new();
                    if show_signature {
                        msg.extend_from_slice(&log_signature_human_output(
                            &git_dir, &db, &config, record,
                        )?);
                    }
                    if record.parents.len() > 1 {
                        let merged: Vec<String> =
                            record.parents.iter().map(format_log_abbrev_oid).collect();
                        writeln!(msg, "Merge: {}", merged.join(" ")).map_err(io::Error::from)?;
                    }
                    if *kind == LogDefaultKind::Fuller {
                        writeln!(
                            msg,
                            "Author:     {}",
                            commit_identity_mailmapped(
                                &record.commit.author,
                                use_mailmap.then_some(output_mailmap)
                            )
                        )?;
                        writeln!(
                            msg,
                            "AuthorDate: {}",
                            commit_identity_date_or_sentinel(&record.commit.author, &date_mode)
                        )?;
                        writeln!(
                            msg,
                            "Commit:     {}",
                            commit_identity_mailmapped(
                                &record.commit.committer,
                                use_mailmap.then_some(output_mailmap)
                            )
                        )?;
                        writeln!(
                            msg,
                            "CommitDate: {}",
                            commit_identity_date_or_sentinel(&record.commit.committer, &date_mode)
                        )?;
                    } else {
                        writeln!(
                            msg,
                            "Author: {}",
                            commit_identity_mailmapped(
                                &record.commit.author,
                                use_mailmap.then_some(output_mailmap)
                            )
                        )?;
                        if *kind == LogDefaultKind::Full {
                            writeln!(
                                msg,
                                "Commit: {}",
                                commit_identity_mailmapped(
                                    &record.commit.committer,
                                    use_mailmap.then_some(output_mailmap)
                                )
                            )?;
                        }
                        if *kind == LogDefaultKind::Medium {
                            writeln!(
                                msg,
                                "Date:   {}",
                                commit_identity_date_or_sentinel(&record.commit.author, &date_mode)
                            )?;
                        }
                    }
                    msg.push(b'\n');
                    let display_message =
                        commit_message_for_commit_encoding(&record.commit, &output_encoding);
                    for line in commit_message_lines(&display_message) {
                        if line.is_empty() {
                            msg.push(b'\n');
                        } else {
                            msg.extend_from_slice(b"    ");
                            msg.extend_from_slice(&log_expand_tabs(line, expand_tabs_in_log));
                            msg.push(b'\n');
                        }
                    }
                    if let Some(log_diff) = &log_diff {
                        // Measure the graph padding that will prefix the diff
                        // lines so the stat width math sees the same budget
                        // git's line-prefix callback gives it.
                        let mut padding = String::new();
                        graph_state.padding_line(&mut padding);
                        let prefix_width =
                            log_prefix_display_width(&padding) + log_prefix_display_width(prefix);
                        log_diff.render(record, prefix_width, &mut diff_block)?;
                        if !diff_block.is_empty() {
                            msg.extend_from_slice(diff_opts.block_separator_for(record));
                            msg.extend_from_slice(&diff_block);
                        }
                    }
                    graph_show_commit_msg(&mut graph_state, prefix, &msg, &mut out)?;
                    prev_missing_newline = false;
                }
            }
        }
        out.flush()?;
        return Ok(());
    }

    let stdout = io::stdout();
    let mut default_out = if matches!(output, LogOutput::Default(_)) {
        Some(io::BufWriter::new(stdout.lock()))
    } else {
        None
    };
    let mut diff_block = Vec::new();
    let mut printed_entries = 0usize;
    for (index, record) in selected.iter().enumerate() {
        match output {
            LogOutput::Default(kind) => {
                let out = default_out.as_mut().expect("default output buffer");
                let separate_parent_count = log_diff
                    .as_ref()
                    .and_then(|log_diff| log_diff.separate_parent_count(record));
                let parent_slots = separate_parent_count.unwrap_or(1);
                // The diff block is rendered up front: whatchanged
                // (always_show_header = 0) omits the whole entry when the
                // commit's diff comes out empty.
                for parent_slot in 0..parent_slots {
                    let separate_parent_index = separate_parent_count.map(|_| parent_slot);
                    diff_block.clear();
                    if let Some(log_diff) = &log_diff {
                        let prefix_width =
                            log_prefix_display_width(line_prefix.as_deref().unwrap_or(""));
                        if let Some(parent_index) = separate_parent_index {
                            log_diff.render_parent(
                                record,
                                parent_index,
                                prefix_width,
                                &mut diff_block,
                            )?;
                        } else {
                            log_diff.render(record, prefix_width, &mut diff_block)?;
                        }
                    }
                    if whatchanged && log_diff.is_some() && diff_block.is_empty() {
                        continue;
                    }
                    if kind == LogDefaultKind::Raw {
                        if printed_entries > 0 {
                            // `-z` separates commits with NUL instead of the
                            // blank-line separator (git's line_termination).
                            if null_terminate {
                                out.write_all(b"\0")?;
                            } else {
                                writeln!(out)?;
                            }
                        }
                        printed_entries += 1;
                        let mut raw = render_log_raw_pretty(record, expand_tabs_in_log);
                        // git appends the notes block after the message and
                        // before the diff for every built-in format (here, raw)
                        // once notes display is active (`--show-notes`).
                        if let Some((notes_store, display_notes_refs)) = display_notes.as_ref()
                            && !display_notes_refs.is_empty()
                        {
                            let notes = render_notes_block(
                                &git_dir,
                                format,
                                notes_store,
                                display_notes_refs,
                                &record.oid,
                            )?;
                            raw.extend_from_slice(&notes);
                        }
                        if !diff_block.is_empty() {
                            raw.extend_from_slice(diff_opts.block_separator_for(record));
                            raw.extend_from_slice(&diff_block);
                        }
                        out.write_all(&raw)?;
                        continue;
                    }
                    if printed_entries > 0 {
                        // `-z` separates commits with NUL instead of the
                        // blank-line separator (git's line_termination).
                        if null_terminate {
                            out.write_all(b"\0")?;
                        } else {
                            writeln!(out)?;
                        }
                    }
                    printed_entries += 1;
                    if show_signature {
                        let signature = log_signature_human_output(&git_dir, &db, &config, record)?;
                        out.write_all(&signature)?;
                    }
                    write!(
                        out,
                        "commit {}",
                        format_log_commit_header_oid(&record.oid, abbrev_commit, abbrev_len)
                    )?;
                    if let Some(parent_index) = separate_parent_index
                        && let Some(parent) = record.parents.get(parent_index)
                    {
                        write!(
                            out,
                            " (from {})",
                            format_log_commit_header_oid(parent, abbrev_commit, abbrev_len)
                        )?;
                    }
                    if show_source
                        && let Some(source) = log_source_label(
                            &record.oid,
                            log_format_source.as_deref(),
                            source_labels.as_ref(),
                        )
                    {
                        write!(out, "\t{source}")?;
                    }
                    if let Some(labels) = decorations.get(&record.oid)
                        && !labels.is_empty()
                    {
                        write!(out, " ({})", labels.join(", "))?;
                    }
                    if show_parents {
                        let parent_abbrev = abbrev_commit.then_some(abbrev_len).flatten();
                        for parent in &record.parents {
                            write!(out, " {}", format_log_oid(parent, parent_abbrev))?;
                        }
                    }
                    writeln!(out)?;
                    if record.parents.len() > 1 {
                        let merged: Vec<String> =
                            record.parents.iter().map(format_log_abbrev_oid).collect();
                        writeln!(out, "Merge: {}", merged.join(" "))?;
                    }
                    if kind == LogDefaultKind::Fuller {
                        let author = commit_identity_mailmapped(
                            &record.commit.author,
                            use_mailmap.then_some(output_mailmap),
                        );
                        write!(out, "Author:     ")?;
                        out.write_all(&log_highlight_matches(
                            author.as_bytes(),
                            author_filters.as_ref(),
                            &grep_colors,
                        ))?;
                        writeln!(out)?;
                        writeln!(
                            out,
                            "AuthorDate: {}",
                            commit_identity_date_or_sentinel(&record.commit.author, &date_mode)
                        )?;
                        let committer = commit_identity_mailmapped(
                            &record.commit.committer,
                            use_mailmap.then_some(output_mailmap),
                        );
                        write!(out, "Commit:     ")?;
                        out.write_all(&log_highlight_matches(
                            committer.as_bytes(),
                            committer_filters.as_ref(),
                            &grep_colors,
                        ))?;
                        writeln!(out)?;
                        writeln!(
                            out,
                            "CommitDate: {}",
                            commit_identity_date_or_sentinel(&record.commit.committer, &date_mode)
                        )?;
                    } else {
                        let author = commit_identity_mailmapped(
                            &record.commit.author,
                            use_mailmap.then_some(output_mailmap),
                        );
                        write!(out, "Author: ")?;
                        out.write_all(&log_highlight_matches(
                            author.as_bytes(),
                            author_filters.as_ref(),
                            &grep_colors,
                        ))?;
                        writeln!(out)?;
                        if kind == LogDefaultKind::Full {
                            let committer = commit_identity_mailmapped(
                                &record.commit.committer,
                                use_mailmap.then_some(output_mailmap),
                            );
                            write!(out, "Commit: ")?;
                            out.write_all(&log_highlight_matches(
                                committer.as_bytes(),
                                committer_filters.as_ref(),
                                &grep_colors,
                            ))?;
                            writeln!(out)?;
                        }
                    }
                    if kind == LogDefaultKind::Medium {
                        writeln!(
                            out,
                            "Date:   {}",
                            commit_identity_date_or_sentinel(&record.commit.author, &date_mode)
                        )?;
                    }
                    writeln!(out)?;
                    let display_message =
                        commit_message_for_commit_encoding(&record.commit, &output_encoding);
                    for line in commit_message_lines(&display_message) {
                        write!(out, "    ")?;
                        out.write_all(&log_highlight_matches(
                            &log_expand_tabs(line, expand_tabs_in_log),
                            grep_filters.as_ref(),
                            &grep_colors,
                        ))?;
                        writeln!(out)?;
                    }
                    if let Some((notes_store, display_notes_refs)) = display_notes.as_ref()
                        && !display_notes_refs.is_empty()
                    {
                        let notes = render_notes_block(
                            &git_dir,
                            format,
                            notes_store,
                            display_notes_refs,
                            &record.oid,
                        )?;
                        out.write_all(&notes)?;
                    }
                    if !diff_block.is_empty() {
                        out.write_all(diff_opts.block_separator_for(record))?;
                        out.write_all(&diff_block)?;
                    }
                }
            }
            LogOutput::Compiled {
                ref compiled,
                final_newline,
                suppress_extra_final_newline,
                show_children: compiled_children,
                inline_children,
            } => {
                printed_entries += 1;
                let term: &[u8] = if null_terminate { b"\0" } else { b"\n" };
                if index > 0 && !final_newline {
                    io::stdout().write_all(term)?;
                }
                let format_context = LogFormatContext {
                    abbrev_len,
                    decorations: &decorations,
                    marker: '>',
                    dialect: LogFormatDialect::Log,
                    source: log_format_source.as_deref(),
                    date_mode: &date_mode,
                    source_oid: source_labels.as_ref(),
                    describe: Some(&CliLogDescribeAdapter(&describe_ctx)),
                    signature: Some(&CliLogSignatureAdapter(&signature_ctx)),
                    color: color_always,
                    output_encoding: &output_encoding,
                    mailmap: &CliMailmapAdapter(output_mailmap),
                    use_mailmap,
                };
                let mut ended_with_newline = false;
                if compiled_children && inline_children {
                    print_log_format_with_children(
                        record,
                        compiled,
                        format_context,
                        &child_oids,
                        abbrev_len,
                    )?;
                } else if let Some(prefix) = &line_prefix {
                    // `--line-prefix=<p>` prefixes every output line.
                    let mut line = Vec::with_capacity(compiled.estimated_line_capacity());
                    if let Some((notes_store, display_notes_refs)) = display_notes.as_ref() {
                        emit_encoded_compiled_log_format_with_notes(
                            &git_dir,
                            format,
                            notes_store,
                            display_notes_refs,
                            record,
                            compiled,
                            &format_context,
                            &mut line,
                        )?;
                    } else {
                        emit_encoded_compiled_log_format_no_notes(
                            record,
                            compiled,
                            &format_context,
                            &mut line,
                        )?;
                    }
                    let mut stdout = io::stdout();
                    let mut start = 0usize;
                    while start < line.len() {
                        let end = line[start..]
                            .iter()
                            .position(|&byte| byte == b'\n')
                            .map(|pos| start + pos + 1)
                            .unwrap_or(line.len());
                        stdout.write_all(prefix.as_bytes())?;
                        stdout.write_all(&line[start..end])?;
                        start = end;
                    }
                    if line.is_empty() {
                        stdout.write_all(prefix.as_bytes())?;
                    }
                    ended_with_newline = line.last() == Some(&b'\n');
                } else {
                    let mut out = Vec::with_capacity(compiled.estimated_line_capacity());
                    if let Some((notes_store, display_notes_refs)) = display_notes.as_ref() {
                        emit_encoded_compiled_log_format_with_notes(
                            &git_dir,
                            format,
                            notes_store,
                            display_notes_refs,
                            record,
                            compiled,
                            &format_context,
                            &mut out,
                        )?;
                    } else {
                        emit_encoded_compiled_log_format_no_notes(
                            record,
                            compiled,
                            &format_context,
                            &mut out,
                        )?;
                    }
                    ended_with_newline = out.last() == Some(&b'\n');
                    io::stdout().write_all(&out)?;
                }
                if final_newline
                    && (!suppress_extra_final_newline
                        || null_terminate
                        || index + 1 < selected.len()
                        || !ended_with_newline)
                {
                    io::stdout().write_all(term)?;
                }
                if let Some(log_diff) = &log_diff {
                    // oneline/format outputs put the diff right after the
                    // entry, with no separating blank line. `--line-prefix`
                    // narrows the stat budget and prefixes every diff line.
                    let prefix = line_prefix.as_deref().unwrap_or("");
                    log_diff.render(record, log_prefix_display_width(prefix), &mut diff_block)?;
                    if !diff_block.is_empty() {
                        let mut stdout = io::stdout();
                        let separator = diff_opts.block_separator_for(record);
                        if prefix.is_empty() {
                            stdout.write_all(separator)?;
                            stdout.write_all(&diff_block)?;
                        } else {
                            for line in separator.split_inclusive(|byte| *byte == b'\n') {
                                stdout.write_all(prefix.as_bytes())?;
                                stdout.write_all(line)?;
                            }
                            let mut start = 0usize;
                            while start < diff_block.len() {
                                let end = diff_block[start..]
                                    .iter()
                                    .position(|&byte| byte == b'\n')
                                    .map(|pos| start + pos + 1)
                                    .unwrap_or(diff_block.len());
                                stdout.write_all(prefix.as_bytes())?;
                                stdout.write_all(&diff_block[start..end])?;
                                start = end;
                            }
                        }
                    }
                }
            }
        }
    }
    if let Some(mut out) = default_out {
        out.flush()?;
    }
    Ok(())
}

/// The outcome of resolving a `--pretty=`/`--format=` spec.
pub(crate) enum ResolvedPretty {
    Oneline,
    Default,
    Short,
    Full,
    Fuller,
    Raw,
    /// `--pretty=reference`: `%C(auto)%h (%s, %ad)` with a default short date
    /// that an explicit `--date=` overrides (but `log.date` config does not).
    Reference,
    Compiled {
        compiled: CompiledLogFormat,
        final_newline: bool,
        suppress_extra_final_newline: bool,
    },
}

/// Resolve a `--pretty=`/`--format=` spec into a compiled format, mirroring
/// git's `get_commit_format` + `pretty.<name>` alias chain. `format_kind` is the
/// `--format=`/`tformat:` flag (terminator semantics → `final_newline: true`).
pub(crate) fn resolve_pretty_spec(
    spec: &str,
    format_kind: bool,
    config: &GitConfig,
) -> Result<ResolvedPretty> {
    // Follow `pretty.<name>` aliases (case-insensitive) up to a bounded depth,
    // matching git's loop guard against alias cycles.
    let mut current = spec.to_string();
    // `--format=`/`tformat:` apply terminator semantics. A `--format=X` with no
    // recognized prefix is treated as a user format `tformat:X`.
    let mut terminate = format_kind;
    for _ in 0..32 {
        if let Some(rest) = current.strip_prefix("format:") {
            return Ok(ResolvedPretty::Compiled {
                compiled: CompiledLogFormat::compile(rest, LogFormatDialect::Log)?,
                final_newline: terminate,
                suppress_extra_final_newline: terminate,
            });
        }
        if let Some(rest) = current.strip_prefix("tformat:") {
            return Ok(ResolvedPretty::Compiled {
                compiled: CompiledLogFormat::compile(rest, LogFormatDialect::Log)?,
                final_newline: true,
                suppress_extra_final_newline: false,
            });
        }
        match current.as_str() {
            "oneline" => return Ok(ResolvedPretty::Oneline),
            "medium" => return Ok(ResolvedPretty::Default),
            "short" => return Ok(ResolvedPretty::Short),
            "full" => return Ok(ResolvedPretty::Full),
            "fuller" => return Ok(ResolvedPretty::Fuller),
            "raw" => return Ok(ResolvedPretty::Raw),
            "reference" => {
                return Ok(ResolvedPretty::Reference);
            }
            _ => {}
        }
        // Try a `pretty.<name>` alias (case-insensitive); aliases may chain.
        if let Some(value) = config.get("pretty", None, &current) {
            current = value.to_string();
            terminate = false;
            continue;
        }
        // No builtin or alias matched. `--format=<raw>` treats the value as a
        // user format string with terminator semantics; bare `--pretty=<raw>`
        // does too when it contains a `%` placeholder (git's heuristic).
        if terminate || current.contains('%') {
            return Ok(ResolvedPretty::Compiled {
                compiled: CompiledLogFormat::compile(&current, LogFormatDialect::Log)?,
                final_newline: true,
                suppress_extra_final_newline: false,
            });
        }
        eprintln!("fatal: invalid --pretty format: {spec}");
        return Err(GitError::Exit(128));
    }
    eprintln!("fatal: invalid --pretty format: {spec}");
    Err(GitError::Exit(128))
}

// ---------------------------------------------------------------------------
// `--graph` rendering helpers (upstream graph.c's `graph_show_*` family)
// ---------------------------------------------------------------------------

/// The `log.graphColors` palette (empty -> the renderer's ANSI default).
/// Invalid entries are warned about and skipped, like upstream
/// `parse_graph_colors_config`.
fn log_graph_color_palette(config: &GitConfig) -> Vec<String> {
    let Some(value) = config.get("log", None, "graphColors") else {
        return Vec::new();
    };
    let mut palette = Vec::new();
    for token in value.split(',') {
        if token.trim().is_empty() {
            eprintln!("warning: ignored invalid color '{token}' in log.graphColors");
            continue;
        }
        match crate::commands::config_cmd::try_format_config_color_value(token) {
            Ok(code) => palette.push(code),
            Err(()) => {
                eprintln!("warning: ignored invalid color '{token}' in log.graphColors");
            }
        }
    }
    palette
}

// Emit graph rows up to and including the current commit's row (no trailing
// newline), prefixing each physical line with `prefix`.

fn log_fatal_unrecognized_argument(value: &str) -> Result<()> {
    eprintln!("fatal: unrecognized argument: {value}");
    Err(GitError::Exit(128))
}

fn log_diff_merges_requires_value_error() -> GitError {
    eprintln!("fatal: Option '--diff-merges' requires a value");
    GitError::Exit(128)
}

fn log_max_age_requires_value_error() -> GitError {
    eprintln!("fatal: Option '--max-age' requires a value");
    GitError::Exit(128)
}

fn log_min_age_requires_value_error() -> GitError {
    eprintln!("fatal: Option '--min-age' requires a value");
    GitError::Exit(128)
}

fn log_date_cutoff_requires_value_error(option: &str) -> GitError {
    eprintln!("fatal: Option '{option}' requires a value");
    GitError::Exit(128)
}

fn log_no_walk_invalid_argument(value: &str) -> Result<()> {
    eprintln!("error: invalid argument to --no-walk");
    eprintln!("fatal: unrecognized argument: {value}");
    Err(GitError::Exit(128))
}

fn log_parse_parent_count(value: &str) -> Result<usize> {
    value.parse::<usize>().map_err(|_| {
        eprintln!("fatal: '{value}': not an integer");
        GitError::Exit(128)
    })
}

fn log_parse_abbrev_width(value: &str) -> usize {
    value.parse::<usize>().unwrap_or(0).max(4)
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum LogDefaultKind {
    /// `medium` (the default): includes the `Date:` line.
    Medium,
    /// `--pretty=short`: omits the `Date:` line.
    Short,
    /// `--pretty=full`: author and committer identities, no dates.
    Full,
    /// `--pretty=fuller`: author and committer identities with dates.
    Fuller,
    /// `--pretty=raw`: full object headers and raw identity lines.
    Raw,
}

#[derive(Debug, Clone)]
enum LogOutput {
    /// `short`/`medium` structured layout.
    Default(LogDefaultKind),
    /// `--oneline`, `--pretty=oneline`, or `--format=` resolved to a compiled stream.
    Compiled {
        compiled: CompiledLogFormat,
        final_newline: bool,
        suppress_extra_final_newline: bool,
        show_children: bool,
        /// When true, `--children` oids are printed between the commit oid and subject
        /// (oneline presets only; custom `--format=` ignores `--children`).
        inline_children: bool,
    },
}

fn log_output_needs_abbrev(output: &LogOutput, abbrev_commit: bool, show_children: bool) -> bool {
    match output {
        LogOutput::Default(_) => abbrev_commit,
        LogOutput::Compiled { compiled, .. } => {
            show_children
                || compiled.tokens.iter().any(|token| {
                    matches!(
                        token,
                        FormatToken::OidAbbrev
                            | FormatToken::TreeAbbrev
                            | FormatToken::ParentsAbbrev
                    )
                })
        }
    }
}

fn log_age_filters_match(
    record: &sley_rev::CommitRecord,
    max_age: Option<i64>,
    min_age: Option<i64>,
) -> Result<bool> {
    if max_age.is_none() && min_age.is_none() {
        return Ok(true);
    }
    let timestamp = commit_identity_timestamp_i64(&record.commit.committer)?;
    Ok(max_age.is_none_or(|age| timestamp >= age) && min_age.is_none_or(|age| timestamp <= age))
}

fn print_log_selected_child_oids(
    record: &sley_rev::CommitRecord,
    child_oids: &HashMap<ObjectId, Vec<ObjectId>>,
    show_children: bool,
    abbrev_len: Option<usize>,
) {
    if show_children && let Some(children) = child_oids.get(&record.oid) {
        for child in children {
            print!(" {}", format_log_oid(child, abbrev_len));
        }
    }
}

fn log_signature_human_output(
    git_dir: &Path,
    db: &FileObjectDatabase,
    config: &GitConfig,
    record: &sley_rev::CommitRecord,
) -> Result<Vec<u8>> {
    let object = db.read_object(&record.oid)?;
    let Some((payload, signature)) = commands::signing::commit_signature_payload(&object.body)
    else {
        return Ok(Vec::new());
    };
    let verification =
        commands::signing::verify_payload(git_dir, Some(config), &payload, &signature)?;
    Ok(verification.human_output)
}

fn print_log_format_with_children(
    record: &sley_rev::CommitRecord,
    compiled: &CompiledLogFormat,
    context: LogFormatContext<'_>,
    child_oids: &HashMap<ObjectId, Vec<ObjectId>>,
    abbrev_len: Option<usize>,
) -> Result<()> {
    let subject_index = compiled
        .tokens
        .iter()
        .position(|token| matches!(token, FormatToken::Subject | FormatToken::SanitizedSubject));
    let child_abbrev_len = if compiled.tokens.contains(&FormatToken::OidFull) {
        None
    } else {
        abbrev_len
    };
    let Some(subject_index) = subject_index else {
        print_log_format(record, compiled, context)?;
        print_log_selected_child_oids(record, child_oids, true, child_abbrev_len);
        return Ok(());
    };
    let mut pre_subject_end = subject_index;
    while pre_subject_end > 0
        && matches!(
            compiled.tokens[pre_subject_end - 1],
            FormatToken::Literal(ref text) if text.chars().all(char::is_whitespace)
        )
    {
        pre_subject_end -= 1;
    }
    let mut line = Vec::with_capacity(compiled.estimated_line_capacity());
    emit_compiled_log_format(record, compiled, &context, &mut line, 0..pre_subject_end)?;
    io::stdout().write_all(&line)?;
    print_log_selected_child_oids(record, child_oids, true, child_abbrev_len);
    if pre_subject_end < subject_index {
        io::stdout().write_all(b" ")?;
    }
    line.clear();
    emit_compiled_log_format(
        record,
        compiled,
        &context,
        &mut line,
        subject_index..compiled.tokens.len(),
    )?;
    io::stdout().write_all(&line)?;
    io::stdout().flush()?;
    Ok(())
}
