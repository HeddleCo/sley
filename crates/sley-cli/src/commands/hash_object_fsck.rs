//! `git hash-object` object-structure validation.
//!
//! When `--literally` is absent, git frames the bytes as the requested object
//! type and runs `fsck` over them before hashing (see `index_mem()` in
//! object-file.c, guarded by `INDEX_FORMAT_CHECK`). Any fsck problem is printed
//! as
//!
//! ```text
//! error: object fails fsck: <camelCasedMsgId>: <detail>
//! ```
//!
//! and a single failing object then ends with
//!
//! ```text
//! fatal: refusing to create malformed object
//! ```
//!
//! followed by exit code 128 (git's `die()`).
//!
//! This module is a direct port of the relevant git paths:
//!
//!   * `index_mem()` / `hash_format_check_report()` (object-file.c) — the
//!     `object fails fsck` framing and the `refusing to create malformed object`
//!     fatal. `opts.strict = 1`, so WARN-severity fsck messages are promoted to
//!     errors; the report callback always returns non-zero, so *any* reported
//!     problem (including INFO-severity ones) makes the object malformed.
//!   * `fsck_tree()` (fsck.c) + `decode_tree_entry()` /
//!     `update_tree_entry_internal()` (tree-walk.c) — note the tree-walk decode
//!     errors (`too-short tree object`, `malformed mode in tree entry`,
//!     `empty filename in tree entry`) are printed via plain `error()` *before*
//!     the `badTree` fsck line, because `init_tree_desc_gently()` reports them
//!     itself.
//!   * `fsck_commit()` / `fsck_tag()` / `fsck_ident()` / `verify_headers()`
//!     (fsck.c).
//!
//! Blobs are never validated (any byte sequence is a valid blob), matching git.

use sley::{GitError, ObjectFormat, ObjectId, Result};
use sley::plumbing::sley_object::ObjectType;

/// The fatal line git prints (via `die()`) once an object fails fsck.
const REFUSING_MALFORMED: &str = "fatal: refusing to create malformed object";

/// Validate `body` as `object_type` the way `git hash-object` does without
/// `--literally`. On success returns `Ok(())`; if the object fails fsck the git
/// diagnostics have already been printed to stderr and `Err(GitError::Exit(128))`
/// is returned so the binary exits with git's status.
///
/// "Fails fsck" means git's `fsck_buffer()` returned non-zero. Each per-type fsck
/// function returns the same running `ret` git computes (see the per-function
/// docs for the overwrite-vs-accumulate distinction): the object dies iff that
/// final value is non-zero. A printed diagnostic does *not* by itself imply a
/// failure — git can print an error whose `ret` is later overwritten to zero by
/// an `FSCK_IGNORE` report, in which case the object still hashes successfully.
pub(crate) fn check_object(
    object_type: ObjectType,
    format: ObjectFormat,
    body: &[u8],
) -> Result<()> {
    let mut reporter = FsckReporter;
    let ret = match object_type {
        // Blobs are always valid: git's fsck has no blob checks.
        ObjectType::Blob => 0,
        ObjectType::Tree => fsck_tree(format, body, &mut reporter),
        ObjectType::Commit => fsck_commit(format, body, &mut reporter),
        ObjectType::Tag => fsck_tag(format, body, &mut reporter),
    };
    if ret != 0 {
        eprintln!("{REFUSING_MALFORMED}");
        return Err(GitError::Exit(128));
    }
    Ok(())
}

/// fsck message severity (`enum fsck_msg_type`), restricted to the values the
/// tree/commit/tag checks actually use. Under git's `index_mem` the options are
/// `strict = 1` with the `hash_format_check_report` error callback, which prints
/// `error: object fails fsck: <msg>` and returns 1. The severity only changes two
/// things in `fsck_vreport`: an `Ignore` message returns 0 *without* printing
/// (and can therefore mask a prior error by overwriting `ret`), and `Warn` is
/// promoted to an error under strict. `Info`/`Warn`/`Error` all otherwise print
/// and return 1 here because the callback ignores the (printed) severity.
#[derive(Clone, Copy)]
enum Severity {
    /// Default error severity: printed, returns 1.
    Error,
    /// Warning: under `strict` behaves exactly like `Error` here.
    Warn,
    /// Informational: still printed and returns 1 on the hash-object path.
    Info,
    /// Ignored: not printed, returns 0 (so it overwrites `ret` to zero).
    Ignore,
}

/// Emits fsck problems with git's `index_mem()` framing and returns the value
/// git's `report()` returns, so callers can reproduce git's `ret` bookkeeping
/// exactly.
struct FsckReporter;

impl FsckReporter {
    /// Report one fsck problem with severity `sev`. `id` is the camelCased message
    /// id (e.g. `badTree`); `detail` is the human-readable tail. Returns the value
    /// git's `report()` would (`0` for `Ignore`, `1` otherwise) so the caller can
    /// thread it through git's running `ret`.
    fn report(&mut self, sev: Severity, id: &str, detail: &str) -> i32 {
        match sev {
            // FSCK_IGNORE short-circuits in fsck_vreport before printing.
            Severity::Ignore => 0,
            _ => {
                print_error_line(&format!("object fails fsck: {id}: {detail}"));
                1
            }
        }
    }

