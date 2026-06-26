//! `git refs verify` — ref-store consistency check (the fsck for refs).
//!
//! Mirrors upstream git's `refs_fsck` dispatch (`refs.c`) and the per-backend
//! `files_fsck` / `packed_fsck` (`refs/files-backend.c`, `refs/packed-backend.c`)
//! plus the reftable table-name check (`refs/reftable-backend.c`). The same
//! engine backs `git fsck --references`.

use crate::*;
use sley_fsck::SeverityConfig;
use sley_fsck::content::{MsgId, Severity};
use std::fs;
use std::path::{Component, Path, PathBuf};

/// Resolved options for a verify run (severity table + verbosity).
pub(crate) struct RefsVerifyOptions {
    severity: SeverityConfig,
    verbose: bool,
}

impl RefsVerifyOptions {
    /// Build from the repository config (folding in command-line `-c
    /// fsck.<id>=<sev>` via `GIT_CONFIG_PARAMETERS`) plus the `--strict` flag.
    pub(crate) fn from_repo(git_dir: &Path, strict: bool, verbose: bool) -> Self {
        let mut severity = SeverityConfig::new(strict);
        if let Ok(config) = read_repo_config(git_dir) {
            for (key, value) in config.fsck_entries() {
                severity.set(&key, &value);
            }
        }
        Self { severity, verbose }
    }

    /// Emit one finding and report whether it counted as an *error* (warnings
    /// and ignored ids return `false`). Matches git's `fsck_report_ref`, whose
    /// return value is non-zero only for `FSCK_ERROR`.
    fn report(&self, path: &str, msg_id: MsgId, message: &str) -> bool {
        match self.severity.resolve(msg_id) {
            Severity::Ignore => false,
            Severity::Warn => {
                eprintln!("warning: {path}: {}: {message}", msg_id.camel());
                false
            }
            Severity::Error => {
                eprintln!("error: {path}: {}: {message}", msg_id.camel());
                true
            }
        }
    }
}

/// A single worktree to verify (the main worktree plus every linked one).
struct WorktreeCtx {
    /// The worktree's own git dir (`<common>` for main, `<common>/worktrees/<id>`
    /// for a linked worktree).
    gitdir: PathBuf,
    /// `worktrees/<id>/` prefix for refnames, empty for the main worktree.
    prefix: String,
    is_main: bool,
}

/// Enumerate every worktree, main first then linked ones sorted by id. Mirrors
/// `get_worktrees_without_reading_head()`: verify always covers all worktrees
/// regardless of the current directory.
fn list_worktrees(common_dir: &Path) -> Vec<WorktreeCtx> {
    let mut out = vec![WorktreeCtx {
        gitdir: common_dir.to_path_buf(),
        prefix: String::new(),
        is_main: true,
    }];
    if let Ok(entries) = fs::read_dir(common_dir.join("worktrees")) {
        let mut linked: Vec<PathBuf> = entries
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.is_dir())
            .collect();
        linked.sort();
        for path in linked {
            let Some(id) = path
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_string)
            else {
                continue;
            };
            out.push(WorktreeCtx {
                gitdir: path,
                prefix: format!("worktrees/{id}/"),
                is_main: false,
            });
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Root-ref classification (refs.c is_root_ref / is_pseudo_ref).
// ---------------------------------------------------------------------------

fn is_root_ref_syntax(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b == b'-' || b == b'_')
}

fn is_pseudo_ref(name: &str) -> bool {
    matches!(name, "FETCH_HEAD" | "MERGE_HEAD")
}

fn is_root_ref(name: &str) -> bool {
    if !is_root_ref_syntax(name) || is_pseudo_ref(name) {
        return false;
    }
    if name.ends_with("_HEAD") {
        return true;
    }
    matches!(
        name,
        "HEAD"
            | "AUTO_MERGE"
            | "BISECT_EXPECTED_REV"
            | "NOTES_MERGE_PARTIAL"
            | "NOTES_MERGE_REF"
            | "MERGE_AUTOSTASH"
    )
}

/// Strip a leading `worktrees/<id>/` segment (refs.c `parse_worktree_ref`),
/// yielding the stripped refname used for HEAD classification.
fn strip_worktree_prefix(refname: &str) -> &str {
    if let Some(rest) = refname.strip_prefix("worktrees/") {
        if let Some((_id, tail)) = rest.split_once('/') {
            return tail;
        }
    }
    refname
}

