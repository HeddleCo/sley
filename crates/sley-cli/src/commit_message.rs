//! Commit message assembly, cleanup modes, and related validation helpers.

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use sley::plumbing::sley_object::{Commit, ObjectType};
use sley::plumbing::sley_odb::ObjectReader;
use sley::{GitError, ObjectFormat, Result};

use crate::sley_rev;

pub(crate) fn commit_message_requires_value_error() -> Result<()> {
    eprintln!("error: switch `m' requires a value");
    Err(GitError::Exit(129))
}

pub(crate) fn read_commit_pathspecs_from_file(path: &Path, nul: bool) -> Result<Vec<PathBuf>> {
    let mut bytes = Vec::new();
    if path == Path::new("-") {
        io::stdin().read_to_end(&mut bytes)?;
    } else {
        bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(err) => {
                let message = match err.kind() {
                    io::ErrorKind::NotFound => "No such file or directory".to_string(),
                    io::ErrorKind::PermissionDenied => "Permission denied".to_string(),
                    _ => err.to_string(),
                };
                eprintln!(
                    "fatal: could not open '{}' for reading: {message}",
                    path.display()
                );
                return Err(GitError::Exit(128));
            }
        };
    }
    let separator = if nul { b'\0' } else { b'\n' };
    Ok(bytes
        .split(|byte| *byte == separator)
        .filter_map(|entry| {
            let entry = if !nul && entry.ends_with(b"\r") {
                &entry[..entry.len() - 1]
            } else {
                entry
            };
            if entry.is_empty() {
                None
            } else {
                if !nul && entry.first() == Some(&b'"') {
                    let mut unquoted = Vec::new();
                    if crate::commands::ref_command_stream::unquote_c_style(entry, &mut unquoted)
                        .is_some()
                    {
                        return Some(PathBuf::from(
                            String::from_utf8_lossy(&unquoted).into_owned(),
                        ));
                    }
                }
                Some(PathBuf::from(String::from_utf8_lossy(entry).into_owned()))
            }
        })
        .collect())
}

pub(crate) fn commit_unified_requires_value_error(short: bool) -> Result<()> {
    if short {
        eprintln!("error: switch `U' requires a value");
    } else {
        eprintln!("error: option `unified' requires a value");
    }
    Err(GitError::Exit(129))
}

pub(crate) fn commit_inter_hunk_context_requires_value_error() -> Result<()> {
    eprintln!("error: option `inter-hunk-context' requires a value");
    Err(GitError::Exit(129))
}

pub(crate) fn commit_validate_unified_context(value: &str, short: bool) -> Result<()> {
    if value.is_empty() {
        return commit_unified_expects_numerical_value_error(short);
    }
    if git_count_value_is_valid(value) {
        return Ok(());
    }
    if short {
        eprintln!("error: switch `U' expects an integer value with an optional k/m/g suffix");
    } else {
        eprintln!("error: option `unified' expects an integer value with an optional k/m/g suffix");
    }
    Err(GitError::Exit(129))
}

pub(crate) fn patch_validate_unified_context(value: &str, short: bool) -> Result<()> {
    commit_validate_unified_context(value, short)?;
    if git_count_value_is_negative(value) {
        eprintln!("fatal: '--unified' cannot be negative");
        return Err(GitError::Exit(128));
    }
    Ok(())
}

pub(crate) fn commit_unified_expects_numerical_value_error(short: bool) -> Result<()> {
    if short {
        eprintln!("error: switch `U' expects a numerical value");
    } else {
        eprintln!("error: option `unified' expects a numerical value");
    }
    Err(GitError::Exit(129))
}

pub(crate) fn commit_validate_inter_hunk_context(value: &str) -> Result<()> {
    if value.is_empty() {
        return commit_inter_hunk_context_expects_numerical_value_error();
    }
    if git_count_value_is_valid(value) {
        return Ok(());
    }
    eprintln!(
        "error: option `inter-hunk-context' expects an integer value with an optional k/m/g suffix"
    );
    Err(GitError::Exit(129))
}

pub(crate) fn patch_validate_inter_hunk_context(value: &str) -> Result<()> {
    commit_validate_inter_hunk_context(value)?;
    if git_count_value_is_negative(value) {
        eprintln!("fatal: '--inter-hunk-context' cannot be negative");
        return Err(GitError::Exit(128));
    }
    Ok(())
}