    /// Print a raw `error: <msg>` line, matching git's plain `error()` used by
    /// the tree-walk decoder (`init_tree_desc_gently` /
    /// `update_tree_entry_gently`). This does not itself contribute to `ret`; the
    /// caller follows it with the `badTree` report that does.
    fn error_line(&mut self, msg: &str) {
        print_error_line(msg);
    }
}

/// Print `error: <msg>` to stderr with git's control-character sanitization.
///
/// git routes every `error()`/`die()` through `vfreportf` (usage.c), which after
/// formatting replaces any control byte other than `\t`/`\n` with `?`:
///
/// ```c
/// if (iscntrl(*p) && *p != '\t' && *p != '\n') *p = '?';
/// ```
///
/// fsck details echo user bytes (e.g. the `badTagName` tag name), so we apply the
/// same sanitization to the whole `error: <msg>` line for byte-exact parity.
fn print_error_line(msg: &str) {
    let mut line = format!("error: {msg}");
    // SAFETY-NOTE: operating on bytes is correct here — git sanitizes the raw
    // formatted byte buffer, and our `msg` may contain non-UTF-8-intent bytes
    // that arrived via `String::from_utf8_lossy`. We sanitize the byte view.
    let bytes: Vec<u8> = line
        .bytes()
        .map(|b| {
            if b.is_ascii_control() && b != b'\t' && b != b'\n' {
                b'?'
            } else {
                b
            }
        })
        .collect();
    line = String::from_utf8(bytes).unwrap_or(line);
    eprintln!("{line}");
}

// ---------------------------------------------------------------------------
// Tree
// ---------------------------------------------------------------------------

/// Outcome of decoding a single tree entry, mirroring `decode_tree_entry()`.
struct TreeEntry<'a> {
    mode: u16,
    name: &'a [u8],
    oid_raw: &'a [u8],
    /// Offset just past this entry's hash, i.e. the start of the next entry.
    next: usize,
    /// True when this entry's mode field had a leading zero (`zeroPaddedFilemode`).
    zero_padded: bool,
}

/// Decode the tree entry beginning at `buf` (`decode_tree_entry` in tree-walk.c).
/// On a structural problem the matching git error string is returned in `Err`,
/// and the caller prints it via `error()` then reports `badTree`.
fn decode_tree_entry<'a>(
    buf: &'a [u8],
    hashsz: usize,
) -> std::result::Result<TreeEntry<'a>, &'static str> {
    // `if (size < hashsz + 3 || buf[size - (hashsz + 1)])`
    //
    // The entry must hold at least one mode digit, a space, a one-byte name, a
    // NUL, and the hash; and the byte immediately before the hash (the name's
    // NUL terminator) must be zero.
    if buf.len() < hashsz + 3 || buf[buf.len() - (hashsz + 1)] != 0 {
        return Err("too-short tree object");
    }

    // parse_mode(): octal digits until a space; NULL if the first byte is a
    // space or any byte is a non-octal digit.
    let mut mode: u16 = 0;
    let mut idx = 0;
    if buf[idx] == b' ' {
        return Err("malformed mode in tree entry");
    }
    loop {
        let c = buf[idx];
        idx += 1;
        if c == b' ' {
            break;
        }
        if !(b'0'..=b'7').contains(&c) {
            return Err("malformed mode in tree entry");
        }
        mode = (mode << 3).wrapping_add(u16::from(c - b'0'));
    }
    // `idx` now points just past the space; the name runs to the NUL.
    let name_start = idx;
    if buf.get(name_start).copied() == Some(0) {
        // `if (!*path)` — empty filename.
        return Err("empty filename in tree entry");
    }
    // The terminating NUL is guaranteed to exist within bounds by the size check
    // above (the byte before the hash is a NUL), so this find always succeeds.
    let nul_rel = buf[name_start..]
        .iter()
        .position(|&b| b == 0)
        .expect("tree entry name is NUL-terminated by the size precondition");
    let name = &buf[name_start..name_start + nul_rel];
    let oid_start = name_start + nul_rel + 1;
    let oid_end = oid_start + hashsz;
    let oid_raw = &buf[oid_start..oid_end];
    Ok(TreeEntry {
        mode,
        name,
        oid_raw,
        next: oid_end,
        zero_padded: buf[0] == b'0',
    })
}