// ---------------------------------------------------------------------------
// Symref-target check (refs.c refs_fsck_symref).
// ---------------------------------------------------------------------------

fn refs_fsck_symref(opts: &RefsVerifyOptions, refname: &str, target: &str) -> bool {
    let stripped = strip_worktree_prefix(refname);

    if stripped == "HEAD"
        && !target.starts_with("refs/heads/")
        && opts.report(
            refname,
            MsgId::BadHeadTarget,
            &format!("HEAD points to non-branch '{target}'"),
        )
    {
        return true;
    }

    if is_root_ref(target) {
        return false;
    }

    if check_refname_format(target, false).is_err()
        && opts.report(
            refname,
            MsgId::BadReferentName,
            &format!("points to invalid refname '{target}'"),
        )
    {
        return true;
    }

    if !target.starts_with("refs/")
        && !target.starts_with("worktrees/")
        && opts.report(
            refname,
            MsgId::SymrefTargetIsNotARef,
            &format!("points to non-ref target '{target}'"),
        )
    {
        return true;
    }

    false
}

/// `files_fsck_symref_target`: trailing-content checks (for the textual symref
/// case) then the target validity check.
fn files_fsck_symref_target(
    opts: &RefsVerifyOptions,
    refname: &str,
    referent: &str,
    symbolic_link: bool,
) -> bool {
    let mut had_error = false;
    let target;
    if symbolic_link {
        target = referent;
    } else {
        let orig_len = referent.len();
        let trimmed = referent.trim_end_matches(is_c_space);
        let trimmed_len = trimmed.len();
        let orig_last = referent.as_bytes().last().copied().unwrap_or(0);

        if trimmed_len == orig_len || (trimmed_len < orig_len && orig_last != b'\n') {
            had_error |= opts.report(refname, MsgId::RefMissingNewline, "misses LF at the end");
        }
        if trimmed_len != orig_len && trimmed_len + 1 != orig_len {
            had_error |= opts.report(
                refname,
                MsgId::TrailingRefContent,
                "has trailing whitespaces or newlines",
            );
        }
        target = trimmed;
    }
    had_error |= refs_fsck_symref(opts, refname, target);
    had_error
}

/// git's `isspace`: space, tab, newline, vertical tab, form feed, carriage
/// return (used by `strbuf_rtrim`).
fn is_c_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\x0b' | '\x0c' | '\r')
}