pub(crate) fn commit_inter_hunk_context_expects_numerical_value_error() -> Result<()> {
    eprintln!("error: option `inter-hunk-context' expects a numerical value");
    Err(GitError::Exit(129))
}

pub(crate) fn git_count_value_is_negative(value: &str) -> bool {
    let number = match value.as_bytes().last() {
        Some(b'k' | b'K' | b'm' | b'M' | b'g' | b'G') => &value[..value.len() - 1],
        _ => value,
    };
    number.trim_start().starts_with('-')
}

pub(crate) fn git_count_value_is_valid(value: &str) -> bool {
    let number = match value.as_bytes().last() {
        Some(b'k' | b'K' | b'm' | b'M' | b'g' | b'G') => &value[..value.len() - 1],
        _ => value,
    };
    let digits = match number.as_bytes().first() {
        Some(b'+' | b'-') if number.len() > 1 => &number[1..],
        _ => number,
    };
    !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
}

pub(crate) fn commit_tree_file_requires_value_error() -> Result<()> {
    eprintln!("error: switch `F' requires a value");
    Err(GitError::Exit(129))
}

pub(crate) fn read_commit_message_file(path: &str) -> Result<Vec<u8>> {
    if path == "-" {
        let mut message = Vec::new();
        io::stdin().read_to_end(&mut message)?;
        Ok(message)
    } else {
        Ok(fs::read(path)?)
    }
}

pub(crate) fn commit_message_from_prepared_chunks(chunks: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for chunk in chunks
        .iter()
        .filter(|chunk| !commit_message_chunk_is_empty(chunk))
    {
        if !out.is_empty() {
            out.push(b'\n');
        }
        out.extend_from_slice(chunk);
    }
    out
}

/// A `-m` chunk is empty only when it has no content at all. A lone newline
/// (from `-m "$LF"`) is a real paragraph under `--cleanup=verbatim` and must
/// not be dropped — git keeps it and produces a non-empty message (t6006).
pub(crate) fn commit_message_chunk_is_empty(chunk: &[u8]) -> bool {
    chunk.is_empty()
}

/// The resolved commit-message cleanup mode (git's `enum
/// commit_msg_cleanup_mode`). The raw `--cleanup`/`commit.cleanup` arg plus
/// whether an editor runs resolve to one of these via
/// [`resolve_commit_cleanup_mode`].
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommitCleanupMode {
    /// `verbatim` → `COMMIT_MSG_CLEANUP_NONE`: no cleanup at all.
    Verbatim,
    /// `whitespace` (and the non-editor default) → `COMMIT_MSG_CLEANUP_SPACE`:
    /// strip trailing whitespace, squash blank-line runs, drop leading/trailing
    /// blanks. Comment lines are preserved.
    Whitespace,
    /// `strip` (and the editor default) → `COMMIT_MSG_CLEANUP_ALL`: whitespace
    /// cleanup plus dropping comment lines.
    Strip,
    /// `scissors` (with an editor) → `COMMIT_MSG_CLEANUP_SCISSORS`: truncate at
    /// the scissors line, then whitespace cleanup (comments preserved).
    Scissors,
}

/// Resolve the raw `--cleanup`/`commit.cleanup` argument (or its absence) to a
/// concrete [`CommitCleanupMode`], honouring git's editor-dependent defaults
/// (`get_cleanup_mode`): `default`/absent → `ALL` with an editor else `SPACE`;
/// `scissors` → `SCISSORS` with an editor else `SPACE`. Unknown values are
/// rejected earlier by [`validate_commit_cleanup_mode`], so we treat them as the
/// default here.
pub(crate) fn resolve_commit_cleanup_mode(
    arg: Option<&str>,
    use_editor: bool,
) -> CommitCleanupMode {
    let editor_default = if use_editor {
        CommitCleanupMode::Strip
    } else {
        CommitCleanupMode::Whitespace
    };
    match arg {
        None | Some("default") => editor_default,
        Some("verbatim") => CommitCleanupMode::Verbatim,
        Some("whitespace") => CommitCleanupMode::Whitespace,
        Some("strip") => CommitCleanupMode::Strip,
        Some("scissors") => {
            if use_editor {
                CommitCleanupMode::Scissors
            } else {
                CommitCleanupMode::Whitespace
            }
        }
        Some(_) => editor_default,
    }
}