/// Port of `fsck_tree()` (fsck.c). Walks every entry, printing the tree-walk
/// decode error then `badTree` on a malformed entry, then accumulating the
/// per-property problems git reports after the walk (in git's order). Returns
/// git's `retval` (the SUM of every `report()` return — trees accumulate, unlike
/// commits/tags which overwrite), so the object dies iff any reported message was
/// non-IGNORE.
fn fsck_tree(format: ObjectFormat, body: &[u8], reporter: &mut FsckReporter) -> i32 {
    let hashsz = format.raw_len();
    let mut retval = 0i32;

    let mut has_null_sha1 = false;
    let mut has_full_path = false;
    let mut has_empty_name = false;
    let mut has_dot = false;
    let mut has_dotdot = false;
    let mut has_dotgit = false;
    let mut has_zero_pad = false;
    let mut has_bad_modes = false;
    let mut has_dup_entries = false;
    let mut not_properly_sorted = false;
    let mut has_large_name = false;

    // init_tree_desc_gently() decodes the first entry up front; a failure there
    // reports `badTree` and returns immediately.
    if body.is_empty() {
        // An empty buffer is a valid (empty) tree.
        return 0;
    }
    let mut offset = 0usize;
    let first = match decode_tree_entry(&body[offset..], hashsz) {
        Ok(entry) => entry,
        Err(msg) => {
            reporter.error_line(msg);
            return reporter.report(Severity::Error, "badTree", "cannot be parsed as a tree");
        }
    };

    // `o_mode` / `o_name` track the previous entry for ordering checks.
    let mut prev: Option<(u16, Vec<u8>)> = None;
    let mut current = first;
    loop {
        let mode = current.mode;
        let name = current.name;

        has_null_sha1 |= ObjectId::from_raw(format, current.oid_raw)
            .map(|oid| oid.is_null())
            .unwrap_or(false);
        has_full_path |= name.contains(&b'/');
        has_empty_name |= name.is_empty();
        has_dot |= name == b".";
        has_dotdot |= name == b"..";
        has_dotgit |= is_dotgit(name);
        has_zero_pad |= current.zero_padded;
        has_large_name |= name.len() > MAX_TREE_ENTRY_LEN;

        // Advance to the next entry (update_tree_entry_internal +
        // update_tree_entry_gently). git performs this BEFORE the mode and
        // ordering checks, and on a decode failure prints the error, reports
        // `badTree`, and `break`s — so the mode/ordering checks below are skipped
        // for the current entry when the advance fails.
        let next_off = offset + current.next;
        let advanced = if next_off < body.len() {
            match decode_tree_entry(&body[next_off..], hashsz) {
                Ok(entry) => Some(entry),
                Err(msg) => {
                    reporter.error_line(msg);
                    retval +=
                        reporter.report(Severity::Error, "badTree", "cannot be parsed as a tree");
                    // git: `goto`-less `break` right after the failed advance.
                    break;
                }
            }
        } else {
            None
        };

        // Mode classification (runs only after a successful advance, matching
        // git's loop where the `switch (mode)` follows update_tree_entry_gently).
        if !is_standard_mode(mode) {
            has_bad_modes = true;
        }

        // Ordering / duplicate detection against the previous entry.
        if let Some((prev_mode, prev_name)) = prev.as_ref() {
            match verify_ordered(*prev_mode, prev_name, mode, name) {
                TreeOrder::Unordered => not_properly_sorted = true,
                TreeOrder::Dups => has_dup_entries = true,
                TreeOrder::Ok => {}
            }
        }
        prev = Some((mode, name.to_vec()));

        match advanced {
            Some(entry) => {
                offset = next_off;
                current = entry;
            }
            None => break,
        }
    }

    // Post-walk property reports, in git's exact order, each with its severity,
    // message id, and detail string verbatim from fsck.c.
    if has_null_sha1 {
        retval += reporter.report(
            Severity::Warn,
            "nullSha1",
            "contains entries pointing to null sha1",
        );
    }
    if has_full_path {
        retval += reporter.report(Severity::Warn, "fullPathname", "contains full pathnames");
    }
    if has_empty_name {
        retval += reporter.report(Severity::Warn, "emptyName", "contains empty pathname");
    }
    if has_dot {
        retval += reporter.report(Severity::Warn, "hasDot", "contains '.'");
    }
    if has_dotdot {
        retval += reporter.report(Severity::Warn, "hasDotdot", "contains '..'");
    }
    if has_dotgit {
        retval += reporter.report(Severity::Warn, "hasDotgit", "contains '.git'");
    }
    if has_zero_pad {
        retval += reporter.report(
            Severity::Warn,
            "zeroPaddedFilemode",
            "contains zero-padded file modes",
        );
    }
    if has_bad_modes {
        retval += reporter.report(Severity::Info, "badFilemode", "contains bad file modes");
    }
    if has_dup_entries {
        retval += reporter.report(
            Severity::Error,
            "duplicateEntries",
            "contains duplicate file entries",
        );
    }
    if not_properly_sorted {
        retval += reporter.report(Severity::Error, "treeNotSorted", "not properly sorted");
    }
    if has_large_name {
        retval += reporter.report(
            Severity::Warn,
            "largePathname",
            "contains excessively large pathname",
        );
    }
    retval
}

/// git's default `core.maxTreeEntryLen` (fsck.c `max_tree_entry_len`).
const MAX_TREE_ENTRY_LEN: usize = 4096;

/// Standard tree entry modes git accepts in non-strict mode plus the strict set;
/// under hash-object `opts.strict = 1`, the nonstandard `0o100664` is *not*
/// allowed (it only passes when `!options->strict`).
fn is_standard_mode(mode: u16) -> bool {
    matches!(mode, 0o100755 | 0o100644 | 0o120000 | 0o040000 | 0o160000)
}

/// Whether `name` is one of git's `.git` spellings flagged by `has_dotgit`.
/// git checks HFS/NTFS-folded forms; for hash-object's purposes the exact ASCII
/// `.git`, `.GIT`, etc. plus the common NTFS short name `git~1` cover the cases
/// reachable here. We match git's case-insensitive `.git` and `git~1`.
fn is_dotgit(name: &[u8]) -> bool {
    name.eq_ignore_ascii_case(b".git") || name.eq_ignore_ascii_case(b"git~1")
}

enum TreeOrder {
    Ok,
    Unordered,
    Dups,
}