fn is_c_space_byte(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

// ---------------------------------------------------------------------------
// Loose-ref content parse (files-backend.c parse_loose_ref_contents).
// ---------------------------------------------------------------------------

enum LooseRef {
    /// Symbolic ref: the referent string after `ref:` and the leading
    /// whitespace, including any trailing whitespace/newline.
    Symref(String),
    /// Direct ref: the object id plus the trailing bytes after the hex digits.
    Oid(ObjectId, String),
    /// Unparseable content.
    Broken,
}

fn parse_loose_ref_contents(format: ObjectFormat, buf: &[u8]) -> LooseRef {
    if let Some(rest) = buf.strip_prefix(b"ref:") {
        let mut start = 0;
        while start < rest.len() && is_c_space_byte(rest[start]) {
            start += 1;
        }
        let referent = String::from_utf8_lossy(&rest[start..]).into_owned();
        return LooseRef::Symref(referent);
    }

    let hexsz = format.hex_len();
    match parse_oid_prefix(format, buf, hexsz) {
        Some(oid) => {
            let after = &buf[hexsz..];
            if after.is_empty() || is_c_space_byte(after[0]) {
                LooseRef::Oid(oid, String::from_utf8_lossy(after).into_owned())
            } else {
                LooseRef::Broken
            }
        }
        None => LooseRef::Broken,
    }
}

/// Parse exactly `hexsz` hex digits at the start of `buf` into an object id.
fn parse_oid_prefix(format: ObjectFormat, buf: &[u8], hexsz: usize) -> Option<ObjectId> {
    if buf.len() < hexsz {
        return None;
    }
    let hex = std::str::from_utf8(&buf[..hexsz]).ok()?;
    if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    ObjectId::from_hex(format, hex).ok()
}

// ---------------------------------------------------------------------------
// Per-ref checks (files-backend.c files_fsck_refs_name / files_fsck_refs_content).
// ---------------------------------------------------------------------------

fn files_fsck_refs_name(opts: &RefsVerifyOptions, refname: &str, path: &Path) -> bool {
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    // Lock files are ignored, but a bare ".lock" is not.
    if !filename.starts_with('.') && filename.ends_with(".lock") {
        return false;
    }
    if is_root_ref(refname) {
        return false;
    }
    if check_refname_format(refname, false).is_err() {
        return opts.report(refname, MsgId::BadRefName, "invalid refname format");
    }
    false
}

fn files_fsck_refs_content(
    opts: &RefsVerifyOptions,
    format: ObjectFormat,
    common_dir: &Path,
    refname: &str,
    path: &Path,
    is_symlink: bool,
) -> bool {
    if is_symlink {
        let mut had_error = opts.report(
            refname,
            MsgId::SymlinkRef,
            "use deprecated symbolic link for symref",
        );
        let referent = resolve_symlink_referent(common_dir, path);
        had_error |= files_fsck_symref_target(opts, refname, &referent, true);
        return had_error;
    }

    let content = match fs::read(path) {
        Ok(content) => content,
        // Concurrent removal: ignore, like git.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return false,
        Err(_) => return false,
    };

    match parse_loose_ref_contents(format, &content) {
        LooseRef::Symref(referent) => files_fsck_symref_target(opts, refname, &referent, false),
        LooseRef::Broken => {
            let trimmed = String::from_utf8_lossy(&content);
            let trimmed = trimmed.trim_end_matches(is_c_space);
            opts.report(refname, MsgId::BadRefContent, trimmed)
        }
        LooseRef::Oid(oid, trailing) => {
            if trailing.is_empty() {
                opts.report(refname, MsgId::RefMissingNewline, "misses LF at the end")
            } else if trailing != "\n" {
                opts.report(
                    refname,
                    MsgId::TrailingRefContent,
                    &format!("has trailing garbage: '{trailing}'"),
                )
            } else if oid.is_null() {
                opts.report(
                    refname,
                    MsgId::BadRefOid,
                    &format!("points to invalid object ID '{oid}'"),
                )
            } else {
                false
            }
        }
    }
}

fn files_fsck_ref(
    opts: &RefsVerifyOptions,
    format: ObjectFormat,
    common_dir: &Path,
    refname: &str,
    path: &Path,
    file_type: &fs::FileType,
) -> bool {
    if opts.verbose {
        eprintln!("Checking {refname}");
    }
    let is_symlink = file_type.is_symlink();
    if !file_type.is_file() && !is_symlink {
        return opts.report(refname, MsgId::BadRefFiletype, "unexpected file type");
    }
    let mut had_error = files_fsck_refs_name(opts, refname, path);
    had_error |= files_fsck_refs_content(opts, format, common_dir, refname, path, is_symlink);
    had_error
}

/// Resolve a symref symbolic link to its referent, relative to the common git
/// dir when the target lands inside it (files-backend.c symlink branch).
fn resolve_symlink_referent(common_dir: &Path, link_path: &Path) -> String {
    let abs_gitdir = fs::canonicalize(common_dir).unwrap_or_else(|_| common_dir.to_path_buf());
    let Ok(target) = fs::read_link(link_path) else {
        return String::new();
    };
    let joined = if target.is_absolute() {
        target
    } else {
        let parent = link_path.parent().unwrap_or_else(|| Path::new("."));
        let parent = fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
        parent.join(target)
    };
    let normalized = lexical_normalize(&joined);
    match normalized.strip_prefix(&abs_gitdir) {
        Ok(rel) => rel.to_string_lossy().into_owned(),
        Err(_) => normalized.to_string_lossy().into_owned(),
    }
}

/// Lexically resolve `.`/`..` components without touching the filesystem.
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Recursively collect ref files under `<gitdir>/refs`, returning
/// `(refname, path, file_type)` tuples sorted by refname.
fn collect_ref_files(gitdir: &Path, prefix: &str) -> Vec<(String, PathBuf, fs::FileType)> {
    let refs_dir = gitdir.join("refs");
    let mut out = Vec::new();
    walk_ref_dir(&refs_dir, &refs_dir, prefix, &mut out);
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn walk_ref_dir(
    base: &Path,
    dir: &Path,
    prefix: &str,
    out: &mut Vec<(String, PathBuf, fs::FileType)>,
) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let file_type = meta.file_type();
        if file_type.is_dir() {
            walk_ref_dir(base, &path, prefix, out);
            continue;
        }
        let Ok(relative) = path.strip_prefix(base) else {
            continue;
        };
        let refname = format!("{prefix}refs/{}", relative.to_string_lossy());
        out.push((refname, path, file_type));
    }
}

