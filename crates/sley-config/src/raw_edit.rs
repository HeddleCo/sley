//! Byte-faithful in-place config editing — git's `git_config_set_multivar_in_file`.
//!
//! The canonical writer ([`crate::GitConfig::to_canonical_bytes`]) re-serialises
//! the entire document, which is fine for files sley itself authored but loses
//! fidelity when editing a *user-authored* file: it re-indents untouched lines,
//! rewrites `;` comments as `#`, collapses `[section] key = v` onto two lines,
//! drops blank lines, and normalises `key=v` spacing. Git never does this. Git
//! performs a *surgical* edit: it parses the file into a list of byte-offset
//! "events" (section headers, entries, comments, whitespace), locates the spans
//! that must change, and splices the new bytes in place — every untouched byte is
//! copied verbatim.
//!
//! This module re-implements that algorithm (`config.c:store_aux` +
//! `git_config_set_multivar_in_file_gently` + `write_pair`/`write_section` +
//! `maybe_remove_section`) so that `git config <set/unset/replace-all/add>` and
//! `--rename-section`/`--remove-section` preserve the original file byte-for-byte
//! apart from the lines they genuinely touch.

use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::quote_config_value;

/// A value-pattern predicate: given an entry's value (`None` for a bare
/// boolean-true key), report whether it matches git's value-pattern. The
/// `'a` borrow lets callers close over a compiled regex.
pub type ValueMatcher<'a> = &'a dyn Fn(Option<&str>) -> bool;

/// The kind of a parsed event, mirroring git's `enum config_event_t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EventType {
    Section,
    Entry,
    Comment,
    Whitespace,
}

/// One parsed element with its byte span `[begin, end)` in the source.
#[derive(Debug, Clone)]
struct Event {
    ty: EventType,
    begin: usize,
    end: usize,
    /// For `Section` events: whether this header names the target section.
    /// For `Entry` events: whether the entry is inside such a section, so an
    /// absent key can be inserted there.
    is_keys_section: bool,
    /// For `Entry` events: whether the entry's section/subsection prefix is an
    /// exact key match. Deprecated dotted headers are section-compatible
    /// case-insensitively, but their parsed subsection is lower-cased for full
    /// key matching.
    is_key_match_section: bool,
    /// For `Entry` events: the parsed key (lower-cased) and decoded value.
    key: Option<String>,
    value: Option<String>,
}

/// Result of [`RawConfigEditor::set_multivar`] mirroring git's `CONFIG_*` codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawEditOutcome {
    /// The edit was applied; the buffer was rewritten.
    Changed,
    /// Nothing matched on an unset, or more than one entry matched a
    /// single-replace set (git's `CONFIG_NOTHING_SET`, exit code 5).
    NothingSet,
}

/// Options for writing a config file through `<path>.lock`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ConfigFileWriteOptions {
    /// Sync the lockfile contents to disk before promoting it into place.
    pub fsync: bool,
}

/// A config file write failed before the lockfile could be atomically promoted.
#[derive(Debug)]
pub enum ConfigFileWriteError {
    /// `<path>.lock` already exists; the original config was left untouched.
    ExistingLock(PathBuf),
    /// Filesystem failure at the given path.
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl fmt::Display for ConfigFileWriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ExistingLock(path) => {
                write!(f, "config lock already exists: {}", path.display())
            }
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
        }
    }
}

impl std::error::Error for ConfigFileWriteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ExistingLock(_) => None,
            Self::Io { source, .. } => Some(source),
        }
    }
}

impl ConfigFileWriteError {
    fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}

/// Write `contents` to `path` via `<path>.lock` and an atomic rename.
///
/// The helper mirrors git's config writer shape: create the parent directory if
/// needed, refuse to reuse an existing lockfile, write the complete replacement
/// bytes to the lock, optionally fsync them, then promote the lock into place.
/// On errors before promotion the lockfile is removed when possible and the
/// original config file is left untouched.
pub fn write_config_file_locked(
    path: impl AsRef<Path>,
    contents: &[u8],
    options: ConfigFileWriteOptions,
) -> Result<(), ConfigFileWriteError> {
    let path = path.as_ref();
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|err| ConfigFileWriteError::io(parent, err))?;
    }

    let lock_path = config_lock_path(path)?;
    let mut lock = ConfigFileLock::acquire(lock_path)?;
    lock.write_all(contents, options.fsync)?;
    let lock_path = lock.close();
    if let Err(err) = fs::rename(&lock_path, path) {
        let _ = fs::remove_file(&lock_path);
        return Err(ConfigFileWriteError::io(path, err));
    }
    Ok(())
}

fn config_lock_path(path: &Path) -> Result<PathBuf, ConfigFileWriteError> {
    let file_name = path.file_name().ok_or_else(|| {
        ConfigFileWriteError::io(
            path,
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "config path has no filename",
            ),
        )
    })?;
    let mut lock_name = file_name.to_os_string();
    lock_name.push(".lock");
    Ok(path.with_file_name(lock_name))
}

struct ConfigFileLock {
    path: PathBuf,
    file: Option<fs::File>,
    active: bool,
}

