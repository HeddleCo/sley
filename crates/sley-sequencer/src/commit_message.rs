//! Commit message assembly, cleanup modes, and related validation helpers.
//!
//! Sunk out of the CLI so sequencer-adjacent consumers (and future movers)
//! share one canonical implementation of git's commit-message cleanup state
//! machine (`wt-status` scissors handling, `strbuf_stripspace`, and the
//! `--unified`/`--inter-hunk-context` count validation).

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use sley_core::{GitError, ObjectFormat, Result};
use sley_object::{Commit, ObjectType};
use sley_odb::{FileObjectDatabase, ObjectReader};

pub fn commit_message_requires_value_error() -> Result<()> {
    eprintln!("error: switch `m' requires a value");
    Err(GitError::Exit(129))
}

/// Decode a C-style quoted string per git's `quote.c` (`unquote_c_style`).
///
/// Decodes a leading `"`-quoted C string from `input`, appending the decoded
/// bytes to `out`. Returns the number of input bytes consumed (up to and
/// including the closing quote) on success, or `None` if the quoting is
/// malformed. A NUL byte terminates the input just as it does in git's C-string
/// view. This is the workspace's single canonical port; the sley-diff-merge
/// name parser carries a mirror copy with its own oracle matrix.
pub fn unquote_c_style(input: &[u8], out: &mut Vec<u8>) -> Option<usize> {
    let mut i = 0usize;
    if input.get(i).copied()? != b'"' {
        return None;
    }
    i += 1;
    loop {
        // Copy the run up to the next '"' or '\\' (NUL ends the C string).
        while let Some(&c) = input.get(i) {
            if c == b'"' || c == b'\\' || c == 0 {
                break;
            }
            out.push(c);
            i += 1;
        }
        match input.get(i).copied() {
            Some(b'"') => {
                i += 1;
                return Some(i);
            }
            Some(b'\\') => {
                i += 1;
            }
            // NUL or end-of-input before a closing quote: malformed.
            _ => return None,
        }
        let esc = input.get(i).copied()?;
        i += 1;
        let decoded = match esc {
            b'a' => 0x07,
            b'b' => 0x08,
            b'f' => 0x0c,
            b'n' => b'\n',
            b'r' => b'\r',
            b't' => b'\t',
            b'v' => 0x0b,
            b'\\' | b'"' => esc,
            b'0'..=b'3' => {
                // Octal: first digit 0..3 (>=4 would overflow a byte), then two
                // more octal digits, all required.
                let mut ac = ((esc - b'0') as u32) << 6;
                let d1 = input.get(i).copied()?;
                if !(b'0'..=b'7').contains(&d1) {
                    return None;
                }
                i += 1;
                ac |= ((d1 - b'0') as u32) << 3;
                let d2 = input.get(i).copied()?;
                if !(b'0'..=b'7').contains(&d2) {
                    return None;
                }
                i += 1;
                ac |= (d2 - b'0') as u32;
                ac as u8
            }
            _ => return None,
        };
        out.push(decoded);
    }
}