fn files_fsck_refs_dir(
    opts: &RefsVerifyOptions,
    format: ObjectFormat,
    common_dir: &Path,
    wt: &WorktreeCtx,
) -> bool {
    let mut had_error = false;
    for (refname, path, file_type) in collect_ref_files(&wt.gitdir, &wt.prefix) {
        had_error |= files_fsck_ref(opts, format, common_dir, &refname, &path, &file_type);
    }
    had_error
}

/// `for_each_root_ref` + `files_fsck_root_ref`: check the loose root refs
/// (`HEAD`, `*_HEAD`, …) living directly in the worktree git dir.
fn files_fsck_root_refs(
    opts: &RefsVerifyOptions,
    format: ObjectFormat,
    common_dir: &Path,
    wt: &WorktreeCtx,
) -> bool {
    let mut had_error = false;
    let Ok(entries) = fs::read_dir(&wt.gitdir) else {
        return false;
    };
    let mut roots: Vec<(String, PathBuf, fs::FileType)> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with('.') || name.ends_with(".lock") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.file_type().is_file() || !is_root_ref(name) {
            continue;
        }
        let refname = format!("{}{name}", wt.prefix);
        roots.push((refname, entry.path(), meta.file_type()));
    }
    roots.sort_by(|a, b| a.0.cmp(&b.0));
    for (refname, path, file_type) in roots {
        had_error |= files_fsck_ref(opts, format, common_dir, &refname, &path, &file_type);
    }
    had_error
}

// ---------------------------------------------------------------------------
// packed-refs checks (packed-backend.c packed_fsck*).
// ---------------------------------------------------------------------------

fn packed_fsck(opts: &RefsVerifyOptions, format: ObjectFormat, common_dir: &Path) -> bool {
    let path = common_dir.join("packed-refs");
    let meta = match fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return false,
        Err(_) => return false,
    };
    let file_type = meta.file_type();
    if file_type.is_symlink() {
        return opts.report(
            "packed-refs",
            MsgId::BadRefFiletype,
            "not a regular file but a symlink",
        );
    }
    if !file_type.is_file() {
        return opts.report("packed-refs", MsgId::BadRefFiletype, "not a regular file");
    }
    let content = match fs::read(&path) {
        Ok(content) => content,
        Err(_) => return false,
    };
    if content.is_empty() {
        return opts.report("packed-refs", MsgId::EmptyPackedRefsFile, "file is empty");
    }

    let mut sorted = false;
    let mut had_error = packed_fsck_content(opts, format, &content, &mut sorted);
    if !had_error && sorted {
        had_error |= packed_fsck_sorted(opts, format, &content);
    }
    had_error
}

/// Return `(end_of_line_index, terminated)` for the line starting at `pos`.
fn find_eol(buf: &[u8], pos: usize) -> (usize, bool) {
    match buf[pos..].iter().position(|&b| b == b'\n') {
        Some(rel) => (pos + rel, true),
        None => (buf.len(), false),
    }
}

fn packed_fsck_not_terminated(opts: &RefsVerifyOptions, line_number: u64, line: &[u8]) -> bool {
    opts.report(
        &format!("packed-refs line {line_number}"),
        MsgId::PackedRefEntryNotTerminated,
        &format!(
            "'{}' is not terminated with a newline",
            String::from_utf8_lossy(line)
        ),
    )
}

