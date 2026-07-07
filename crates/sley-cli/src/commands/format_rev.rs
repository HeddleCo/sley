//! `git format-rev` — expand revision placeholders inside a format string.
//!
//! Mirrors upstream `builtin/name-rev.c::cmd_format_rev`: read revs or free
//! text from stdin, substitute pretty-formatted commit output for each
//! resolvable object name.

use std::collections::HashMap;
use std::io::{self, Read, Write};

use sley::plumbing::sley_rev;
use sley_core::DateMode;
use sley_notes::NotesRef;
use sley_pretty::{CompiledLogFormat, LogFormatDialect, LogFormatContext};

use crate::commands::log::{
    compiled_format_uses_notes, expand_notes_glob, format_commit_pretty_with_notes,
    resolve_pretty_spec, ResolvedPretty,
};
use crate::*;

struct FormatRevOptions {
    format: Option<String>,
    stdin_mode: Option<StdinMode>,
    nul_input: bool,
    nul_output: bool,
    notes_refs: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StdinMode {
    Text,
    Revs,
}

struct FormatRevFormat {
    compiled: CompiledLogFormat,
    date_mode: &'static DateMode,
}

pub(crate) fn cmd_format_rev(args: &[String]) -> Result<()> {
    let options = parse_format_rev_options(args)?;
    let Some(format) = options.format.as_deref() else {
        eprintln!("fatal: '--format' is required");
        return Err(GitError::Exit(128));
    };
    let Some(stdin_mode) = options.stdin_mode else {
        eprintln!("fatal: '--stdin-mode' is required");
        return Err(GitError::Exit(128));
    };
    let repo = RepositoryContext::discover_current()?;
    let git_dir = repo.git_dir();
    let object_format = repo.format();
    let db = repo.objects();
    let config = read_repo_config(git_dir)?;
    let abbrev_len = repository_abbrev(git_dir, object_format)?;
    let resolved = resolve_pretty_spec(format, true, &config)?;
    let format_rev = resolve_format_rev_format(&resolved)?;
    let notes_refs = resolve_format_rev_notes_refs(git_dir, object_format, &options)?;
    let show_notes = !notes_refs.is_empty() || compiled_format_uses_notes(&format_rev.compiled);

    let mut input = Vec::new();
    io::stdin().read_to_end(&mut input)?;
    let terminator = if options.nul_output { b'\0' } else { b'\n' };
    let stdout = io::stdout();
    let mut out = stdout.lock();

    match stdin_mode {
        StdinMode::Revs => {
            for line in read_input_records(&input, options.nul_input) {
                let line = String::from_utf8_lossy(&line);
                let line = line.trim_end_matches('\n');
                if line.is_empty() {
                    continue;
                }
                match format_rev_revs_line(
                    &repo,
                    git_dir,
                    object_format,
                    db,
                    &format_rev,
                    &notes_refs,
                    show_notes,
                    abbrev_len,
                    line,
                ) {
                    Ok(Some(bytes)) => {
                        out.write_all(&bytes)?;
                        out.write_all(&[terminator])?;
                    }
                    Ok(None) => {}
                    Err(err) => return Err(err),
                }
            }
        }
        StdinMode::Text => {
            format_rev_text_input(
                &repo,
                git_dir,
                object_format,
                db,
                &format_rev,
                &notes_refs,
                show_notes,
                abbrev_len,
                &input,
                options.nul_input,
                terminator,
                &mut out,
            )?;
        }
    }
    out.flush()?;
    Ok(())
}

fn resolve_format_rev_format(resolved: &ResolvedPretty) -> Result<FormatRevFormat> {
    static DATE_DEFAULT: DateMode = DateMode::Default;
    static DATE_SHORT: DateMode = DateMode::Short;
    match resolved {
        ResolvedPretty::Reference => Ok(FormatRevFormat {
            compiled: CompiledLogFormat::compile("%C(auto)%h (%s, %ad)", LogFormatDialect::Log)?,
            date_mode: &DATE_SHORT,
        }),
        ResolvedPretty::Compiled { compiled, .. } => Ok(FormatRevFormat {
            compiled: compiled.clone(),
            date_mode: &DATE_DEFAULT,
        }),
        _ => {
            eprintln!("fatal: unsupported format for format-rev");
            Err(GitError::Exit(128))
        }
    }
}

fn resolve_format_rev_notes_refs(
    git_dir: &Path,
    format: ObjectFormat,
    options: &FormatRevOptions,
) -> Result<Vec<String>> {
    if options.notes_refs.is_empty() {
        return Ok(Vec::new());
    }
    let store = FileRefStore::new(git_dir, format);
    let mut refs = Vec::new();
    for spec in &options.notes_refs {
        let spec = if spec == "*" {
            "refs/notes/*".to_string()
        } else {
            NotesRef::expand(spec).as_str().to_string()
        };
        for expanded in expand_notes_glob(&store, &spec)? {
            if !refs.iter().any(|existing| existing == &expanded) {
                refs.push(expanded);
            }
        }
    }
    Ok(refs)
}

fn format_rev_log_context<'a>(
    decorations: &'a HashMap<ObjectId, Vec<String>>,
    mailmap: &'a dyn MailmapLookup,
    abbrev_len: Option<usize>,
    date_mode: &'a DateMode,
) -> LogFormatContext<'a> {
    LogFormatContext {
        abbrev_len,
        decorations,
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
    }
}