pub fn read_commit_pathspecs_from_file(path: &Path, nul: bool) -> Result<Vec<PathBuf>> {
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
                    if unquote_c_style(entry, &mut unquoted).is_some() {
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

pub fn commit_unified_requires_value_error(short: bool) -> Result<()> {
    if short {
        eprintln!("error: switch `U' requires a value");
    } else {
        eprintln!("error: option `unified' requires a value");
    }
    Err(GitError::Exit(129))
}

pub fn commit_inter_hunk_context_requires_value_error() -> Result<()> {
    eprintln!("error: option `inter-hunk-context' requires a value");
    Err(GitError::Exit(129))
}

pub fn commit_validate_unified_context(value: &str, short: bool) -> Result<()> {
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

pub fn patch_validate_unified_context(value: &str, short: bool) -> Result<()> {
    commit_validate_unified_context(value, short)?;
    if git_count_value_is_negative(value) {
        eprintln!("fatal: '--unified' cannot be negative");
        return Err(GitError::Exit(128));
    }
    Ok(())
}

pub fn commit_unified_expects_numerical_value_error(short: bool) -> Result<()> {
    if short {
        eprintln!("error: switch `U' expects a numerical value");
    } else {
        eprintln!("error: option `unified' expects a numerical value");
    }
    Err(GitError::Exit(129))
}

pub fn commit_validate_inter_hunk_context(value: &str) -> Result<()> {
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

pub fn patch_validate_inter_hunk_context(value: &str) -> Result<()> {
    commit_validate_inter_hunk_context(value)?;
    if git_count_value_is_negative(value) {
        eprintln!("fatal: '--inter-hunk-context' cannot be negative");
        return Err(GitError::Exit(128));
    }
    Ok(())
}

pub fn commit_inter_hunk_context_expects_numerical_value_error() -> Result<()> {
    eprintln!("error: option `inter-hunk-context' expects a numerical value");
    Err(GitError::Exit(129))
}

pub fn git_count_value_is_negative(value: &str) -> bool {
    let number = match value.as_bytes().last() {
        Some(b'k' | b'K' | b'm' | b'M' | b'g' | b'G') => &value[..value.len() - 1],
        _ => value,
    };
    number.trim_start().starts_with('-')
}

pub fn git_count_value_is_valid(value: &str) -> bool {
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

pub fn commit_tree_file_requires_value_error() -> Result<()> {
    eprintln!("error: switch `F' requires a value");
    Err(GitError::Exit(129))
}

pub fn read_commit_message_file(path: &str) -> Result<Vec<u8>> {
    if path == "-" {
        let mut message = Vec::new();
        io::stdin().read_to_end(&mut message)?;
        Ok(message)
    } else {
        Ok(fs::read(path)?)
    }
}

pub fn commit_message_from_prepared_chunks(chunks: &[Vec<u8>]) -> Vec<u8> {
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
pub fn commit_message_chunk_is_empty(chunk: &[u8]) -> bool {
    chunk.is_empty()
}

/// The resolved commit-message cleanup mode (git's `enum
/// commit_msg_cleanup_mode`). The raw `--cleanup`/`commit.cleanup` arg plus
/// whether an editor runs resolve to one of these via
/// [`resolve_commit_cleanup_mode`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommitCleanupMode {
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
/// rejected earlier by `validate_commit_cleanup_mode`, so we treat them as the
/// default here.
pub fn resolve_commit_cleanup_mode(arg: Option<&str>, use_editor: bool) -> CommitCleanupMode {
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
pub fn commit_cleanup_message(
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

pub fn commit_stripspace_message(message: &[u8], comment_char: Option<&str>) -> Vec<u8> {
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

pub fn commit_trim_trailing_space(line: &[u8]) -> &[u8] {
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
pub fn commit_locate_scissors(message: &[u8], comment_char: &str) -> usize {
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

/// Resolve `rev`, peel it to a commit, and parse it — the engine half of the
/// CLI's reused-commit lookup (`commit --reuse-message`/`--fixup` and friends).
///
/// Session-aware ODB opening (replacement policy from the invocation config)
/// stays on the CLI side; callers pass the already-opened database. Any failure
/// collapses to git's `fatal: could not lookup commit '<rev>'`.
pub fn read_reused_commit_from_db(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    rev: &str,
) -> Result<Commit> {
    let result = (|| {
        let oid = sley_rev::RevisionResolver::new(git_dir, format, db).resolve(rev)?;
        let commit_oid = sley_rev::peel_to_commit(db, format, &oid)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn unquote(s: &[u8]) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        unquote_c_style(s, &mut out).map(|_| out)
    }

    #[test]
    fn cleanup_mode_defaults_track_the_editor() {
        assert_eq!(
            resolve_commit_cleanup_mode(None, true),
            CommitCleanupMode::Strip
        );
        assert_eq!(
            resolve_commit_cleanup_mode(None, false),
            CommitCleanupMode::Whitespace
        );
        assert_eq!(
            resolve_commit_cleanup_mode(Some("default"), false),
            CommitCleanupMode::Whitespace
        );
        assert_eq!(
            resolve_commit_cleanup_mode(Some("verbatim"), false),
            CommitCleanupMode::Verbatim
        );
        assert_eq!(
            resolve_commit_cleanup_mode(Some("scissors"), true),
            CommitCleanupMode::Scissors
        );
        assert_eq!(
            resolve_commit_cleanup_mode(Some("scissors"), false),
            CommitCleanupMode::Whitespace
        );
        // Unknown values resolve to the default (validation happens earlier).
        assert_eq!(
            resolve_commit_cleanup_mode(Some("bogus"), false),
            CommitCleanupMode::Whitespace
        );
    }

    #[test]
    fn stripspace_squashes_blanks_and_drops_comments_only_when_asked() {
        // Trailing whitespace is stripped; leading whitespace is kept (git's
        // strbuf_stripspace semantics), blank runs squash to one separator.
        assert_eq!(
            commit_stripspace_message(b"\n  a  \n\n\nb\t\n", None),
            b"  a\n\nb\n".to_vec()
        );
        assert_eq!(
            commit_stripspace_message(b"# comment\nreal\n", Some("#")),
            b"real\n".to_vec()
        );
        assert_eq!(
            commit_stripspace_message(b"# comment\nreal\n", None),
            b"# comment\nreal\n".to_vec()
        );
        assert_eq!(commit_trim_trailing_space(b"a \t\r "), b"a");
    }

    #[test]
    fn cleanup_applies_scissors_and_verbose_truncation() {
        let msg = b"subject\n# ------------------------ >8 ------------------------\n# junk\n";
        assert_eq!(
            commit_cleanup_message(msg.to_vec(), CommitCleanupMode::Scissors, "#", false),
            b"subject\n".to_vec()
        );
        assert_eq!(
            commit_cleanup_message(b"subject\n".to_vec(), CommitCleanupMode::Strip, "#", true),
            b"subject\n".to_vec()
        );
        assert_eq!(
            commit_cleanup_message(b"subject\n".to_vec(), CommitCleanupMode::Verbatim, "#", false),
            b"subject\n".to_vec()
        );
    }

    #[test]
    fn scissors_cutoff_matches_wt_status_locate_end() {
        assert_eq!(commit_locate_scissors(b"# ------------------------ >8 ------------------------\nx", "#"), 0);
        assert_eq!(commit_locate_scissors(b"a\n# ------------------------ >8 ------------------------\nb", "#"), 2);
        assert_eq!(commit_locate_scissors(b"no cut here\n", "#"), b"no cut here\n".len());
    }

    #[test]
    fn git_counts_accept_kmg_suffixes_and_reject_garbage() {
        assert!(git_count_value_is_valid("12"));
        assert!(git_count_value_is_valid("+3k"));
        assert!(git_count_value_is_valid("-4M"));
        assert!(!git_count_value_is_valid("k"));
        assert!(!git_count_value_is_valid("1x"));
        assert!(!git_count_value_is_valid(""));
        assert!(git_count_value_is_negative("-2g"));
        assert!(!git_count_value_is_negative("2g"));
    }

    #[test]
    fn prepared_chunks_skip_only_fully_empty_entries() {
        assert_eq!(
            commit_message_from_prepared_chunks(&[Vec::new(), b"one".to_vec(), b"two".to_vec()]),
            b"one\ntwo".to_vec()
        );
        assert!(commit_message_chunk_is_empty(b""));
        // A lone newline is real content under verbatim (t6006).
        assert!(!commit_message_chunk_is_empty(b"\n"));
    }

    #[test]
    fn unquote_oracle_truth_table() {
        // Empirically verified against oracle git 2.55 (`unquote_c_style`,
        // quote.c); keep byte-identical with the sley-diff-merge mirror matrix.
        assert_eq!(unquote(br#""hello""#).as_deref(), Some(&b"hello"[..]));
        assert_eq!(unquote(br#""\123""#).as_deref(), Some(&b"S"[..]));
        assert_eq!(unquote(br#""\1234""#).as_deref(), Some(&b"S4"[..]));
        assert_eq!(unquote(br#""\377""#), Some(vec![0xff]));
        assert_eq!(unquote(br#""a\tb\n""#).as_deref(), Some(&b"a\tb\n"[..]));
        assert_eq!(unquote(br#""ma\zn""#), None);
        assert_eq!(unquote(br#""main"#), None);
        assert_eq!(unquote(b"plain"), None);
    }
}