fn packed_fsck_content(
    opts: &RefsVerifyOptions,
    format: ObjectFormat,
    buf: &[u8],
    sorted: &mut bool,
) -> bool {
    let eof = buf.len();
    let mut had_error = false;
    let mut pos = 0;
    let mut line_number: u64 = 1;

    let (eol, terminated) = find_eol(buf, pos);
    if !terminated {
        had_error |= packed_fsck_not_terminated(opts, line_number, &buf[pos..eol]);
    }
    if buf.first() == Some(&b'#') {
        had_error |= packed_fsck_header(opts, &buf[pos..eol], sorted);
        pos = eol + 1;
        line_number += 1;
    }

    while pos < eof {
        let (eol, terminated) = find_eol(buf, pos);
        if !terminated {
            had_error |= packed_fsck_not_terminated(opts, line_number, &buf[pos..eol]);
        }
        had_error |= packed_fsck_main_line(opts, format, line_number, &buf[pos..eol]);
        pos = eol + 1;
        line_number += 1;

        if pos < eof && buf[pos] == b'^' {
            let (eol, terminated) = find_eol(buf, pos);
            if !terminated {
                had_error |= packed_fsck_not_terminated(opts, line_number, &buf[pos..eol]);
            }
            had_error |= packed_fsck_peeled_line(opts, format, line_number, &buf[pos..eol]);
            pos = eol + 1;
            line_number += 1;
        }
    }

    had_error
}

fn packed_fsck_header(opts: &RefsVerifyOptions, line: &[u8], sorted: &mut bool) -> bool {
    let text = String::from_utf8_lossy(line);
    if let Some(rest) = text.strip_prefix("# pack-refs with: ") {
        *sorted = rest.split(' ').any(|trait_name| trait_name == "sorted");
        false
    } else {
        opts.report(
            "packed-refs.header",
            MsgId::BadPackedRefHeader,
            &format!("'{text}' does not start with '# pack-refs with: '"),
        )
    }
}

fn packed_fsck_main_line(
    opts: &RefsVerifyOptions,
    format: ObjectFormat,
    line_number: u64,
    line: &[u8],
) -> bool {
    let path = format!("packed-refs line {line_number}");
    let hexsz = format.hex_len();
    let Some(oid) = parse_oid_prefix(format, line, hexsz) else {
        return opts.report(
            &path,
            MsgId::BadPackedRefEntry,
            &format!("'{}' has invalid oid", String::from_utf8_lossy(line)),
        );
    };
    let after = &line[hexsz..];
    if after.is_empty() || !is_c_space_byte(after[0]) {
        return opts.report(
            &path,
            MsgId::BadPackedRefEntry,
            &format!(
                "has no space after oid '{oid}' but with '{}'",
                String::from_utf8_lossy(after)
            ),
        );
    }
    let refname = String::from_utf8_lossy(&after[1..]).into_owned();
    let mut had_error = false;
    if refname.contains('\0') {
        had_error |= opts.report(
            &path,
            MsgId::BadPackedRefEntry,
            &format!("refname '{refname}' contains NULL binaries"),
        );
    }
    if check_refname_format(&refname, false).is_err() {
        had_error |= opts.report(
            &path,
            MsgId::BadRefName,
            &format!("has bad refname '{refname}'"),
        );
    }
    had_error
}

fn packed_fsck_peeled_line(
    opts: &RefsVerifyOptions,
    format: ObjectFormat,
    line_number: u64,
    line: &[u8],
) -> bool {
    let path = format!("packed-refs line {line_number}");
    let hexsz = format.hex_len();
    // Skip the leading '^'.
    let rest = &line[1..];
    if parse_oid_prefix(format, rest, hexsz).is_none() {
        return opts.report(
            &path,
            MsgId::BadPackedRefEntry,
            &format!("'{}' has invalid peeled oid", String::from_utf8_lossy(rest)),
        );
    }
    if rest.len() != hexsz {
        return opts.report(
            &path,
            MsgId::BadPackedRefEntry,
            &format!(
                "has trailing garbage after peeled oid '{}'",
                String::from_utf8_lossy(&rest[hexsz..])
            ),
        );
    }
    false
}