/// Apply a resolved cleanup mode to a message (git's `cleanup_message`):
///   * SCISSORS (or `verbose`) truncates the message at the scissors line.
///   * Any mode other than NONE/Verbatim runs `strbuf_stripspace`, additionally
///     dropping comment lines under `Strip` (ALL).
pub(crate) fn commit_cleanup_message(
    mut message: Vec<u8>,
    mode: CommitCleanupMode,
    comment_char: &str,
    verbose: bool,
) -> Vec<u8> {
    if verbose || mode == CommitCleanupMode::Scissors {
        let end = commit_locate_scissors(&message, comment_char);
        message.truncate(end);
    }
    match mode {
        CommitCleanupMode::Verbatim => message,
        CommitCleanupMode::Strip => commit_stripspace_message(&message, Some(comment_char)),
        CommitCleanupMode::Whitespace | CommitCleanupMode::Scissors => {
            commit_stripspace_message(&message, None)
        }
    }
}

pub(crate) fn commit_stripspace_message(message: &[u8], comment_char: Option<&str>) -> Vec<u8> {
    let mut out = Vec::new();
    let mut pending_blank = false;
    let comment = comment_char.map(str::as_bytes);
    for raw_line in message.split(|byte| *byte == b'\n') {
        let line = commit_trim_trailing_space(raw_line);
        if comment.is_some_and(|prefix| line.starts_with(prefix)) {
            continue;
        }
        if line.is_empty() {
            if !out.is_empty() {
                pending_blank = true;
            }
            continue;
        }
        if pending_blank {
            out.push(b'\n');
            pending_blank = false;
        }
        out.extend_from_slice(line);
        out.push(b'\n');
    }
    out
}

pub(crate) fn commit_trim_trailing_space(line: &[u8]) -> &[u8] {
    let end = line
        .iter()
        .rposition(|byte| !matches!(byte, b' ' | b'\t' | b'\r'))
        .map(|idx| idx + 1)
        .unwrap_or(0);
    &line[..end]
}

/// git's `wt_status_locate_end` over a byte message: the offset of the scissors
/// ("cut") line, or the message length when none is present. Everything from the
/// scissors line on is below the cut and is dropped by SCISSORS/verbose cleanup.
pub(crate) fn commit_locate_scissors(message: &[u8], comment_char: &str) -> usize {
    const CUT_BODY: &[u8] = b"------------------------ >8 ------------------------\n";
    // pattern head (no leading newline): "<comment> <cut_body>"
    let mut head = comment_char.as_bytes().to_vec();
    head.push(b' ');
    head.extend_from_slice(CUT_BODY);
    if message.starts_with(&head) {
        return 0;
    }
    // full pattern: "\n<comment> <cut_body>"
    let mut pattern = vec![b'\n'];
    pattern.extend_from_slice(&head);
    if pattern.len() > message.len() {
        return message.len();
    }
    match message
        .windows(pattern.len())
        .position(|w| w == pattern.as_slice())
    {
        Some(p) => (p + 1).min(message.len()),
        None => message.len(),
    }
}

pub(crate) fn read_reused_commit(
    git_dir: &Path,
    format: ObjectFormat,
    rev: &str,
    replace_objects: bool,
) -> Result<Commit> {
    let result = (|| {
        let db = crate::repository::open_object_database(git_dir, format, replace_objects)?;
        let oid = sley_rev::RevisionResolver::new(git_dir, format, &db).resolve(rev)?;
        let commit_oid = sley_rev::peel_to_commit(&db, format, &oid)?;
        let object = db.read_object(&commit_oid)?;
        if object.object_type != ObjectType::Commit {
            return Err(GitError::InvalidObject(format!(
                "expected commit {}, found {}",
                commit_oid,
                object.object_type.as_str()
            )));
        }
        Commit::parse(format, &object.body)
    })();
    match result {
        Ok(commit) => Ok(commit),
        Err(_) => {
            eprintln!("fatal: could not lookup commit '{rev}'");
            Err(GitError::Exit(128))
        }
    }
}