fn format_rev_revs_line(
    repo: &RepositoryContext,
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    format_rev: &FormatRevFormat,
    notes_refs: &[String],
    show_notes: bool,
    abbrev_len: Option<usize>,
    rev: &str,
) -> Result<Option<Vec<u8>>> {
    let oid = match repo.resolve_revision(rev) {
        Ok(oid) => oid,
        Err(_) => {
            eprintln!("Could not get object name for {rev}. Skipping.");
            return Ok(None);
        }
    };
    if db.read_object(&oid).is_err() {
        eprintln!("Could not get object for {rev}. Skipping.");
        return Ok(None);
    }
    let commit = match sley_rev::peel_to_commit(db, format, &oid) {
        Ok(commit) => commit,
        Err(_) => {
            eprintln!("Could not get commit for {rev}. Skipping.");
            return Ok(None);
        }
    };
    let record = read_rev_list_commit_record(db, format, commit)?;
    let decorations = HashMap::new();
    let mailmap = EmptyMailmap;
    let context =
        format_rev_log_context(&decorations, &mailmap, abbrev_len, format_rev.date_mode);
    Ok(Some(format_commit_pretty_with_notes(
        git_dir,
        format,
        &record,
        &format_rev.compiled,
        &context,
        show_notes,
        notes_refs,
    )?))
}

fn format_rev_text_input(
    repo: &RepositoryContext,
    git_dir: &Path,
    object_format: ObjectFormat,
    db: &FileObjectDatabase,
    format_rev: &FormatRevFormat,
    notes_refs: &[String],
    show_notes: bool,
    abbrev_len: Option<usize>,
    input: &[u8],
    nul_input: bool,
    terminator: u8,
    out: &mut impl Write,
) -> Result<()> {
    let hex_len = object_format.hex_len();
    let decorations = HashMap::new();
    let mailmap = EmptyMailmap;
    let context =
        format_rev_log_context(&decorations, &mailmap, abbrev_len, format_rev.date_mode);
    for record in read_input_records(input, nul_input) {
        let mut segment_start = 0usize;
        let mut counter = 0usize;
        let mut index = 0usize;
        while index < record.len() {
            let byte = record[index];
            if !is_lower_hex(byte) {
                counter = 0;
            } else {
                counter += 1;
                let next_is_hex = record.get(index + 1).is_some_and(|next| is_lower_hex(*next));
                if counter == hex_len && !next_is_hex {
                    let hex_start = index + 1 - hex_len;
                    let hex = &record[hex_start..=index];
                    counter = 0;
                    if let Some(formatted) = format_rev_text_substitute(
                        repo,
                        git_dir,
                        object_format,
                        db,
                        format_rev,
                        notes_refs,
                        show_notes,
                        &context,
                        hex,
                    )? {
                        out.write_all(&record[segment_start..hex_start])?;
                        out.write_all(&formatted)?;
                        segment_start = index + 1;
                    }
                }
            }
            index += 1;
        }
        out.write_all(&record[segment_start..])?;
        out.write_all(&[terminator])?;
    }
    Ok(())
}