fn packed_fsck_sorted(opts: &RefsVerifyOptions, format: ObjectFormat, buf: &[u8]) -> bool {
    let hexsz = format.hex_len();
    let eof = buf.len();
    let mut pos = 0;
    let mut line_number: u64 = 1;
    let mut former: Option<String> = None;

    if buf.first() == Some(&b'#') {
        let (eol, _) = find_eol(buf, pos);
        pos = eol + 1;
        line_number += 1;
    }

    while pos < eof {
        let (eol, _) = find_eol(buf, pos);
        if buf[pos] == b'^' {
            pos = eol + 1;
            line_number += 1;
            continue;
        }
        let refname_start = pos + hexsz + 1;
        let current = if refname_start <= eol {
            String::from_utf8_lossy(&buf[refname_start..eol]).into_owned()
        } else {
            String::new()
        };
        if let Some(prev) = &former {
            if prev.as_bytes() >= current.as_bytes() {
                return opts.report(
                    &format!("packed-refs line {line_number}"),
                    MsgId::PackedRefUnsorted,
                    &format!("refname '{current}' is less than previous refname '{prev}'"),
                );
            }
        }
        former = Some(current);
        pos = eol + 1;
        line_number += 1;
    }

    false
}

// ---------------------------------------------------------------------------
// reftable table-name check (reftable-backend.c reftable_be_fsck).
// ---------------------------------------------------------------------------

/// A valid reftable table name is `0x%012x-0x%012x-%08x.ref` (two 12-hex
/// update-index bounds, an 8-hex random suffix, and the `.ref` extension).
fn is_valid_reftable_name(name: &str) -> bool {
    let Some(rest) = name.strip_suffix(".ref") else {
        return false;
    };
    let parts: Vec<&str> = rest.split('-').collect();
    if parts.len() != 3 {
        return false;
    }
    let valid_hex = |s: &str, prefix: bool, len: usize| -> bool {
        let body = if prefix {
            match s.strip_prefix("0x") {
                Some(body) => body,
                None => return false,
            }
        } else {
            s
        };
        body.len() == len && body.bytes().all(|b| b.is_ascii_hexdigit())
    };
    valid_hex(parts[0], true, 12) && valid_hex(parts[1], true, 12) && valid_hex(parts[2], false, 8)
}

fn reftable_fsck_worktree(
    opts: &RefsVerifyOptions,
    common_dir: &Path,
    wt: &WorktreeCtx,
) -> Result<bool> {
    let reftable_dir = wt.gitdir.join("reftable");
    let list_path = reftable_dir.join("tables.list");
    let Ok(content) = fs::read_to_string(&list_path) else {
        return Ok(false);
    };

    let mut had_error = false;
    for table in content.split_whitespace() {
        if is_valid_reftable_name(table) {
            // Confirm the table parses; a garbage tables.list entry whose file
            // is unreadable/unparseable means the stack is broken.
            if !reftable_dir.join(table).exists() {
                return broken_reftable_stack(common_dir, wt);
            }
            continue;
        }
        // An invalid-looking name that still maps to a present, parseable table
        // is only a warning; otherwise the whole stack is broken.
        let table_path = reftable_dir.join(table);
        if table_path.exists() && sley_formats::Reftable::parse(&fs::read(&table_path)?).is_ok() {
            had_error |= opts.report(
                table,
                MsgId::BadReftableTableName,
                "invalid reftable table name",
            );
        } else {
            return broken_reftable_stack(common_dir, wt);
        }
    }
    Ok(had_error)
}

fn broken_reftable_stack(common_dir: &Path, wt: &WorktreeCtx) -> Result<bool> {
    let id = if wt.is_main {
        main_worktree_name(common_dir)
    } else {
        wt.prefix
            .trim_start_matches("worktrees/")
            .trim_end_matches('/')
            .to_string()
    };
    eprintln!("error: reftable stack for worktree '{id}' is broken");
    Ok(true)
}

/// The main worktree's display name is the basename of its top-level work
/// tree directory (the parent of the common git dir for a standard layout).
fn main_worktree_name(common_dir: &Path) -> String {
    common_dir
        .parent()
        .and_then(|parent| parent.file_name())
        .and_then(|name| name.to_str())
        .unwrap_or("")
        .to_string()
}