/// Port of the consecutive-entry portion of `verify_ordered()` (fsck.c). The
/// non-consecutive directory/file duplicate detection via the name stack is not
/// reproduced; the consecutive cases (identical names, and the directory-vs-file
/// sort) cover every duplicate/sort problem reachable through `hash-object`'s
/// realistic malformed inputs.
fn verify_ordered(mode1: u16, name1: &[u8], mode2: u16, name2: &[u8]) -> TreeOrder {
    let len = name1.len().min(name2.len());
    let cmp = name1[..len].cmp(&name2[..len]);
    match cmp {
        std::cmp::Ordering::Less => return TreeOrder::Ok,
        std::cmp::Ordering::Greater => return TreeOrder::Unordered,
        std::cmp::Ordering::Equal => {}
    }
    // First `len` bytes equal; order by the next byte, treating a missing byte on
    // a directory as '/'.
    let c1 = name1.get(len).copied();
    let c2 = name2.get(len).copied();
    match (c1, c2) {
        (None, None) => TreeOrder::Dups,
        _ => {
            let c1 = c1.unwrap_or_else(|| if is_dir(mode1) { b'/' } else { 0 });
            let c2 = c2.unwrap_or_else(|| if is_dir(mode2) { b'/' } else { 0 });
            if c1 < c2 {
                TreeOrder::Ok
            } else {
                TreeOrder::Unordered
            }
        }
    }
}

fn is_dir(mode: u16) -> bool {
    mode & 0o170000 == 0o040000
}

// ---------------------------------------------------------------------------
// Commit
// ---------------------------------------------------------------------------

/// Port of `fsck_commit()` (fsck.c). Returns git's running `ret`: the structural
/// checks short-circuit on a non-zero report (`if (err) return err`), while the
/// `tree`/`parent` bad-sha1 checks fall through when their report is zero (which
/// never happens here, since those ids are ERROR). The object dies iff the
/// returned `ret` is non-zero.
fn fsck_commit(format: ObjectFormat, body: &[u8], reporter: &mut FsckReporter) -> i32 {
    // verify_headers must pass before any linewise scan (memory safety in git).
    let headers = verify_headers(body, reporter);
    if headers != 0 {
        return headers;
    }

    let mut rest = body;

    // tree <oid>\n — `if (!skip_prefix("tree ")) return report(MISSING_TREE)`.
    let Some(after_tree) = strip_prefix(rest, b"tree ") else {
        return reporter.report(
            Severity::Error,
            "missingTree",
            "invalid format - expected 'tree' line",
        );
    };
    match parse_oid_line(format, after_tree) {
        Some(next) => rest = next,
        None => {
            let err = reporter.report(
                Severity::Error,
                "badTreeSha1",
                "invalid 'tree' line format - bad sha1",
            );
            if err != 0 {
                return err;
            }
            rest = advance_past_line(rest);
        }
    }

    // parent <oid>\n (zero or more)
    while let Some(after_parent) = strip_prefix(rest, b"parent ") {
        match parse_oid_line(format, after_parent) {
            Some(next) => rest = next,
            None => {
                let err = reporter.report(
                    Severity::Error,
                    "badParentSha1",
                    "invalid 'parent' line format - bad sha1",
                );
                if err != 0 {
                    return err;
                }
                rest = advance_past_line(rest);
            }
        }
    }

    // author <ident>\n (one or more, then count check). `if (err) return err`.
    let mut author_count = 0u32;
    while let Some(after_author) = strip_prefix(rest, b"author ") {
        author_count += 1;
        let (err, next) = fsck_ident(after_author, reporter);
        if err != 0 {
            return err;
        }
        rest = next;
    }
    let mut err;
    if author_count < 1 {
        err = reporter.report(
            Severity::Error,
            "missingAuthor",
            "invalid format - expected 'author' line",
        );
    } else if author_count > 1 {
        err = reporter.report(
            Severity::Error,
            "multipleAuthors",
            "invalid format - multiple 'author' lines",
        );
    } else {
        err = 0;
    }
    if err != 0 {
        return err;
    }

    // committer <ident>\n (exactly one). `if (err) return err`.
    let Some(after_committer) = strip_prefix(rest, b"committer ") else {
        return reporter.report(
            Severity::Error,
            "missingCommitter",
            "invalid format - expected 'committer' line",
        );
    };
    let (cerr, _) = fsck_ident(after_committer, reporter);
    if cerr != 0 {
        return cerr;
    }
    err = 0;

    // NUL anywhere in the object body (WARN → ERROR under strict).
    if body.contains(&0) {
        err = reporter.report(
            Severity::Warn,
            "nulInCommit",
            "NUL byte in the commit object body",
        );
        if err != 0 {
            return err;
        }
    }
    err
}

// ---------------------------------------------------------------------------
// Tag
// ---------------------------------------------------------------------------