impl ConfigFileLock {
    fn acquire(path: PathBuf) -> Result<Self, ConfigFileWriteError> {
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => Ok(Self {
                path,
                file: Some(file),
                active: true,
            }),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                Err(ConfigFileWriteError::ExistingLock(path))
            }
            Err(err) => Err(ConfigFileWriteError::io(path, err)),
        }
    }

    fn write_all(&mut self, contents: &[u8], fsync: bool) -> Result<(), ConfigFileWriteError> {
        let Some(file) = self.file.as_mut() else {
            return Err(ConfigFileWriteError::io(
                &self.path,
                std::io::Error::other("config lock is already closed"),
            ));
        };
        file.write_all(contents)
            .map_err(|err| ConfigFileWriteError::io(&self.path, err))?;
        if fsync {
            file.sync_all()
                .map_err(|err| ConfigFileWriteError::io(&self.path, err))?;
        }
        Ok(())
    }

    fn close(mut self) -> PathBuf {
        self.active = false;
        let _ = self.file.take();
        self.path.clone()
    }
}

impl Drop for ConfigFileLock {
    fn drop(&mut self) {
        if self.active {
            let _ = self.file.take();
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// A byte-faithful editor over a single config file's raw contents.
pub struct RawConfigEditor {
    contents: Vec<u8>,
    events: Vec<Event>,
    /// The matched key section's normalised `section.subsection` prefix used for
    /// header synthesis when the key/section is absent.
    section: String,
    subsection: Option<String>,
    /// The variable name (suffix after the last dot), lower-cased for matching.
    name: String,
    /// The variable name as typed on the command line, written verbatim into a
    /// newly-synthesised line (git's `write_pair` preserves the caller's case,
    /// e.g. `git config Section.Movie x` writes `Movie`, not `movie`).
    name_as_typed: String,
}

impl RawConfigEditor {
    /// Parse `contents` into byte-offset events for the variable `key`
    /// (`section[.subsection].name`). Section/name comparisons are
    /// case-insensitive; quoted subsections are case-sensitive. The `section`
    /// and `name` are written verbatim (preserving the caller's case) when a new
    /// header/line must be synthesised.
    pub fn new(contents: Vec<u8>, section: &str, subsection: Option<&str>, name: &str) -> Self {
        let mut editor = Self {
            contents,
            events: Vec::new(),
            section: section.to_string(),
            subsection: subsection.map(str::to_string),
            name: name.to_ascii_lowercase(),
            name_as_typed: name.to_string(),
        };
        editor.parse_events();
        editor
    }

    /// Walk the raw bytes emitting contiguous events with byte spans, matching
    /// git's `git_parse_source` event sequence. Each event's `end` is the next
    /// event's `begin` (git makes spans contiguous via `do_event`).
    fn parse_events(&mut self) {
        let bytes = self.contents.clone();
        let len = bytes.len();
        let mut i = 0usize;
        // Whether the most recent section header names the target section, so
        // entries know whether they belong to it.
        let mut cur_is_keys_section = false;
        let mut cur_is_key_match_section = false;

        // Skip a UTF-8 BOM exactly as git does.
        if bytes.starts_with(b"\xEF\xBB\xBF") {
            i = 3;
        }

        while i < len {
            let c = bytes[i];
            if c == b'\n' {
                self.push_ws(i, i + 1);
                i += 1;
                continue;
            }
            if c == b' ' || c == b'\t' || c == b'\r' {
                // Coalesce a run of whitespace into one event (git collapses
                // consecutive WHITESPACE events).
                let begin = i;
                while i < len && matches!(bytes[i], b' ' | b'\t' | b'\r') {
                    i += 1;
                }
                self.push_ws(begin, i);
                continue;
            }
            if c == b'#' || c == b';' {
                // Comment runs to end of line (the newline is a separate WS event).
                let begin = i;
                while i < len && bytes[i] != b'\n' {
                    i += 1;
                }
                self.events.push(Event {
                    ty: EventType::Comment,
                    begin,
                    end: i,
                    is_keys_section: false,
                    is_key_match_section: false,
                    key: None,
                    value: None,
                });
                continue;
            }
            if c == b'[' {
                let begin = i;
                let (section, sub, subsection_case_sensitive, next) =
                    parse_section_header(&bytes, i);
                i = next;
                let is_keys = section.as_ref().is_some_and(|s| {
                    self.section_matches(s, sub.as_deref(), subsection_case_sensitive)
                });
                let is_key_match = section
                    .as_ref()
                    .is_some_and(|s| self.section_exact_matches(s, sub.as_deref()));
                cur_is_keys_section = is_keys;
                cur_is_key_match_section = is_key_match;
                self.events.push(Event {
                    ty: EventType::Section,
                    begin,
                    end: i,
                    is_keys_section: is_keys,
                    is_key_match_section: is_key_match,
                    key: None,
                    value: None,
                });
                continue;
            }
            if c.is_ascii_alphabetic() {
                let begin = i;
                let (key, value, next) = parse_entry(&bytes, i);
                i = next;
                self.events.push(Event {
                    ty: EventType::Entry,
                    begin,
                    end: i,
                    is_keys_section: cur_is_keys_section,
                    is_key_match_section: cur_is_key_match_section,
                    key,
                    value,
                });
                continue;
            }
            // An unexpected character: treat the rest of the line as opaque so we
            // never corrupt a file we cannot fully model (parse already validated
            // it upstream before this editor runs).
            let begin = i;
            while i < len && bytes[i] != b'\n' {
                i += 1;
            }
            self.push_ws(begin, i);
        }
    }

    fn push_ws(&mut self, begin: usize, end: usize) {
        if begin >= end {
            return;
        }
        // git collapses consecutive WHITESPACE events into one.
        if let Some(last) = self.events.last_mut()
            && last.ty == EventType::Whitespace
            && last.end == begin
        {
            last.end = end;
            return;
        }
        self.events.push(Event {
            ty: EventType::Whitespace,
            begin,
            end,
            is_keys_section: false,
            is_key_match_section: false,
            key: None,
            value: None,
        });
    }

    fn section_matches(
        &self,
        section: &str,
        subsection: Option<&str>,
        subsection_case_sensitive: bool,
    ) -> bool {
        if !section.eq_ignore_ascii_case(&self.section) {
            return false;
        }
        match (self.subsection.as_deref(), subsection) {
            (None, None) => true,
            (Some(a), Some(b)) => {
                if subsection_case_sensitive {
                    a == b
                } else {
                    a.eq_ignore_ascii_case(b)
                }
            }
            _ => false,
        }
    }

    fn section_exact_matches(&self, section: &str, subsection: Option<&str>) -> bool {
        if !section.eq_ignore_ascii_case(&self.section) {
            return false;
        }
        match (self.subsection.as_deref(), subsection) {
            (None, None) => true,
            (Some(a), Some(b)) => a == b,
            _ => false,
        }
    }

    /// Apply git's `git_config_set_multivar_in_file_gently`.
    ///
    /// * `value == None` → unset (remove matching entries; error if none match).
    /// * `value == Some` → set: replace matching entries with one new pair.
    /// * `value_matches` filters which existing entries are considered a match
    ///   (git's value-pattern; pass a closure that already folds in `!` negation).
    ///   `None` means "every entry of the key matches".
    /// * `multi_replace` is git's `CONFIG_FLAGS_MULTI_REPLACE` (`--replace-all`):
    ///   when false and more than one entry matches, the edit is refused
    ///   (`NothingSet`).
    ///
    /// On `Changed`, the internal buffer is rewritten; read it with
    /// [`RawConfigEditor::into_bytes`].
    pub fn set_multivar(
        &mut self,
        value: Option<&str>,
        comment: Option<&str>,
        value_matches: Option<ValueMatcher>,
        multi_replace: bool,
    ) -> RawEditOutcome {
        // Faithfully replicate git's `store.seen[]` / `seen_nr` / `key_seen` /
        // `section_seen` state machine (`store_aux` + `store_aux_event`).
        //
        // `seen` carries `seen_nr` *committed* matches plus one optional
        // *speculative* slot at `seen[seen_nr]` (git writes the slot before it
        // knows whether the entry matches, and only bumps `seen_nr` on a match).
        let mut seen: Vec<usize> = Vec::new();
        let mut seen_nr = 0usize;
        let mut key_seen = false;
        let mut section_seen = false;

        let set_slot = |seen: &mut Vec<usize>, slot: usize, idx: usize| {
            if seen.len() <= slot {
                seen.resize(slot + 1, 0);
            }
            seen[slot] = idx;
        };

        for (idx, ev) in self.events.iter().enumerate() {
            match ev.ty {
                EventType::Section if ev.is_keys_section => {
                    // store_aux_event: record speculatively, mark section_seen.
                    section_seen = true;
                    set_slot(&mut seen, seen_nr, idx);
                }
                EventType::Entry => {
                    if key_seen {
                        if self.entry_matches(ev, value_matches) {
                            set_slot(&mut seen, seen_nr, idx);
                            seen_nr += 1;
                        }
                    } else if ev.is_keys_section {
                        set_slot(&mut seen, seen_nr, idx);
                        section_seen = true;
                        if self.entry_matches(ev, value_matches) {
                            seen_nr += 1;
                            key_seen = true;
                        }
                    }
                }
                _ => {}
            }
        }

        // Error conditions (git): nothing to unset, or >1 match on single replace.
        if (seen_nr == 0 && value.is_none()) || (seen_nr > 1 && !multi_replace) {
            return RawEditOutcome::NothingSet;
        }

        // git: when seen_nr == 0 here (insert, key absent), fall back to the
        // speculative slot (last entry of the target section, or the section
        // header, or — if even the section is absent — the last parsed element).
        if seen_nr == 0 {
            if seen.is_empty()
                && let Some(last) = self.events.len().checked_sub(1)
            {
                // Did not see key nor section: target the last parsed element.
                seen.push(last);
            }
            // else: keep the speculative slot at index 0.
            seen_nr = 1;
        }

        let seen_indices: Vec<usize> = seen.into_iter().take(seen_nr).collect();
        self.splice(&seen_indices, key_seen, section_seen, value, comment);
        RawEditOutcome::Changed
    }

    fn entry_matches(&self, ev: &Event, value_matches: Option<ValueMatcher>) -> bool {
        // git's `matches()` compares the FULL key (`section.subsection.name`), so
        // an entry only matches when it is BOTH in the target section and has the
        // target variable name. (Without the section guard a same-named variable
        // in a different section would be wrongly collected — t1300 #235.)
        if !ev.is_key_match_section || ev.key.as_deref() != Some(self.name.as_str()) {
            return false;
        }
        match value_matches {
            None => true,
            Some(pred) => pred(ev.value.as_deref()),
        }
    }

    /// The core byte-splice, mirroring the final loop of
    /// `git_config_set_multivar_in_file_gently`. A single loop drives both set
    /// (replace each matched entry with one new pair at the end) and unset
    /// (delete each matched entry's span, optionally extending to swallow an
    /// emptied section).
    fn splice(
        &mut self,
        seen: &[usize],
        key_seen: bool,
        section_seen: bool,
        value: Option<&str>,
        comment: Option<&str>,
    ) {
        let contents = self.contents.clone();
        let contents_sz = contents.len();
        let mut out: Vec<u8> = Vec::with_capacity(contents_sz + 64);
        let mut copy_begin = 0usize;

        let mut i = 0usize;
        while i < seen.len() {
            let j = seen[i];
            let mut new_line = false;
            let copy_end;
            let replace_end;
            if !key_seen {
                // Inserting a fresh key after the speculative slot (section header
                // or last entry of the section). Copy up to its end; include the
                // trailing '\n' when present.
                let mut ce = self.events[j].end;
                if ce > 0 && ce < contents_sz && contents[ce - 1] != b'\n' && contents[ce] == b'\n'
                {
                    ce += 1;
                }
                copy_end = ce;
                replace_end = ce;
            } else {
                let mut re = self.events[j].end;
                let mut ce = self.events[j].begin;
                if value.is_none() {
                    // Unset: maybe extend to swallow the whole emptied section.
                    let (nb, ne) = self.maybe_remove_section(seen, &mut i, ce, re);
                    ce = nb;
                    re = ne;
                }
                // Swallow preceding whitespace on the same line.
                while ce > 0 {
                    let ch = contents[ce - 1];
                    if (ch == b' ' || ch == b'\t' || ch == b'\r') && ch != b'\n' {
                        ce -= 1;
                    } else {
                        break;
                    }
                }
                copy_end = ce;
                replace_end = re;
            }

            if copy_end > 0 && contents[copy_end - 1] != b'\n' {
                new_line = true;
            }
            if copy_end > copy_begin {
                out.extend_from_slice(&contents[copy_begin..copy_end]);
                if new_line {
                    out.push(b'\n');
                }
            }
            copy_begin = replace_end;
            i += 1;
        }

        // Write the new pair (value == None means pure unset).
        if let Some(value) = value {
            if !section_seen {
                write_section(&mut out, &self.section, self.subsection.as_deref());
            }
            write_pair(&mut out, &self.name_as_typed, value, comment);
        }

        if copy_begin < contents_sz {
            out.extend_from_slice(&contents[copy_begin..contents_sz]);
        }

        self.contents = out;
    }

    /// git's `maybe_remove_section`: if unsetting the first/only key of a section
    /// and there are no comments inside or just before the section, extend the
    /// removal span to cover the whole section header and any trailing entries.
    ///
    /// Returns the (begin, end) of the span to remove.
    fn maybe_remove_section(
        &self,
        seen: &[usize],
        seen_ptr: &mut usize,
        begin_in: usize,
        end_in: usize,
    ) -> (usize, usize) {
        // git writes to `*begin_offset`/`*end_offset` (and `*seen_ptr`) only on
        // the final success; every early return leaves the caller's span
        // untouched. So we compute a *local* `begin`/`end`/`seen_idx` and only
        // commit them at the end.
        let parsed = &self.events;
        let parsed_nr = parsed.len();
        let mut seen_idx = *seen_ptr;

        // First, ensure this is the section's first key and that no comment
        // precedes the entry or its section header. Mirrors git's
        // `for (i = seen[seen]; i > 0; i--)` over `parsed[i-1]`, breaking with `i`
        // pointing at the keys-section header (or 0).
        let mut section_seen = false;
        let mut i = seen[seen_idx];
        while i > 0 {
            let ty = parsed[i - 1].ty;
            match ty {
                EventType::Comment => return (begin_in, end_in),
                EventType::Entry => {
                    if !section_seen {
                        return (begin_in, end_in);
                    }
                    break;
                }
                EventType::Section => {
                    if !parsed[i - 1].is_keys_section {
                        break;
                    }
                    section_seen = true;
                    i -= 1;
                }
                EventType::Whitespace => {
                    i -= 1;
                }
            }
        }
        let begin = parsed[i].begin;

        // Next, ensure we remove the last key(s) and there are no enclosing or
        // surrounding comments.
        let mut k = seen[seen_idx] + 1;
        while k < parsed_nr {
            let ty = parsed[k].ty;
            match ty {
                EventType::Comment => return (begin_in, end_in),
                EventType::Section => {
                    if parsed[k].is_keys_section {
                        k += 1;
                        continue;
                    }
                    break;
                }
                EventType::Entry => {
                    seen_idx += 1;
                    if seen_idx < seen.len() && k == seen[seen_idx] {
                        // We want to remove this entry too.
                        k += 1;
                        continue;
                    }
                    // Another entry survives in this section.
                    return (begin_in, end_in);
                }
                EventType::Whitespace => {
                    k += 1;
                }
            }
        }

        // Really removing the section's last entry/entries with no comments.
        *seen_ptr = seen_idx;
        let end = if k < parsed_nr {
            parsed[k].begin
        } else {
            parsed[parsed_nr - 1].end
        };
        (begin, end)
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.contents
    }
}

/// git's `GIT_CONFIG_MAX_LINE_LEN`.
const GIT_CONFIG_MAX_LINE_LEN: usize = 512 * 1024;

/// Outcome of [`rename_or_remove_section`].
pub enum SectionEditOutcome {
    /// One or more sections matched; the new file bytes are returned.
    Changed(Vec<u8>),
    /// No section matched `old_name` (git's `ret == 0` → exit 128).
    NotFound,
    /// A line exceeded `GIT_CONFIG_MAX_LINE_LEN`; carries the 1-based line
    /// number for git's "refusing to work with overly long line" diagnostic.
    LineTooLong(usize),
}

/// git's `repo_config_copy_or_rename_section_in_file` (copy = false): rename
/// every `[old_name]` header to `[new_name]` (when `new_name` is `Some`) or
/// remove the matching sections (`new_name == None`), preserving every other
/// byte. `old_name`/`new_name` are git's normalised `section[.subsection]`
/// form (section lower-cased).
pub fn rename_or_remove_section(
    contents: &[u8],
    old_name: &str,
    new_name: Option<&str>,
) -> SectionEditOutcome {
    let mut out: Vec<u8> = Vec::with_capacity(contents.len() + 16);
    let mut matched = 0usize;
    let mut removing = false;
    let mut line_nr = 0usize;
    let mut pos = 0usize;
    let len = contents.len();

    while pos < len {
        // Read one whole physical line including its trailing '\n'.
        let line_start = pos;
        while pos < len && contents[pos] != b'\n' {
            pos += 1;
        }
        if pos < len {
            pos += 1; // include the '\n'
        }
        let line = &contents[line_start..pos];
        line_nr += 1;

        if line.len() >= GIT_CONFIG_MAX_LINE_LEN {
            return SectionEditOutcome::LineTooLong(line_nr);
        }

        // Find the first non-space char (git skips leading whitespace).
        let mut i = 0usize;
        while i < line.len() && (line[i] as char).is_whitespace() {
            i += 1;
        }
        if i < line.len() && line[i] == b'[' {
            let offset = section_name_match(&line[i..], old_name);
            if offset > 0 {
                matched += 1;
                if let Some(new_name) = new_name {
                    write_section_normalised(&mut out, new_name);
                    // Skip the old section header; emit the remainder of the line
                    // (an inline declaration) indented with a tab. git gobbles the
                    // trailing newline into `offset` for a bare header, so a header
                    // with nothing after it leaves `consumed == line.len()`.
                    let consumed = i + offset;
                    if consumed < line.len() {
                        // There is more content on this line beyond the header.
                        out.push(b'\t');
                        out.extend_from_slice(&line[consumed..]);
                    }
                    removing = false;
                    continue;
                } else {
                    // Remove mode: drop this header and following lines until the
                    // next section.
                    removing = true;
                    continue;
                }
            }
            removing = false;
        }
        if removing {
            continue;
        }
        out.extend_from_slice(line);
    }

    if matched == 0 {
        SectionEditOutcome::NotFound
    } else {
        SectionEditOutcome::Changed(out)
    }
}

/// git's `section_name_match`: returns the byte length consumed (including
/// trailing whitespace after `]`) when `buf` (starting at `[`) is a header for
/// `name` (`section[.subsection]`), else 0.
fn section_name_match(buf: &[u8], name: &str) -> usize {
    // Treat `buf` as a NUL-terminated C string: `gb(k)` returns 0 past the end,
    // mirroring git's reliance on the trailing NUL to stop the scan.
    let nb = name.as_bytes();
    let gb = |k: usize| -> u8 { *buf.get(k).unwrap_or(&0) };
    let gn = |k: usize| -> u8 { *nb.get(k).unwrap_or(&0) };

    if gb(0) != b'[' {
        return 0;
    }
    // Faithful port of git's `for (i = 1; buf[i] && buf[i] != ']'; i++)`: the
    // loop's `i++` fires at the end of EVERY iteration, including after the
    // `continue` in the dot-transition branch (this is the subtle part — the
    // opening `"` of a quoted subsection is skipped by that trailing `i++`).
    let mut i = 1usize;
    let mut j = 0usize;
    let mut dot = false;
    // git's `isspace`: space, tab, newline, CR, form-feed, vertical-tab. The
    // trailing-whitespace gobble after `]` deliberately swallows the line's `\n`,
    // so a bare header consumes its whole line (leaving no inline remainder).
    let is_space =
        |c: u8| c == b' ' || c == b'\t' || c == b'\n' || c == b'\r' || c == 0x0c || c == 0x0b;
    'outer: while gb(i) != 0 && gb(i) != b']' {
        // `continue` in C re-runs the for-increment; emulate with a flag.
        let mut did_continue = false;
        if !dot && is_space(gb(i)) {
            dot = true;
            let nj = gn(j);
            j += 1;
            if nj != b'.' {
                break;
            }
            // for (i++; isspace(buf[i]); i++);
            i += 1;
            while is_space(gb(i)) {
                i += 1;
            }
            if gb(i) != b'"' {
                break;
            }
            did_continue = true;
        }
        if !did_continue {
            if gb(i) == b'\\' && dot {
                i += 1;
            } else if gb(i) == b'"' && dot {
                i += 1;
                while is_space(gb(i)) {
                    i += 1;
                }
                break 'outer;
            }
            // buf[i] != name[j++]
            let bc = gb(i);
            let nc = gn(j);
            j += 1;
            if bc != nc {
                break 'outer;
            }
        }
        i += 1; // the for-loop increment, run for every iteration
    }
    if gb(i) == b']' && gn(j) == 0 {
        i += 1;
        while gb(i) != 0 && is_space(gb(i)) {
            i += 1;
        }
        return i;
    }
    0
}

/// git's `store_create_section`/`write_section` for the rename path: render the
/// normalised `section[.subsection]` name as `[section "subsection"]` (or
/// `[section]`) followed by a newline.
fn write_section_normalised(out: &mut Vec<u8>, name: &str) {
    out.push(b'[');
    if let Some((section, subsection)) = name.split_once('.') {
        out.extend_from_slice(section.as_bytes());
        out.extend_from_slice(b" \"");
        for ch in subsection.chars() {
            if ch == '"' || ch == '\\' {
                out.push(b'\\');
            }
            let mut b = [0u8; 4];
            out.extend_from_slice(ch.encode_utf8(&mut b).as_bytes());
        }
        out.push(b'"');
    } else {
        out.extend_from_slice(name.as_bytes());
    }
    out.extend_from_slice(b"]\n");
}

/// git's `write_pair`: always `\t<name> = <quoted-value>[<comment>]\n`.
fn write_pair(out: &mut Vec<u8>, name: &str, value: &str, comment: Option<&str>) {
    out.push(b'\t');
    out.extend_from_slice(name.as_bytes());
    out.extend_from_slice(b" = ");
    out.extend_from_slice(quote_config_value(value).as_bytes());
    if let Some(comment) = comment {
        out.extend_from_slice(comment.as_bytes());
    }
    out.push(b'\n');
}

/// git's `write_section`: `[<section>]\n` or `[<section> "<subsection>"]\n`.
fn write_section(out: &mut Vec<u8>, section: &str, subsection: Option<&str>) {
    out.push(b'[');
    out.extend_from_slice(section.as_bytes());
    if let Some(sub) = subsection {
        out.extend_from_slice(b" \"");
        for ch in sub.chars() {
            if ch == '"' || ch == '\\' {
                out.push(b'\\');
            }
            let mut buf = [0u8; 4];
            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
        }
        out.push(b'"');
    }
    out.extend_from_slice(b"]\n");
}

/// Parse a section header starting at `bytes[start] == b'['`, returning the
/// (section, subsection, subsection_case_sensitive, next_index). On a malformed
/// header the whole rest of the line is consumed and `None` is returned for the
/// name.
fn parse_section_header(
    bytes: &[u8],
    start: usize,
) -> (Option<String>, Option<String>, bool, usize) {
    let len = bytes.len();
    let mut i = start + 1; // past '['
    let name_start = i;
    while i < len {
        let c = bytes[i];
        if c.is_ascii_alphanumeric() || c == b'-' || c == b'.' {
            i += 1;
        } else {
            break;
        }
    }
    let raw_name = String::from_utf8_lossy(&bytes[name_start..i]).into_owned();

    // Dotted deprecated form: `[section.subsection]`.
    if let Some((head, rest)) = raw_name.split_once('.') {
        // consume up to closing ']'
        while i < len && bytes[i] != b']' && bytes[i] != b'\n' {
            i += 1;
        }
        if i < len && bytes[i] == b']' {
            i += 1;
        }
        return (
            Some(head.to_ascii_lowercase()),
            Some(rest.to_ascii_lowercase()),
            false,
            i,
        );
    }

    // Skip blanks, optional quoted subsection.
    while i < len && matches!(bytes[i], b' ' | b'\t' | b'\r') {
        i += 1;
    }
    let mut subsection = None;
    if i < len && bytes[i] == b'"' {
        i += 1;
        let mut sub = String::new();
        while i < len {
            match bytes[i] {
                b'"' => {
                    i += 1;
                    break;
                }
                b'\\' if i + 1 < len => {
                    i += 1;
                    sub.push(bytes[i] as char);
                    i += 1;
                }
                b'\n' => break,
                other => {
                    sub.push(other as char);
                    i += 1;
                }
            }
        }
        subsection = Some(sub);
        while i < len && matches!(bytes[i], b' ' | b'\t' | b'\r') {
            i += 1;
        }
    }
    if i < len && bytes[i] == b']' {
        i += 1;
    } else {
        // Malformed: consume to end of line.
        while i < len && bytes[i] != b'\n' {
            i += 1;
        }
        return (None, None, true, i);
    }
    (Some(raw_name), subsection, true, i)
}

/// Parse an entry starting at `bytes[start]` (an alpha key char). Returns
/// (lower-cased key, decoded value, next_index). Continuation lines (`\`-newline)
/// are consumed so the entry span covers the full logical entry.
fn parse_entry(bytes: &[u8], start: usize) -> (Option<String>, Option<String>, usize) {
    let len = bytes.len();
    let mut i = start;
    let key_start = i;
    while i < len {
        let c = bytes[i];
        if c.is_ascii_alphanumeric() || c == b'-' {
            i += 1;
        } else {
            break;
        }
    }
    let key = String::from_utf8_lossy(&bytes[key_start..i]).to_ascii_lowercase();
    // Skip blanks.
    while i < len && matches!(bytes[i], b' ' | b'\t' | b'\r') {
        i += 1;
    }
    if i >= len || bytes[i] == b'\n' {
        // Bare boolean-true key.
        if i < len && bytes[i] == b'\n' {
            i += 1;
        }
        return (Some(key), None, i);
    }
    if bytes[i] != b'=' {
        // Malformed — consume the line.
        while i < len && bytes[i] != b'\n' {
            i += 1;
        }
        if i < len {
            i += 1;
        }
        return (Some(key), None, i);
    }
    i += 1; // past '='
    let (value, next) = parse_value_span(bytes, i);
    (Some(key), Some(value), next)
}

/// Decode a value starting after `=`, returning (decoded value, next_index).
/// Mirrors the value parser in `lib.rs` enough to know the entry's byte span and
/// the decoded value used for pattern matching. The newline that ends the value
/// is consumed into the span.
fn parse_value_span(bytes: &[u8], start: usize) -> (String, usize) {
    let len = bytes.len();
    let mut i = start;
    let mut out = String::new();
    let mut trailing_ws = 0usize;
    let mut leading = true;
    let mut in_quotes = false;
    while i < len {
        let c = bytes[i];
        match c {
            b'\n' if !in_quotes => {
                i += 1;
                break;
            }
            b'"' => {
                i += 1;
                in_quotes = !in_quotes;
                leading = false;
            }
            b'\\' => {
                i += 1;
                if i >= len {
                    break;
                }
                let e = bytes[i];
                i += 1;
                match e {
                    b'\n' => {} // line continuation
                    b'\r' if i < len && bytes[i] == b'\n' => {
                        i += 1;
                    }
                    b'n' => {
                        out.push('\n');
                        trailing_ws = 0;
                        leading = false;
                    }
                    b't' => {
                        out.push('\t');
                        trailing_ws = 0;
                        leading = false;
                    }
                    b'b' => {
                        out.push('\u{0008}');
                        trailing_ws = 0;
                        leading = false;
                    }
                    b'"' => {
                        out.push('"');
                        trailing_ws = 0;
                        leading = false;
                    }
                    b'\\' => {
                        out.push('\\');
                        trailing_ws = 0;
                        leading = false;
                    }
                    _ => {
                        // Invalid escape; keep going to find the span end.
                        leading = false;
                    }
                }
            }
            b'#' | b';' if !in_quotes => {
                // Comment terminates the value; consume to end of line.
                while i < len && bytes[i] != b'\n' {
                    i += 1;
                }
                if i < len {
                    i += 1;
                }
                break;
            }
            b' ' | b'\t' if !in_quotes => {
                i += 1;
                if leading {
                    // drop
                } else {
                    out.push(c as char);
                    trailing_ws += 1;
                }
            }
            b'\r' if !in_quotes => {
                i += 1;
                if i < len && bytes[i] == b'\n' {
                    // trailing CR before newline — line ending
                } else if !leading {
                    out.push('\r');
                    trailing_ws = 0;
                    leading = false;
                }
            }
            other => {
                i += 1;
                out.push(other as char);
                trailing_ws = 0;
                leading = false;
            }
        }
    }
    out.truncate(out.len().saturating_sub(trailing_ws));
    (out, i)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::too_many_arguments)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "sley-config-raw-edit-{}-{}",
                std::process::id(),
                TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).expect("create temp dir");
            Self { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn edit(
        src: &str,
        sec: &str,
        sub: Option<&str>,
        name: &str,
        value: Option<&str>,
        comment: Option<&str>,
        vm: Option<ValueMatcher>,
        multi: bool,
    ) -> (String, RawEditOutcome) {
        let mut e = RawConfigEditor::new(src.as_bytes().to_vec(), sec, sub, name);
        let out = e.set_multivar(value, comment, vm, multi);
        (String::from_utf8(e.into_bytes()).unwrap(), out)
    }

    #[test]
    fn write_config_file_locked_writes_and_cleans_lock() {
        let temp = TempDir::new();
        let path = temp.path.join("nested").join("config");

        write_config_file_locked(
            &path,
            b"[user]\n\tname = Ada\n",
            ConfigFileWriteOptions::default(),
        )
        .expect("write config");

        assert_eq!(
            fs::read(&path).expect("read config"),
            b"[user]\n\tname = Ada\n"
        );
        assert!(!path.with_file_name("config.lock").exists());
    }

    #[test]
    fn write_config_file_locked_existing_lock_preserves_original() {
        let temp = TempDir::new();
        let path = temp.path.join("config");
        let lock_path = path.with_file_name("config.lock");
        fs::write(&path, b"[user]\n\tname = Old\n").expect("write original");
        fs::write(&lock_path, b"held\n").expect("write lock");

        let err = write_config_file_locked(
            &path,
            b"[user]\n\tname = New\n",
            ConfigFileWriteOptions::default(),
        )
        .expect_err("held lock must fail");

        assert!(matches!(err, ConfigFileWriteError::ExistingLock(_)));
        assert_eq!(
            fs::read(&path).expect("read original"),
            b"[user]\n\tname = Old\n"
        );
        assert_eq!(fs::read(&lock_path).expect("read lock"), b"held\n");
    }

    #[test]
    fn unset_cont_lines_preserves_layout() {
        let src = "[alpha]\nbar = foo\n[beta]\nbaz = multiple \\\nlines\nfoo = bar\n";
        let (out, _) = edit(src, "beta", None, "baz", None, None, None, true);
        assert_eq!(out, "[alpha]\nbar = foo\n[beta]\nfoo = bar\n");
    }

    #[test]
    fn unset_all_silly_comments_preserved() {
        let src = "[beta] ; silly comment # another comment\nnoIndent= sillyValue ; 'nother silly comment\n\n# empty line\n\t\t; comment\n\t\thaha   =\"beta\" # last silly comment\nhaha = hello\n\thaha = bello\n[nextSection] noNewline = ouch\n";
        let (out, _) = edit(src, "beta", None, "haha", None, None, None, true);
        let expect = "[beta] ; silly comment # another comment\nnoIndent= sillyValue ; 'nother silly comment\n\n# empty line\n\t\t; comment\n[nextSection] noNewline = ouch\n";
        assert_eq!(out, expect);
    }

    #[test]
    fn replace_all_preserves_other_lines() {
        let src = "[beta] ; silly comment # another comment\nnoIndent= sillyValue ; 'nother silly comment\n\n# empty line\n\t\t; comment\n\t\thaha   =\"beta\" # last silly comment\nhaha = hello\n\thaha = bello\n[nextSection] noNewline = ouch\n";
        let (out, _) = edit(src, "beta", None, "haha", Some("gamma"), None, None, true);
        let expect = "[beta] ; silly comment # another comment\nnoIndent= sillyValue ; 'nother silly comment\n\n# empty line\n\t\t; comment\n\thaha = gamma\n[nextSection] noNewline = ouch\n";
        assert_eq!(out, expect);
    }

    #[test]
    fn replace_all_does_not_touch_same_name_in_other_section() {
        // `abc.key` must NOT match `xyz.key` — a same-named variable in another
        // section (t1300 #235). The `[abc]key` header has an inline bare key.
        let src = "[abc]key\n\tkeepSection\n[xyz]\n\tkey = 1\n[abc]\n\tkey = a\n";
        let (out, _) = edit(src, "abc", None, "key", Some("b"), None, None, true);
        let expect = "[abc]\n\tkeepSection\n[xyz]\n\tkey = 1\n[abc]\n\tkey = b\n";
        assert_eq!(out, expect);
    }

    #[test]
    fn set_uses_case_compatible_dotted_subsection_for_insert() {
        let src = "[V.A]\n\tx = old\n";
        let (out, outcome) = edit(src, "V", Some("A"), "r", Some("new"), None, None, false);
        assert_eq!(outcome, RawEditOutcome::Changed);
        assert_eq!(out, "[V.A]\n\tx = old\n\tr = new\n");
    }

    #[test]
    fn set_keeps_dotted_subsection_exactness_for_replacement() {
        let src = "[V.A]\n\tr = old\n";
        let (out, outcome) = edit(src, "v", Some("a"), "r", Some("new"), None, None, false);
        assert_eq!(outcome, RawEditOutcome::Changed);
        assert_eq!(out, "[V.A]\n\tr = new\n");

        let (out, outcome) = edit(src, "V", Some("A"), "r", Some("new"), None, None, false);
        assert_eq!(outcome, RawEditOutcome::Changed);
        assert_eq!(out, "[V.A]\n\tr = old\n\tr = new\n");
    }

    #[test]
    fn quoted_subsection_stays_case_sensitive_for_insert() {
        let src = "[V \"a\"]\n\tx = old\n";
        let (out, outcome) = edit(src, "V", Some("A"), "r", Some("new"), None, None, false);
        assert_eq!(outcome, RawEditOutcome::Changed);
        assert_eq!(out, "[V \"a\"]\n\tx = old\n[V \"A\"]\n\tr = new\n");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod section_tests {
    use super::*;

    fn rr(src: &str, old: &str, new: Option<&str>) -> SectionEditOutcome {
        rename_or_remove_section(src.as_bytes(), old, new)
    }

    #[test]
    fn rename_quoted_and_dotted_forms() {
        let src = "# Hallo\n\t#Bello\n[branch \"eins\"]\n\tx = 1\n[branch.eins]\n\ty = 1\n\t[branch \"1 234 blabl/a\"]\nweird\n";
        let SectionEditOutcome::Changed(out) = rr(src, "branch.eins", Some("branch.zwei")) else {
            panic!("expected Changed");
        };
        let expect = "# Hallo\n\t#Bello\n[branch \"zwei\"]\n\tx = 1\n[branch \"zwei\"]\n\ty = 1\n\t[branch \"1 234 blabl/a\"]\nweird\n";
        assert_eq!(String::from_utf8(out).unwrap(), expect);
    }

    #[test]
    fn rename_inline_var_indents_remainder() {
        let src = "[branch \"vier\"] z = 1\n";
        let SectionEditOutcome::Changed(out) = rr(src, "branch.vier", Some("branch.zwei")) else {
            panic!("expected Changed");
        };
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "[branch \"zwei\"]\n\tz = 1\n"
        );
    }

    #[test]
    fn remove_section_drops_lines() {
        let src = "[a]\n\tx = 1\n[b]\n\ty = 2\n";
        let SectionEditOutcome::Changed(out) = rr(src, "a", None) else {
            panic!("expected Changed");
        };
        assert_eq!(String::from_utf8(out).unwrap(), "[b]\n\ty = 2\n");
    }

    #[test]
    fn rename_nonexistent_is_not_found() {
        let src = "[a]\n\tx = 1\n";
        assert!(matches!(
            rr(src, "zzz", Some("q.r")),
            SectionEditOutcome::NotFound
        ));
    }
}