/// Verify the symbolic refs of a reftable-backed repository (the table-name
/// check does not look at ref contents). Mirrors the symref portion of
/// `reftable_be_fsck`.
fn reftable_fsck_symrefs(opts: &RefsVerifyOptions, git_dir: &Path, format: ObjectFormat) -> bool {
    let store = FileRefStore::new(git_dir, format);
    let mut had_error = false;
    let refs = match store.list_all_refs() {
        Ok(refs) => refs,
        Err(_) => return false,
    };
    for reference in refs {
        if let RefTarget::Symbolic(target) = &reference.target {
            had_error |= refs_fsck_symref(opts, &reference.name, target);
        }
    }
    had_error
}

// ---------------------------------------------------------------------------
// Entry points.
// ---------------------------------------------------------------------------

/// Run the files-backend verify across every worktree. Returns whether any
/// error (not warning) was emitted.
pub(crate) fn verify_files(
    opts: &RefsVerifyOptions,
    format: ObjectFormat,
    common_dir: &Path,
) -> bool {
    let mut had_error = false;
    if opts.verbose {
        eprintln!("Checking references consistency");
    }
    for wt in list_worktrees(common_dir) {
        had_error |= files_fsck_refs_dir(opts, format, common_dir, &wt);
        had_error |= files_fsck_root_refs(opts, format, common_dir, &wt);
        if wt.is_main {
            had_error |= packed_fsck(opts, format, common_dir);
        }
    }
    had_error
}

/// Run the reftable-backend verify across every worktree.
fn verify_reftable(
    opts: &RefsVerifyOptions,
    format: ObjectFormat,
    common_dir: &Path,
) -> Result<bool> {
    let mut had_error = false;
    if opts.verbose {
        eprintln!("Checking references consistency");
    }
    for wt in list_worktrees(common_dir) {
        had_error |= reftable_fsck_worktree(opts, common_dir, &wt)?;
        had_error |= reftable_fsck_symrefs(opts, &wt.gitdir, format);
    }
    Ok(had_error)
}

/// `git refs verify [--strict] [--verbose]`.
pub(crate) fn cmd_refs_verify(args: &[String]) -> Result<()> {
    let mut strict = false;
    let mut verbose = false;
    for arg in args {
        match arg.as_str() {
            "--strict" => strict = true,
            "--no-strict" => strict = false,
            "--verbose" => verbose = true,
            "--no-verbose" => verbose = false,
            "-h" | "--help" => {
                println!("usage: git refs verify [--strict] [--verbose]");
                return Err(GitError::Exit(129));
            }
            other if other.starts_with('-') => {
                eprintln!("error: unknown option `{}'", other.trim_start_matches('-'));
                return Err(GitError::Exit(129));
            }
            _ => {
                eprintln!("usage: 'git refs verify' takes no arguments");
                return Err(GitError::Exit(129));
            }
        }
    }

    let git_dir = discover_git_dir(env::current_dir()?)?;
    let common_dir = common_git_dir_for_git_dir(&git_dir)?;
    let format = repository_object_format(&git_dir)?;
    let opts = RefsVerifyOptions::from_repo(&git_dir, strict, verbose);

    let had_error = if repo_uses_reftable(&common_dir) {
        verify_reftable(&opts, format, &common_dir)?
    } else {
        verify_files(&opts, format, &common_dir)
    };

    if had_error {
        Err(GitError::Exit(1))
    } else {
        Ok(())
    }
}

/// Entry point for `git fsck --references` (the default). Uses the severity
/// table the fsck command already resolved. Returns whether any error fired.
pub(crate) fn verify_for_fsck(
    severity: SeverityConfig,
    verbose: bool,
    git_dir: &Path,
) -> Result<bool> {
    let common_dir = common_git_dir_for_git_dir(git_dir)?;
    let format = repository_object_format(git_dir)?;
    let opts = RefsVerifyOptions { severity, verbose };
    if repo_uses_reftable(&common_dir) {
        verify_reftable(&opts, format, &common_dir)
    } else {
        Ok(verify_files(&opts, format, &common_dir))
    }
}

/// Detect the reftable ref-storage backend from the repo config.
fn repo_uses_reftable(common_dir: &Path) -> bool {
    let Ok(config) = GitConfig::read(common_dir.join("config")) else {
        return false;
    };
    matches!(
        config.get("extensions", None, "refStorage"),
        Some("reftable")
    ) || config
        .get("extensions", None, "refStorage")
        .is_some_and(|value| value.starts_with("reftable://"))
}