fn format_rev_text_substitute(
    repo: &RepositoryContext,
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    format_rev: &FormatRevFormat,
    notes_refs: &[String],
    show_notes: bool,
    context: &LogFormatContext<'_>,
    hex: &[u8],
) -> Result<Option<Vec<u8>>> {
    let Ok(text) = std::str::from_utf8(hex) else {
        return Ok(None);
    };
    let Ok(oid) = ObjectId::from_hex(format, text) else {
        return Ok(None);
    };
    if db.read_object(&oid).is_err() {
        return Ok(None);
    }
    let commit = match sley_rev::peel_to_commit(db, format, &oid) {
        Ok(commit) => commit,
        Err(_) => return Ok(None),
    };
    let record = read_rev_list_commit_record(db, format, commit)?;
    Ok(Some(format_commit_pretty_with_notes(
        git_dir,
        format,
        &record,
        &format_rev.compiled,
        context,
        show_notes,
        notes_refs,
    )?))
}

fn read_input_records(input: &[u8], nul_input: bool) -> Vec<Vec<u8>> {
    if nul_input {
        input
            .split(|byte| *byte == 0)
            .filter(|record| !record.is_empty())
            .map(|record| record.to_vec())
            .collect()
    } else {
        input
            .split(|byte| *byte == b'\n')
            .map(|record| record.to_vec())
            .filter(|record| !record.is_empty())
            .collect()
    }
}

fn is_lower_hex(byte: u8) -> bool {
    byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
}

fn parse_format_rev_options(args: &[String]) -> Result<FormatRevOptions> {
    let mut options = FormatRevOptions {
        format: None,
        stdin_mode: None,
        nul_input: false,
        nul_output: false,
        notes_refs: Vec::new(),
    };
    let mut positional = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "-z" || arg == "--null" {
            options.nul_input = true;
            options.nul_output = true;
            continue;
        }
        if arg == "--null-input" || arg == "---null-input" {
            options.nul_input = true;
            continue;
        }
        if arg == "--no-null-input" {
            options.nul_input = false;
            continue;
        }
        if arg == "--null-output" {
            options.nul_output = true;
            continue;
        }
        if arg == "--no-null-output" {
            options.nul_output = false;
            continue;
        }
        if let Some(value) = arg.strip_prefix("--format=") {
            options.format = Some(value.to_string());
            continue;
        }
        if arg == "--format" {
            let Some(value) = iter.next() else {
                eprintln!("error: option `--format` requires a value");
                return Err(GitError::Exit(129));
            };
            options.format = Some(value.clone());
            continue;
        }
        if let Some(value) = arg.strip_prefix("--stdin-mode=") {
            options.stdin_mode = Some(parse_stdin_mode(value)?);
            continue;
        }
        if arg == "--stdin-mode" {
            let Some(value) = iter.next() else {
                eprintln!("error: option `--stdin-mode` requires a value");
                return Err(GitError::Exit(129));
            };
            options.stdin_mode = Some(parse_stdin_mode(value)?);
            continue;
        }
        if let Some(value) = arg.strip_prefix("--notes=") {
            options.notes_refs.push(value.to_string());
            continue;
        }
        if arg == "--notes" {
            let Some(value) = iter.next() else {
                eprintln!("error: option `--notes` requires a value");
                return Err(GitError::Exit(129));
            };
            options.notes_refs.push(value.clone());
            continue;
        }
        if arg == "-h" || arg == "--help" {
            print_format_rev_usage();
            return Err(GitError::Exit(129));
        }
        if arg.starts_with('-') {
            eprintln!("error: unknown option `{arg}`");
            print_format_rev_usage();
            return Err(GitError::Exit(129));
        }
        positional.push(arg.clone());
    }
    if !positional.is_empty() {
        eprintln!("error: too many arguments");
        print_format_rev_usage();
        return Err(GitError::Exit(129));
    }
    Ok(options)
}

fn parse_stdin_mode(value: &str) -> Result<StdinMode> {
    match value {
        "text" => Ok(StdinMode::Text),
        "revs" | "rev" => Ok(StdinMode::Revs),
        _ => {
            eprintln!("fatal: '--stdin-mode' needs to be either text, revs, or rev");
            Err(GitError::Exit(128))
        }
    }
}

fn print_format_rev_usage() {
    eprintln!(
        "usage: git format-rev --stdin-mode=<mode> --format=<pretty> [--[no-]notes=<ref>] [-z] [--[no-]null-output] [--[no-]null-input]"
    );
}

struct EmptyMailmap;

impl MailmapLookup for EmptyMailmap {
    fn map_user(&self, name: &str, email: &str) -> (String, String) {
        (name.to_string(), email.to_string())
    }
}