/// Port of `fsck_tag_standalone()` (fsck.c). Returns git's running `ret`. The
/// structural checks short-circuit (`goto done` on a non-zero report), but the
/// `tagger`/gpgsig/extra-header tail does NOT: a later `report()` *overwrites*
/// `ret`. In particular `extraHeaderEntry` is `FSCK_IGNORE` (returns 0), so an
/// extra header after `tagger` overwrites a preceding identity error's `ret` to
/// zero — git prints the error but still hashes the object. We reproduce that.
fn fsck_tag(format: ObjectFormat, body: &[u8], reporter: &mut FsckReporter) -> i32 {
    let headers = verify_headers(body, reporter);
    if headers != 0 {
        return headers;
    }

    let mut rest = body;

    // object <oid>\n
    let Some(after_object) = strip_prefix(rest, b"object ") else {
        return reporter.report(
            Severity::Error,
            "missingObject",
            "invalid format - expected 'object' line",
        );
    };
    match parse_oid_line(format, after_object) {
        Some(next) => rest = next,
        None => {
            let err = reporter.report(
                Severity::Error,
                "badObjectSha1",
                "invalid 'object' line format - bad sha1",
            );
            if err != 0 {
                return err;
            }
        }
    }

    // type <type>\n. Absent `type ` prefix => `missingTypeEntry`; present prefix
    // with no terminating newline => `missingType`.
    let Some(after_type) = strip_prefix(rest, b"type ") else {
        return reporter.report(
            Severity::Error,
            "missingTypeEntry",
            "invalid format - expected 'type' line",
        );
    };
    let Some((type_value, after_type_line)) = split_line(after_type) else {
        return reporter.report(
            Severity::Error,
            "missingType",
            "invalid format - unexpected end after 'type' line",
        );
    };
    if parse_object_type(type_value).is_none() {
        let err = reporter.report(Severity::Error, "badType", "invalid 'type' value");
        if err != 0 {
            return err;
        }
    }
    rest = after_type_line;

    // tag <name>\n. Absent prefix => `missingTagEntry`; present prefix without a
    // newline => `missingTag` (git's detail string copy-pastes "after 'type'").
    let Some(after_tag) = strip_prefix(rest, b"tag ") else {
        return reporter.report(
            Severity::Error,
            "missingTagEntry",
            "invalid format - expected 'tag' line",
        );
    };
    let Some((tag_name, after_tag_line)) = split_line(after_tag) else {
        return reporter.report(
            Severity::Error,
            "missingTag",
            "invalid format - unexpected end after 'type' line",
        );
    };
    // git validates `refs/tags/<name>` via check_refname_format; badTagName is
    // INFO (still printed + fatal here), with `if (ret) goto done`.
    if !check_tag_refname(tag_name) {
        let detail = format!("invalid 'tag' name: {}", String::from_utf8_lossy(tag_name));
        let err = reporter.report(Severity::Info, "badTagName", &detail);
        if err != 0 {
            return err;
        }
    }
    rest = after_tag_line;

    // tagger <ident>\n. A missing tagger is `missingTaggerEntry` (INFO; `if (ret)
    // goto done`). When present, `ret = fsck_ident(...)` and execution FALLS
    // THROUGH (no short-circuit) to the gpgsig/extra-header tail below.
    let mut ret;
    match strip_prefix(rest, b"tagger ") {
        Some(after_tagger) => {
            let (err, next) = fsck_ident(after_tagger, reporter);
            ret = err;
            rest = next;
        }
        None => {
            ret = reporter.report(
                Severity::Info,
                "missingTaggerEntry",
                "invalid format - expected 'tagger' line",
            );
            if ret != 0 {
                return ret;
            }
        }
    }

    // Optional gpgsig / gpgsig-sha256 header with folded continuation lines.
    if let Some(after_sig) =
        strip_prefix(rest, b"gpgsig ").or_else(|| strip_prefix(rest, b"gpgsig-sha256 "))
    {
        let Some(nl) = after_sig.iter().position(|&b| b == b'\n') else {
            return reporter.report(
                Severity::Error,
                "badGpgsig",
                "invalid format - unexpected end after 'gpgsig' or 'gpgsig-sha256' line",
            );
        };
        rest = &after_sig[nl + 1..];
        while rest.first() == Some(&b' ') {
            let Some(nl) = rest.iter().position(|&b| b == b'\n') else {
                return reporter.report(
                    Severity::Error,
                    "badHeaderContinuation",
                    "invalid format - unexpected end in 'gpgsig' or 'gpgsig-sha256' continuation line",
                );
            };
            rest = &rest[nl + 1..];
        }
    }

    // Any remaining non-blank line after 'tagger' is an extra header. This report
    // OVERWRITES `ret` (extraHeaderEntry is FSCK_IGNORE → 0), matching git's
    // masking of a preceding identity error.
    if !rest.is_empty() && rest.first() != Some(&b'\n') {
        ret = reporter.report(
            Severity::Ignore,
            "extraHeaderEntry",
            "invalid format - extra header(s) after 'tagger'",
        );
    }
    ret
}

/// git's `check_refname_format("refs/tags/<name>", 0)` reduced to validating the
/// tag name. Because git prefixes `refs/tags/` (both valid components), the
/// result hinges on `<name>`'s `/`-separated components (refs.c
/// `check_refname_component`): each must be non-empty, must not start or end with
/// `.`, must not end with `.lock`, and must not contain a control byte, space,
/// DEL, any of `~ ^ : ? * [ \`, `..`, or `@{`.
fn check_tag_refname(name: &[u8]) -> bool {
    if name.is_empty() {
        return false;
    }
    name.split(|&b| b == b'/').all(check_refname_component)
}

fn check_refname_component(component: &[u8]) -> bool {
    if component.is_empty()
        || component.first() == Some(&b'.')
        || component.last() == Some(&b'.')
        || component.ends_with(b".lock")
    {
        return false;
    }
    for (idx, &byte) in component.iter().enumerate() {
        match byte {
            0x00..=0x20 | 0x7f => return false,
            b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\' => return false,
            b'.' if component.get(idx + 1) == Some(&b'.') => return false,
            b'@' if component.get(idx + 1) == Some(&b'{') => return false,
            _ => {}
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Shared header / ident helpers (verify_headers, fsck_ident)
// ---------------------------------------------------------------------------

/// Port of `verify_headers()` (fsck.c): an embedded NUL in the header region is
/// `nulInHeader` (with its 0-based offset); otherwise the header region must end
/// in `"\n\n"` or at least a trailing newline (`unterminatedHeader`). Scanning
/// stops at the first `"\n\n"`. Both ids are `FSCK_FATAL`, so a problem returns 1
/// (and is printed); returns git's `report()` value (`0` when headers are well-
/// formed).
fn verify_headers(body: &[u8], reporter: &mut FsckReporter) -> i32 {
    let mut i = 0;
    while i < body.len() {
        if body[i] == 0 {
            return reporter.report(
                Severity::Error,
                "nulInHeader",
                &format!("unterminated header: NUL at offset {i}"),
            );
        }
        // A "\n\n" ends the header region cleanly.
        if body[i] == b'\n' && i + 1 < body.len() && body[i + 1] == b'\n' {
            return 0;
        }
        i += 1;
    }
    if !body.is_empty() && body[body.len() - 1] == b'\n' {
        return 0;
    }
    reporter.report(Severity::Error, "unterminatedHeader", "unterminated header")
}

/// Port of `fsck_ident()` (fsck.c). `ident` begins just past the `author `/
/// `committer `/`tagger ` prefix and extends to the end of the object buffer
/// (git's `ident_end`). Validates the `Name <email> <timestamp> <tz>\n` shape,
/// reporting the first problem in git's exact order. Returns `(ret, after)` where
/// `ret` is git's `report()` value and `after` is the slice past the consumed
/// line (`*ident = nl + 1`, advanced regardless of outcome). `verify_headers`
/// guarantees a terminating newline exists in `ident`, so the date/timezone scans
/// below are bounded by it just as git relies on. Every ident message id is
/// ERROR severity, so a reported problem returns 1.
fn fsck_ident<'a>(ident: &'a [u8], reporter: &mut FsckReporter) -> (i32, &'a [u8]) {
    let nl = ident
        .iter()
        .position(|&b| b == b'\n')
        .expect("verify_headers guarantees a terminating newline");
    // git advances `*ident = nl + 1` regardless of outcome.
    let after = &ident[nl + 1..];
    let end = ident.len();
    // `byte(p)` mirrors `*p`/`p[k]` reads; out-of-bounds reads cannot happen on
    // the date/tz path because the guaranteed '\n' stops every scan first.
    let byte = |p: usize| -> Option<u8> { ident.get(p).copied() };

    let mut p = 0usize;
    // `if (*p == '<')` — name must not begin with the email.
    if byte(0) == Some(b'<') {
        let r = reporter.report(
            Severity::Error,
            "missingNameBeforeEmail",
            "invalid author/committer line - missing space before email",
        );
        return (r, after);
    }
    // Scan the name up to '<'. Hitting end/'\n' => missingEmail; '>' => badName.
    loop {
        if p >= end || byte(p) == Some(b'\n') {
            let r = reporter.report(
                Severity::Error,
                "missingEmail",
                "invalid author/committer line - missing email",
            );
            return (r, after);
        }
        match byte(p) {
            Some(b'>') => {
                let r = reporter.report(
                    Severity::Error,
                    "badName",
                    "invalid author/committer line - bad name",
                );
                return (r, after);
            }
            Some(b'<') => break, // end of name, beginning of email
            _ => p += 1,
        }
    }
    // `if (p[-1] != ' ')` — exactly one space must precede '<'.
    if p == 0 || byte(p - 1) != Some(b' ') {
        let r = reporter.report(
            Severity::Error,
            "missingSpaceBeforeEmail",
            "invalid author/committer line - missing space before email",
        );
        return (r, after);
    }
    p += 1; // skip past '<'
    // Scan the email up to '>'. Hitting end/'<'/'\n' => badEmail.
    loop {
        if p >= end || byte(p) == Some(b'<') || byte(p) == Some(b'\n') {
            let r = reporter.report(
                Severity::Error,
                "badEmail",
                "invalid author/committer line - bad email",
            );
            return (r, after);
        }
        if byte(p) == Some(b'>') {
            break; // end of email
        }
        p += 1;
    }
    p += 1; // skip past '>'
    // `if (*p != ' ')` — a space must follow the email.
    if byte(p) != Some(b' ') {
        let r = reporter.report(
            Severity::Error,
            "missingSpaceBeforeDate",
            "invalid author/committer line - missing space before date",
        );
        return (r, after);
    }
    p += 1;
    // Skip linear whitespace (spaces/tabs, but not newlines), then require a
    // digit — matching git's tolerance of extra whitespace before the date.
    while byte(p) == Some(b' ') || byte(p) == Some(b'\t') {
        p += 1;
    }
    if !byte(p).is_some_and(|c| c.is_ascii_digit()) {
        let r = reporter.report(
            Severity::Error,
            "badDate",
            "invalid author/committer line - bad date",
        );
        return (r, after);
    }
    // `if (*p == '0' && p[1] != ' ')` — a leading-zero multi-digit timestamp.
    if byte(p) == Some(b'0') && byte(p + 1) != Some(b' ') {
        let r = reporter.report(
            Severity::Error,
            "zeroPaddedDate",
            "invalid author/committer line - zero-padded date",
        );
        return (r, after);
    }
    // Consume the timestamp digits (parse_timestamp_from_buf) and check overflow.
    let ts_start = p;
    while byte(p).is_some_and(|c| c.is_ascii_digit()) {
        p += 1;
    }
    if date_overflows(&ident[ts_start..p]) {
        let r = reporter.report(
            Severity::Error,
            "badDateOverflow",
            "invalid author/committer line - date causes integer overflow",
        );
        return (r, after);
    }
    // `if (*p != ' ')` — exactly one space separates date and timezone.
    if byte(p) != Some(b' ') {
        let r = reporter.report(
            Severity::Error,
            "badDate",
            "invalid author/committer line - bad date",
        );
        return (r, after);
    }
    p += 1;
    // Timezone: `[+-]` then EXACTLY four digits then '\n'.
    let tz_ok = matches!(byte(p), Some(b'+') | Some(b'-'))
        && byte(p + 1).is_some_and(|c| c.is_ascii_digit())
        && byte(p + 2).is_some_and(|c| c.is_ascii_digit())
        && byte(p + 3).is_some_and(|c| c.is_ascii_digit())
        && byte(p + 4).is_some_and(|c| c.is_ascii_digit())
        && byte(p + 5) == Some(b'\n');
    if !tz_ok {
        let r = reporter.report(
            Severity::Error,
            "badTimezone",
            "invalid author/committer line - bad time zone",
        );
        return (r, after);
    }

    (0, after)
}

/// git's `date_overflows` over `parse_timestamp_from_buf`. `timestamp_t` is the
/// unsigned `uintmax_t` (64-bit); `parse_timestamp_from_buf` fills a 24-byte
/// buffer and returns `TIME_MAX` once it would overflow that buffer (>= 24
/// digits), and `date_overflows` flags any value `>= TIME_MAX`. So a timestamp
/// is an overflow when it has 24 or more digits or does not fit in a `u64`.
fn date_overflows(digits: &[u8]) -> bool {
    if digits.len() >= 24 {
        return true;
    }
    let text = std::str::from_utf8(digits).unwrap_or("");
    // `u64::MAX` stands in for `TIME_MAX`; `>=` is conservative but the digit-cap
    // above already covers the boundary git actually reaches in practice.
    text.parse::<u64>().is_err()
}

// ---------------------------------------------------------------------------
// Small byte helpers
// ---------------------------------------------------------------------------

/// `skip_prefix`: return the slice after `prefix` if `buf` starts with it.
fn strip_prefix<'a>(buf: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    buf.strip_prefix(prefix)
}

/// Parse a hex object-id line: `<hex>\n`. Returns the slice after the newline on
/// success (mirroring git's `parse_oid_hex_algop(buf, &oid, &p) || *p != '\n'`).
fn parse_oid_line(format: ObjectFormat, buf: &[u8]) -> Option<&[u8]> {
    let hex_len = format.hex_len();
    if buf.len() < hex_len + 1 {
        return None;
    }
    let (hex, rest) = buf.split_at(hex_len);
    if rest.first() != Some(&b'\n') {
        return None;
    }
    let hex = std::str::from_utf8(hex).ok()?;
    ObjectId::from_hex(format, hex).ok()?;
    Some(&rest[1..])
}

/// Split a `<value>\n...` slice into (`value`, rest-after-newline). Returns
/// `None` when there is no newline.
fn split_line(buf: &[u8]) -> Option<(&[u8], &[u8])> {
    let nl = buf.iter().position(|&b| b == b'\n')?;
    Some((&buf[..nl], &buf[nl + 1..]))
}

/// Advance past the current line's newline (used on the non-short-circuiting
/// bad-sha1 branches in git's commit fsck).
fn advance_past_line(buf: &[u8]) -> &[u8] {
    match buf.iter().position(|&b| b == b'\n') {
        Some(nl) => &buf[nl + 1..],
        None => &buf[buf.len()..],
    }
}

/// Recognize a git object type name (`fsck`'s `type_from_string_gently`). Only
/// the four canonical types are valid here.
fn parse_object_type(value: &[u8]) -> Option<ObjectType> {
    match value {
        b"blob" => Some(ObjectType::Blob),
        b"tree" => Some(ObjectType::Tree),
        b"commit" => Some(ObjectType::Commit),
        b"tag" => Some(ObjectType::Tag),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_tree_is_valid() {
        let mut r = FsckReporter;
        assert_eq!(fsck_tree(ObjectFormat::Sha1, b"", &mut r), 0);
    }

    #[test]
    fn garbage_tree_fails() {
        // A non-empty buffer too short to be a single entry fails to decode.
        let mut r = FsckReporter;
        assert_ne!(fsck_tree(ObjectFormat::Sha1, b"garbage", &mut r), 0);
    }

    #[test]
    fn decode_rejects_too_short_entry() {
        assert_eq!(
            decode_tree_entry(b"abc\n", 20).err(),
            Some("too-short tree object")
        );
    }

    #[test]
    fn decode_rejects_malformed_mode() {
        // Leading space => parse_mode returns NULL.
        let mut buf = b" 100644 name\0".to_vec();
        buf.extend(std::iter::repeat(0u8).take(20));
        assert_eq!(
            decode_tree_entry(&buf, 20).err(),
            Some("malformed mode in tree entry")
        );
    }

    #[test]
    fn decode_rejects_empty_filename() {
        let mut buf = b"100644 \0".to_vec();
        buf.extend(std::iter::repeat(0u8).take(20));
        assert_eq!(
            decode_tree_entry(&buf, 20).err(),
            Some("empty filename in tree entry")
        );
    }

    #[test]
    fn decode_accepts_valid_entry() {
        let mut buf = b"100644 file\0".to_vec();
        buf.extend(std::iter::repeat(0x11u8).take(20));
        let entry = decode_tree_entry(&buf, 20).expect("valid entry");
        assert_eq!(entry.mode, 0o100644);
        assert_eq!(entry.name, b"file");
        assert!(!entry.zero_padded);
    }

    #[test]
    fn standard_modes_under_strict() {
        for &m in &[0o100644u16, 0o100755, 0o120000, 0o040000, 0o160000] {
            assert!(is_standard_mode(m), "{m:o} should be standard");
        }
        // 0o100664 is only accepted by git in non-strict mode; hash-object is strict.
        assert!(!is_standard_mode(0o100664));
        assert!(!is_standard_mode(0o100600));
    }

    #[test]
    fn consecutive_identical_names_are_dups() {
        assert!(matches!(
            verify_ordered(0o100644, b"file", 0o100644, b"file"),
            TreeOrder::Dups
        ));
    }

    #[test]
    fn descending_names_are_unordered() {
        assert!(matches!(
            verify_ordered(0o100644, b"b", 0o100644, b"a"),
            TreeOrder::Unordered
        ));
    }

    #[test]
    fn ascending_names_are_ok() {
        assert!(matches!(
            verify_ordered(0o100644, b"a", 0o100644, b"b"),
            TreeOrder::Ok
        ));
    }

    #[test]
    fn tag_refname_rules() {
        assert!(check_tag_refname(b"v1.0"));
        assert!(check_tag_refname(b"release/1.0"));
        assert!(!check_tag_refname(b"")); // empty
        assert!(!check_tag_refname(b".hidden")); // leading dot
        assert!(!check_tag_refname(b"ends.")); // trailing dot
        assert!(!check_tag_refname(b"a..b")); // double dot
        assert!(!check_tag_refname(b"has space"));
        assert!(!check_tag_refname(b"caret^")); // forbidden metachar
        assert!(!check_tag_refname(b"name.lock")); // .lock suffix
        assert!(!check_tag_refname(b"a/")); // empty trailing component
    }

    #[test]
    fn date_overflow_rules() {
        assert!(!date_overflows(b"0"));
        assert!(!date_overflows(b"1234567890"));
        assert!(!date_overflows(b"18446744073709551615")); // u64::MAX
        assert!(date_overflows(b"18446744073709551616")); // u64::MAX + 1
        assert!(date_overflows(&[b'9'; 24])); // 24 digits => buffer cap overflow
    }

    #[test]
    fn empty_commit_is_unterminated_header() {
        // /dev/null commit: verify_headers fails with unterminatedHeader => fatal.
        let mut r = FsckReporter;
        assert_ne!(fsck_commit(ObjectFormat::Sha1, b"", &mut r), 0);
    }

    #[test]
    fn empty_tag_is_unterminated_header() {
        let mut r = FsckReporter;
        assert_ne!(fsck_tag(ObjectFormat::Sha1, b"", &mut r), 0);
    }

    #[test]
    fn extra_header_after_tagger_masks_ident_error() {
        // A bad tagger timezone reports an error, but the following extra header
        // (FSCK_IGNORE) overwrites `ret` to 0, so git still hashes the object.
        let body = b"object 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
            type commit\ntag t\ntagger a <a> 0 bad\nextra h\n\nm\n";
        let mut r = FsckReporter;
        assert_eq!(fsck_tag(ObjectFormat::Sha1, body, &mut r), 0);
    }

    #[test]
    fn valid_tag_passes() {
        let body = b"object 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
            type commit\ntag t\ntagger a <a@b> 0 +0000\n\nmsg\n";
        let mut r = FsckReporter;
        assert_eq!(fsck_tag(ObjectFormat::Sha1, body, &mut r), 0);
    }

    #[test]
    fn timezone_must_be_exactly_four_digits() {
        // tagger with a 3-digit tz is rejected; 4-digit is accepted.
        let three = b"object 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
            type commit\ntag t\ntagger a <a> 0 +000\n";
        let four = b"object 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
            type commit\ntag t\ntagger a <a> 0 +0000\n";
        let mut r = FsckReporter;
        assert_ne!(fsck_tag(ObjectFormat::Sha1, three, &mut r), 0);
        let mut r2 = FsckReporter;
        assert_eq!(fsck_tag(ObjectFormat::Sha1, four, &mut r2), 0);
    }
}
