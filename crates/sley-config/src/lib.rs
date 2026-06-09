//! git-config — Git's configuration system: parsing, serialization, value
//! typing, and conditional includes.
//!
//! This crate owns the [`GitConfig`] document model ([`ConfigSection`] /
//! [`ConfigEntry`]), the character-level parser that mirrors git's
//! `git_parse_source`, the canonical writer, the typed value accessors
//! ([`parse_config_bool`], [`parse_config_int`], [`parse_config_bool_or_int`]),
//! and the `include`/`includeIf` resolution machinery
//! ([`load_config_with_includes`], [`ConfigIncludeContext`]).

use sley_core::{GitError, ObjectFormat, Result};
use std::fs;
use std::path::{Path, PathBuf};

/// Structured editing of `[remote "<name>"]` configuration (the document
/// half of `git remote add`/`remove`/`set-url`).
pub mod remotes;

/// A preserved comment or blank line from the source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigPreambleLine {
    /// Full-line `#` or `;` comment (sigil + text; whitespace after sigil is not stored).
    Comment { sigil: char, text: String },
    /// A blank line.
    Blank,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GitConfig {
    /// Comments and blank lines before the first section header.
    pub preamble: Vec<ConfigPreambleLine>,
    pub sections: Vec<ConfigSection>,
    /// Comments and blank lines after the last entry (uncommon but valid).
    pub suffix: Vec<ConfigPreambleLine>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigSection {
    pub name: String,
    pub subsection: Option<String>,
    /// Comments and blank lines after this section header and before its first entry.
    pub preamble: Vec<ConfigPreambleLine>,
    pub entries: Vec<ConfigEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigEntry {
    /// Comments and blank lines immediately preceding this entry within its section.
    pub preamble: Vec<ConfigPreambleLine>,
    /// Leading whitespace before the variable name (defaults to a tab).
    pub indent: String,
    pub key: String,
    pub value: Option<String>,
    pub comment: Option<String>,
}

impl ConfigEntry {
    /// Build a programmatic entry (no preserved preamble/comment).
    pub fn new(key: impl Into<String>, value: Option<String>) -> Self {
        Self {
            preamble: Vec::new(),
            indent: "\t".to_string(),
            key: key.into(),
            value,
            comment: None,
        }
    }
}

impl ConfigSection {
    /// Build a programmatic section (no preserved preamble).
    pub fn new(name: impl Into<String>, subsection: Option<String>, entries: Vec<ConfigEntry>) -> Self {
        Self {
            name: name.into(),
            subsection,
            preamble: Vec::new(),
            entries,
        }
    }
}

impl GitConfig {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let text =
            std::str::from_utf8(bytes).map_err(|err| GitError::InvalidFormat(err.to_string()))?;
        ConfigParser::new(text).parse()
    }

    /// Parse a config file, returning any successfully parsed material even when a
    /// later line is invalid. Used by `git config --list` to mirror git's behaviour
    /// of printing valid entries before reporting a syntax error.
    pub fn parse_collecting(bytes: &[u8]) -> Result<(Self, Option<GitError>)> {
        let text =
            std::str::from_utf8(bytes).map_err(|err| GitError::InvalidFormat(err.to_string()))?;
        Ok(ConfigParser::new(text).parse_collecting())
    }

    pub fn read(path: impl AsRef<Path>) -> Result<Self> {
        Self::parse(&fs::read(path)?)
    }

    /// Return the last value set for `section[.subsection].key`, or `None` if the
    /// key is unset.
    ///
    /// Matches git's "last one wins" precedence: later definitions in the file (and
    /// later files, once includes are spliced) override earlier ones. Section names
    /// and variable names are compared case-insensitively, while subsection names
    /// are matched exactly (case-sensitive), as required by the gitconfig format.
    ///
    /// A bare key with no `=` (a boolean-true variable) has `value == None`, so this
    /// returns `None` for it just as it does for an unset key; use
    /// [`GitConfig::get_bool`] to distinguish those cases.
    pub fn get(&self, section: &str, subsection: Option<&str>, key: &str) -> Option<&str> {
        self.sections
            .iter()
            .rev()
            .filter(|candidate| {
                eq_ignore_ascii_case(&candidate.name, section)
                    && candidate.subsection.as_deref() == subsection
            })
            .flat_map(|candidate| candidate.entries.iter().rev())
            .find(|entry| eq_ignore_ascii_case(&entry.key, key))
            .and_then(|entry| entry.value.as_deref())
    }

    /// Return every value set for `section[.subsection].key`, in file order.
    ///
    /// Multi-valued keys (the same key set several times) are preserved with their
    /// duplicates and original ordering, mirroring git's `--get-all`. Matching
    /// follows the same case rules as [`GitConfig::get`]. A bare boolean-true key
    /// contributes a `None` entry, so callers can tell `key` (present, no value)
    /// apart from `key = value`.
    pub fn get_all(&self, section: &str, subsection: Option<&str>, key: &str) -> Vec<Option<&str>> {
        self.sections
            .iter()
            .filter(|candidate| {
                eq_ignore_ascii_case(&candidate.name, section)
                    && candidate.subsection.as_deref() == subsection
            })
            .flat_map(|candidate| candidate.entries.iter())
            .filter(|entry| eq_ignore_ascii_case(&entry.key, key))
            .map(|entry| entry.value.as_deref())
            .collect()
    }

    /// Interpret the last value of `section[.subsection].key` as a git boolean.
    ///
    /// Returns `None` when the key is unset, and otherwise applies git's
    /// `git_config_bool` rules:
    /// * a bare key with no `=` is `true`;
    /// * `true`/`yes`/`on`/`1` are `true` and `false`/`no`/`off`/`0` are `false`,
    ///   compared case-insensitively;
    /// * an empty value (`key =`) is `false`;
    /// * any other value that parses as an integer is `true` when non-zero and
    ///   `false` when zero.
    ///
    /// A value that is neither a recognised keyword nor an integer yields `None`
    /// (git reports this as a "bad boolean config value" error).
    /// Return the last value of `section[.subsection].key`, distinguishing unset
    /// keys from boolean-true bare keys.
    ///
    /// * `None` — no such key exists;
    /// * `Some(None)` — key exists with no `=` value (boolean true);
    /// * `Some(Some(value))` — key exists with an explicit value (which may be empty).
    pub fn get_entry(
        &self,
        section: &str,
        subsection: Option<&str>,
        key: &str,
    ) -> Option<Option<&str>> {
        self.sections
            .iter()
            .rev()
            .filter(|candidate| {
                eq_ignore_ascii_case(&candidate.name, section)
                    && candidate.subsection.as_deref() == subsection
            })
            .flat_map(|candidate| candidate.entries.iter().rev())
            .find(|entry| eq_ignore_ascii_case(&entry.key, key))
            .map(|entry| entry.value.as_deref())
    }

    pub fn get_bool(&self, section: &str, subsection: Option<&str>, key: &str) -> Option<bool> {
        let entry = self
            .sections
            .iter()
            .rev()
            .filter(|candidate| {
                eq_ignore_ascii_case(&candidate.name, section)
                    && candidate.subsection.as_deref() == subsection
            })
            .flat_map(|candidate| candidate.entries.iter().rev())
            .find(|entry| eq_ignore_ascii_case(&entry.key, key))?;
        match &entry.value {
            // A bare key (no `=`) is boolean true.
            None => Some(true),
            Some(value) => parse_config_bool(value),
        }
    }

    pub fn repository_object_format(&self) -> Result<ObjectFormat> {
        self.get("extensions", None, "objectformat")
            .unwrap_or("sha1")
            .parse()
    }

    /// Serialise the config in git's canonical on-disk form.
    ///
    /// Section headers sit at column 0 as `[section]` or `[section "subsection"]`
    /// (subsections are quoted, with `"` and `\` backslash-escaped). Each entry is
    /// indented with a single tab and written as `key = value`, with the value
    /// quoted/escaped exactly as git would (see [`quote_config_value`]) so the
    /// result round-trips through [`GitConfig::parse`] and matches git's own output
    /// for the common cases. Bare boolean-true keys (value `None`) are written as
    /// just the key.
    ///
    /// Preserved comments and blank lines from [`GitConfig::parse`] are omitted;
    /// use [`GitConfig::to_preserved_bytes`] when rewriting a user-edited file.
    pub fn to_canonical_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for section in &self.sections {
            write_section_header(&mut out, section);
            for entry in &section.entries {
                write_config_entry(&mut out, entry, b"\t");
            }
        }
        out
    }

    /// Serialise while preserving comments, blank lines, and per-entry indentation
    /// captured by [`GitConfig::parse`]. Semantic values are still canonicalised
    /// (quoted/escaped) so parse → edit → write round-trips reliably.
    pub fn to_preserved_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        write_preamble(&mut out, &self.preamble);
        for section in &self.sections {
            write_preamble(&mut out, &section.preamble);
            write_section_header(&mut out, section);
            for entry in &section.entries {
                write_preamble(&mut out, &entry.preamble);
                let indent = if entry.indent.is_empty() {
                    "\t"
                } else {
                    &entry.indent
                };
                write_config_entry(&mut out, entry, indent.as_bytes());
            }
        }
        write_preamble(&mut out, &self.suffix);
        out
    }

    /// Resolve `include`/`includeIf` directives in this already-parsed config.
    ///
    /// `base_dir` is the directory of the file these sections were parsed from;
    /// relative include paths are resolved against it. The returned config has
    /// every include directive replaced (in place, preserving order) by the
    /// parsed-and-resolved contents of the referenced file, so the existing
    /// [`GitConfig::get`]/[`GitConfig::get_bool`] precedence (last value wins)
    /// matches upstream git.
    pub fn resolve_includes(
        &self,
        base_dir: &Path,
        context: &ConfigIncludeContext,
    ) -> Result<GitConfig> {
        let mut resolved = GitConfig::default();
        splice_includes(self, base_dir, context, 0, false, &mut resolved.sections)?;
        Ok(resolved)
    }
}

fn write_preamble(out: &mut Vec<u8>, lines: &[ConfigPreambleLine]) {
    for line in lines {
        match line {
            ConfigPreambleLine::Comment { sigil, text } => {
                out.push(*sigil as u8);
                out.push(b' ');
                out.extend_from_slice(text.as_bytes());
                out.push(b'\n');
            }
            ConfigPreambleLine::Blank => out.push(b'\n'),
        }
    }
}

fn write_section_header(out: &mut Vec<u8>, section: &ConfigSection) {
    out.extend_from_slice(b"[");
    out.extend_from_slice(section.name.as_bytes());
    if let Some(subsection) = &section.subsection {
        out.extend_from_slice(b" \"");
        out.extend_from_slice(escape_config_subsection(subsection).as_bytes());
        out.extend_from_slice(b"\"");
    }
    out.extend_from_slice(b"]\n");
}

fn write_config_entry(out: &mut Vec<u8>, entry: &ConfigEntry, indent: &[u8]) {
    out.extend_from_slice(indent);
    out.extend_from_slice(entry.key.as_bytes());
    if let Some(value) = &entry.value {
        out.extend_from_slice(b" = ");
        out.extend_from_slice(quote_config_value(value).as_bytes());
    }
    if let Some(comment) = &entry.comment {
        out.extend_from_slice(b" # ");
        out.extend_from_slice(comment.as_bytes());
    }
    out.push(b'\n');
}

/// Maximum depth of nested `include`/`includeIf` directives, matching git's
/// `MAX_INCLUDE_DEPTH`.
pub const CONFIG_MAX_INCLUDE_DEPTH: usize = 10;

/// Context used to evaluate conditional `includeIf` directives.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigIncludeContext {
    /// Absolute path to the repository's git directory, used by `gitdir:` conditions.
    pub git_dir: Option<PathBuf>,
    /// Name of the currently checked-out branch, used by `onbranch:` conditions.
    pub current_branch: Option<String>,
}

impl ConfigIncludeContext {
    pub fn new(git_dir: Option<PathBuf>, current_branch: Option<String>) -> Self {
        Self {
            git_dir,
            current_branch,
        }
    }
}

/// Read a config file from disk and resolve its `include`/`includeIf` directives.
///
/// Missing files (including missing *included* files) are treated as empty, which
/// matches git's behaviour of silently ignoring includes that do not exist.
pub fn load_config_with_includes(path: &Path, context: &ConfigIncludeContext) -> Result<GitConfig> {
    let mut sections = Vec::new();
    load_config_file(path, context, 0, false, &mut sections)?;
    Ok(GitConfig {
        preamble: Vec::new(),
        suffix: Vec::new(),
        sections,
    })
}

/// Read and parse a single config file, then splice its includes into `out`.
///
/// A non-existent file contributes nothing (git silently ignores it).
/// When `forbid_remote_url` is set, the file (and any nested includes) must not
/// define `remote.*.url`, matching git's guard for `includeIf.hasconfig` includes.
fn load_config_file(
    path: &Path,
    context: &ConfigIncludeContext,
    depth: usize,
    forbid_remote_url: bool,
    out: &mut Vec<ConfigSection>,
) -> Result<()> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    let parsed = GitConfig::parse(&bytes)?;
    let base_dir = path.parent().unwrap_or_else(|| Path::new("."));
    splice_includes(
        &parsed,
        base_dir,
        context,
        depth,
        forbid_remote_url,
        out,
    )
}

/// Walk the parsed sections in order, copying ordinary sections through and
/// expanding `include`/`includeIf` directives in place.
///
/// `loaded` config for `hasconfig:` conditions is built incrementally: sections
/// already spliced into `out` (from earlier files or parent includes) plus
/// ordinary sections from this file that appear before each `includeIf`.
fn splice_includes(
    parsed: &GitConfig,
    base_dir: &Path,
    context: &ConfigIncludeContext,
    depth: usize,
    forbid_remote_url: bool,
    out: &mut Vec<ConfigSection>,
) -> Result<()> {
    if depth >= CONFIG_MAX_INCLUDE_DEPTH {
        return Err(GitError::InvalidFormat(format!(
            "exceeded maximum config include depth of {CONFIG_MAX_INCLUDE_DEPTH}"
        )));
    }
    if forbid_remote_url {
        reject_remote_urls_in_config(parsed)?;
    }
    let mut loaded = out.to_vec();
    for section in &parsed.sections {
        match include_section_kind(section) {
            Some(IncludeKind::Unconditional) => {
                let before = out.len();
                expand_include_paths(section, base_dir, context, depth, forbid_remote_url, out)?;
                loaded.extend_from_slice(&out[before..]);
            }
            Some(IncludeKind::Conditional(condition)) => {
                if include_condition_matches(condition, base_dir, context, &loaded, parsed) {
                    let before = out.len();
                    let forbid = forbid_remote_url
                        || hasconfig_remote_url_condition(condition);
                    expand_include_paths(section, base_dir, context, depth, forbid, out)?;
                    loaded.extend_from_slice(&out[before..]);
                }
            }
            None => {
                loaded.push(section.clone());
                out.push(section.clone());
            }
        }
    }
    Ok(())
}

/// For an include section, load every `path = <p>` entry in order.
fn expand_include_paths(
    section: &ConfigSection,
    base_dir: &Path,
    context: &ConfigIncludeContext,
    depth: usize,
    forbid_remote_url: bool,
    out: &mut Vec<ConfigSection>,
) -> Result<()> {
    for entry in &section.entries {
        if !eq_ignore_ascii_case(&entry.key, "path") {
            continue;
        }
        let Some(raw) = entry.value.as_deref() else {
            continue;
        };
        if raw.is_empty() {
            continue;
        }
        let resolved = resolve_include_path(raw, base_dir);
        load_config_file(&resolved, context, depth + 1, forbid_remote_url, out)?;
    }
    Ok(())
}

enum IncludeKind<'a> {
    Unconditional,
    Conditional(&'a str),
}

/// Classify a section as an `[include]`, `[includeIf "<cond>"]`, or neither.
fn include_section_kind(section: &ConfigSection) -> Option<IncludeKind<'_>> {
    if !eq_ignore_ascii_case(&section.name, "include")
        && !eq_ignore_ascii_case(&section.name, "includeif")
    {
        return None;
    }
    // `[include]` is unconditional; `[includeIf "..."]` carries the condition in
    // its subsection. An `include` section with a subsection, or an `includeIf`
    // without one, is not a real include directive.
    match (
        eq_ignore_ascii_case(&section.name, "include"),
        &section.subsection,
    ) {
        (true, None) => Some(IncludeKind::Unconditional),
        (false, Some(condition)) => Some(IncludeKind::Conditional(condition)),
        _ => None,
    }
}

/// Resolve an include path string against `~`, the including file's directory,
/// or treat it as absolute.
fn resolve_include_path(raw: &str, base_dir: &Path) -> PathBuf {
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = home_dir() {
            return PathBuf::from(home).join(rest);
        }
        // No usable HOME: fall back to a relative interpretation so the lookup
        // simply misses rather than panicking.
        return base_dir.join(rest);
    }
    let candidate = Path::new(raw);
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        base_dir.join(candidate)
    }
}

/// Evaluate an `includeIf` condition against the context and the config
/// visible at this point in the file.
///
/// `gitdir:` / `onbranch:` use `loaded` (sections already spliced from earlier
/// files/includes plus ordinary sections from the current file that precede
/// this directive). `hasconfig:remote.*.url:` mirrors git's
/// `populate_remote_urls` and inspects every `remote.*.url` in `loaded` plus
/// the entire `current_file` being processed, including sections that appear
/// later in the same file.
fn include_condition_matches(
    condition: &str,
    base_dir: &Path,
    context: &ConfigIncludeContext,
    loaded: &[ConfigSection],
    current_file: &GitConfig,
) -> bool {
    if let Some(pattern) = condition.strip_prefix("gitdir:") {
        return gitdir_condition_matches(pattern, base_dir, context, false);
    }
    if let Some(pattern) = condition.strip_prefix("gitdir/i:") {
        return gitdir_condition_matches(pattern, base_dir, context, true);
    }
    if let Some(pattern) = condition.strip_prefix("onbranch:") {
        return match &context.current_branch {
            Some(branch) => onbranch_pattern_matches(pattern, branch),
            None => false,
        };
    }
    if let Some(glob) = condition.strip_prefix("hasconfig:remote.*.url:") {
        let mut sections = loaded.to_vec();
        sections.extend(current_file.sections.iter().cloned());
        return hasconfig_remote_url_matches(
            &GitConfig {
                preamble: Vec::new(),
                suffix: Vec::new(),
                sections,
            },
            glob,
        );
    }
    // Unknown `hasconfig:` patterns (and any other unrecognised condition) do
    // not match, mirroring upstream git.
    false
}

/// Whether an `includeIf` condition is the supported `hasconfig:remote.*.url:`
/// form (included files must not define `remote.*.url`).
fn hasconfig_remote_url_condition(condition: &str) -> bool {
    condition.starts_with("hasconfig:remote.*.url:")
}

/// Return every `remote.*.url` value in `config`, in file order.
fn collect_remote_urls(config: &GitConfig) -> Vec<&str> {
    config
        .sections
        .iter()
        .filter(|section| eq_ignore_ascii_case(&section.name, "remote"))
        .flat_map(|section| {
            section
                .entries
                .iter()
                .filter(|entry| eq_ignore_ascii_case(&entry.key, "url"))
                .filter_map(|entry| entry.value.as_deref())
        })
        .collect()
}

/// Match a `hasconfig:remote.*.url:<glob>` condition: true when at least one
/// configured remote URL matches `<glob>` (pathname glob semantics).
fn hasconfig_remote_url_matches(config: &GitConfig, glob: &str) -> bool {
    collect_remote_urls(config)
        .into_iter()
        .any(|url| glob_match(glob, url, false))
}

/// Reject configs that set `remote.*.url`, used when expanding files included
/// via `includeIf.hasconfig:remote.*.url`.
fn reject_remote_urls_in_config(config: &GitConfig) -> Result<()> {
    for section in &config.sections {
        if !eq_ignore_ascii_case(&section.name, "remote") || section.subsection.is_none() {
            continue;
        }
        for entry in &section.entries {
            if eq_ignore_ascii_case(&entry.key, "url") {
                return Err(GitError::InvalidFormat(
                    "remote URLs cannot be configured in file directly or indirectly included by includeIf.hasconfig:remote.*.url".into(),
                ));
            }
        }
    }
    Ok(())
}

/// Match a `gitdir:`/`gitdir/i:` pattern against the absolute git directory.
fn gitdir_condition_matches(
    pattern: &str,
    base_dir: &Path,
    context: &ConfigIncludeContext,
    case_insensitive: bool,
) -> bool {
    let Some(git_dir) = &context.git_dir else {
        return false;
    };
    let target = normalize_path_for_match(git_dir);

    // Expand the pattern's own prefixes, then normalise separators.
    let expanded = expand_gitdir_pattern(pattern, base_dir);
    let mut pattern = normalize_separators(&expanded);

    // A trailing slash means "match this directory and everything under it",
    // i.e. an implicit `/**` suffix.
    if pattern.ends_with('/') {
        pattern.push_str("**");
    }
    // A pattern that does not contain a `/` (after expansion) is anchored to the
    // path tail in git; for our supported prefixes the pattern is always rooted,
    // so no extra handling is required here.

    glob_match(&pattern, &target, case_insensitive)
}

/// Look up `$HOME`, returning `None` when it is unset or empty.
fn home_dir() -> Option<String> {
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() => Some(home),
        _ => None,
    }
}

/// Expand the `~/`, `./`, and bare-`**` leading forms of a `gitdir` pattern.
fn expand_gitdir_pattern(pattern: &str, base_dir: &Path) -> String {
    if let Some(rest) = pattern.strip_prefix("~/") {
        if let Some(home) = home_dir() {
            return format!("{home}/{rest}");
        }
        return pattern.to_string();
    }
    if let Some(rest) = pattern.strip_prefix("./") {
        let base = normalize_path_for_match(base_dir);
        let base = base.trim_end_matches('/');
        return format!("{base}/{rest}");
    }
    // A pattern beginning with `**` matches anywhere; leave it as-is.
    pattern.to_string()
}

/// Normalise a path to a forward-slash string for glob comparison.
fn normalize_path_for_match(path: &Path) -> String {
    normalize_separators(&path.to_string_lossy())
}

/// Convert backslashes to forward slashes so matching is separator-agnostic.
fn normalize_separators(value: &str) -> String {
    value.replace('\\', "/")
}

/// Match an `onbranch:` glob against a branch name. A trailing `/` means
/// "everything under this hierarchy" (implicit `/**`), as in git.
fn onbranch_pattern_matches(pattern: &str, branch: &str) -> bool {
    let mut pattern = pattern.to_string();
    if pattern.ends_with('/') {
        pattern.push_str("**");
    }
    glob_match(&pattern, branch, false)
}

/// One token of a compiled glob pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
enum GlobToken {
    /// A literal character that must match exactly.
    Literal(char),
    /// `?` — matches exactly one character that is not `/`.
    AnyChar,
    /// `*` — matches zero or more characters, none of which is `/`.
    Star,
    /// `**` — matches zero or more characters, including `/`.
    DoubleStar,
    /// A `[...]` character class.
    Class {
        negated: bool,
        items: Vec<ClassItem>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ClassItem {
    Single(char),
    Range(char, char),
}

/// Glob matcher supporting `*`, `?`, `[...]` character classes, and `**`.
///
/// `*` matches any run of non-`/` characters; `**` matches across `/`
/// boundaries (including none); `?` matches a single non-`/` character.
fn glob_match(pattern: &str, text: &str, case_insensitive: bool) -> bool {
    let (pattern, text) = if case_insensitive {
        (pattern.to_lowercase(), text.to_lowercase())
    } else {
        (pattern.to_string(), text.to_string())
    };
    let tokens = compile_glob(&pattern);
    let text_chars: Vec<char> = text.chars().collect();
    glob_match_tokens(&tokens, &text_chars)
}

/// Compile a glob string into tokens, handling `\` escapes and `[...]` classes.
fn compile_glob(pattern: &str) -> Vec<GlobToken> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut tokens = Vec::new();
    let mut idx = 0;
    while idx < chars.len() {
        match chars[idx] {
            '*' => {
                if chars.get(idx + 1) == Some(&'*') {
                    tokens.push(GlobToken::DoubleStar);
                    idx += 2;
                } else {
                    tokens.push(GlobToken::Star);
                    idx += 1;
                }
            }
            '?' => {
                tokens.push(GlobToken::AnyChar);
                idx += 1;
            }
            '\\' => {
                if let Some(&next) = chars.get(idx + 1) {
                    tokens.push(GlobToken::Literal(next));
                    idx += 2;
                } else {
                    tokens.push(GlobToken::Literal('\\'));
                    idx += 1;
                }
            }
            '[' => {
                if let Some((token, next)) = compile_char_class(&chars, idx) {
                    tokens.push(token);
                    idx = next;
                } else {
                    // Unterminated class: treat `[` as a literal.
                    tokens.push(GlobToken::Literal('['));
                    idx += 1;
                }
            }
            other => {
                tokens.push(GlobToken::Literal(other));
                idx += 1;
            }
        }
    }
    tokens
}

/// Parse a `[...]` class beginning at `chars[start]`.
///
/// Returns the token and the index just past the closing `]`, or `None` if the
/// class is unterminated.
fn compile_char_class(chars: &[char], start: usize) -> Option<(GlobToken, usize)> {
    let mut idx = start + 1;
    let mut negated = false;
    if chars.get(idx) == Some(&'!') || chars.get(idx) == Some(&'^') {
        negated = true;
        idx += 1;
    }
    let mut items = Vec::new();
    let mut first = true;
    while idx < chars.len() {
        let current = chars[idx];
        if current == ']' && !first {
            return Some((GlobToken::Class { negated, items }, idx + 1));
        }
        first = false;
        if chars.get(idx + 1) == Some(&'-')
            && chars.get(idx + 2).is_some()
            && chars.get(idx + 2) != Some(&']')
        {
            items.push(ClassItem::Range(current, chars[idx + 2]));
            idx += 3;
        } else {
            items.push(ClassItem::Single(current));
            idx += 1;
        }
    }
    None
}

/// Recursively match compiled glob tokens against the remaining text.
fn glob_match_tokens(tokens: &[GlobToken], text: &[char]) -> bool {
    let Some((token, rest)) = tokens.split_first() else {
        return text.is_empty();
    };
    match token {
        GlobToken::Literal('/') => {
            // A trailing `/**` also matches the directory itself, so `foo/**`
            // matches `foo` (text already exhausted) as well as its contents.
            if text.is_empty() && rest == [GlobToken::DoubleStar] {
                return true;
            }
            matches!(text.split_first(), Some((&ch, tail)) if ch == '/' && glob_match_tokens(rest, tail))
        }
        GlobToken::Literal(expected) => {
            matches!(text.split_first(), Some((&ch, tail)) if ch == *expected && glob_match_tokens(rest, tail))
        }
        GlobToken::AnyChar => {
            matches!(text.split_first(), Some((&ch, tail)) if ch != '/' && glob_match_tokens(rest, tail))
        }
        GlobToken::Class { negated, items } => {
            matches!(text.split_first(), Some((&ch, tail))
                if ch != '/' && class_matches(items, ch) != *negated && glob_match_tokens(rest, tail))
        }
        GlobToken::Star => {
            // Match zero-or-more non-`/` characters, trying shortest first.
            if glob_match_tokens(rest, text) {
                return true;
            }
            let mut consumed = 0;
            while consumed < text.len() && text[consumed] != '/' {
                consumed += 1;
                if glob_match_tokens(rest, &text[consumed..]) {
                    return true;
                }
            }
            false
        }
        GlobToken::DoubleStar => {
            match rest.split_first() {
                // `**/<rest>` (a full path-component wildcard): match zero or
                // more complete `component/` units. So `a/**/b` matches `a/b`,
                // `a/x/b`, `a/x/y/b`, and a leading `**/foo` matches `foo` at
                // any depth.
                Some((GlobToken::Literal('/'), after_slash)) => {
                    // Zero directories: the `**/` collapses away entirely.
                    if glob_match_tokens(after_slash, text) {
                        return true;
                    }
                    // One or more directories: consume up to and including the
                    // next `/`, then retry the whole `**/...` against the rest.
                    let mut consumed = 0;
                    while consumed < text.len() {
                        let ch = text[consumed];
                        consumed += 1;
                        if ch == '/' && glob_match_tokens(tokens, &text[consumed..]) {
                            return true;
                        }
                    }
                    false
                }
                // Trailing `**` or `**` before a non-slash: match any run of
                // characters, including `/` and including none.
                _ => {
                    if glob_match_tokens(rest, text) {
                        return true;
                    }
                    for consumed in 1..=text.len() {
                        if glob_match_tokens(rest, &text[consumed..]) {
                            return true;
                        }
                    }
                    false
                }
            }
        }
    }
}

fn class_matches(items: &[ClassItem], ch: char) -> bool {
    items.iter().any(|item| match item {
        ClassItem::Single(value) => *value == ch,
        ClassItem::Range(lo, hi) => *lo <= ch && ch <= *hi,
    })
}

/// Character-level parser for the gitconfig file format.
///
/// This mirrors git's own `git_parse_source`: it scans the input as a stream of
/// characters rather than independent lines, because both line continuations
/// (a trailing `\`) and quoted strings (in values *and* subsection headers) may
/// span physical lines. Section/variable names are matched case-insensitively
/// but stored with their original spelling so rewrites preserve git's casing
/// behavior; subsection names in the quoted form keep their case, while the
/// deprecated dotted form lower-cases the subsection.
struct ConfigParser<'a> {
    chars: std::iter::Peekable<std::str::Chars<'a>>,
    /// 1-based physical line number, advanced on every consumed `\n`.
    line: usize,
}

impl<'a> ConfigParser<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            chars: text.chars().peekable(),
            line: 1,
        }
    }

    fn peek(&mut self) -> Option<char> {
        self.chars.peek().copied()
    }

    /// Consume and return the next character, tracking line numbers.
    fn bump(&mut self) -> Option<char> {
        let ch = self.chars.next();
        if ch == Some('\n') {
            self.line += 1;
        }
        ch
    }

    fn err(&self, message: impl std::fmt::Display) -> GitError {
        GitError::InvalidFormat(format!("config line {}: {message}", self.line))
    }

    /// Skip spaces and tabs (but never newlines), returning the next char if any.
    fn skip_blanks(&mut self) {
        while matches!(self.peek(), Some(' ') | Some('\t') | Some('\r')) {
            self.bump();
        }
    }

    fn parse(self) -> Result<GitConfig> {
        match self.parse_collecting() {
            (config, None) => Ok(config),
            (_, Some(err)) => Err(err),
        }
    }

    fn parse_collecting(mut self) -> (GitConfig, Option<GitError>) {
        let mut config = GitConfig::default();
        let mut current: Option<usize> = None;
        let mut pending_preamble = Vec::new();
        let mut after_section_header = false;
        loop {
            self.skip_blanks();
            match self.peek() {
                None => break,
                Some('\n') => {
                    self.bump();
                    if after_section_header {
                        after_section_header = false;
                    } else {
                        pending_preamble.push(ConfigPreambleLine::Blank);
                    }
                }
                Some('#') | Some(';') => {
                    let sigil = self.bump().expect("peeked comment sigil");
                    pending_preamble.push(ConfigPreambleLine::Comment {
                        sigil,
                        text: self.parse_comment_text(),
                    });
                }
                Some('[') => match self.parse_section_header() {
                    Ok(mut section) => {
                        if config.sections.is_empty() {
                            config.preamble = pending_preamble;
                            section.preamble = Vec::new();
                        } else {
                            section.preamble = pending_preamble;
                        }
                        pending_preamble = Vec::new();
                        config.sections.push(section);
                        current = Some(config.sections.len() - 1);
                        after_section_header = true;
                    }
                    Err(err) => return (config, Some(err)),
                },
                Some(ch) if ch.is_ascii_alphabetic() => match self.parse_entry() {
                    Ok(mut entry) => {
                        entry.preamble = pending_preamble;
                        pending_preamble = Vec::new();
                        after_section_header = false;
                        let Some(idx) = current else {
                            return (
                                config,
                                Some(self.err("variable definition appears before a section")),
                            );
                        };
                        config.sections[idx].entries.push(entry);
                    }
                    Err(err) => return (config, Some(err)),
                },
                Some(ch) => {
                    return (config, Some(self.err(format!("unexpected character {ch:?}"))));
                }
            }
        }
        config.suffix = pending_preamble;
        (config, None)
    }

    /// Parse a `[section]`, `[section "subsection"]`, or deprecated
    /// `[section.subsection]` header. The leading `[` is the next character.
    fn parse_section_header(&mut self) -> Result<ConfigSection> {
        self.bump(); // consume '['
        // Section name: alphanumeric, '-', and '.' (the dotted-subsection form).
        let mut name = String::new();
        while let Some(ch) = self.peek() {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '.' {
                name.push(ch);
                self.bump();
            } else {
                break;
            }
        }
        // Deprecated dotted form: `[section.subsection]`. The subsection runs to
        // the first '.', everything after is the (case-insensitive) subsection.
        if let Some((head, rest)) = name.split_once('.') {
            let subsection = rest.to_string();
            let head = head.to_string();
            self.skip_blanks();
            match self.bump() {
                Some(']') => {}
                _ => return Err(self.err("missing ']' after dotted section header")),
            }
            if !is_config_name(&head) {
                return Err(self.err(format!("invalid section name {head}")));
            }
            // Subsection in the dotted form is lower-cased by git.
            return Ok(ConfigSection {
                name: head.to_ascii_lowercase(),
                subsection: Some(subsection.to_ascii_lowercase()),
                preamble: Vec::new(),
                entries: Vec::new(),
            });
        }
        if !is_config_name(&name) {
            return Err(self.err(format!("invalid section name {name}")));
        }
        // Either a closing ']' or whitespace followed by a quoted subsection.
        match self.peek() {
            Some(']') => {
                self.bump();
                Ok(ConfigSection {
                    name,
                    subsection: None,
                    preamble: Vec::new(),
                    entries: Vec::new(),
                })
            }
            Some(' ') | Some('\t') => {
                self.skip_blanks();
                if self.peek() != Some('"') {
                    return Err(self.err("expected quoted subsection name"));
                }
                let subsection = self.parse_subsection_name()?;
                self.skip_blanks();
                match self.bump() {
                    Some(']') => {}
                    _ => return Err(self.err("missing ']' after subsection name")),
                }
                Ok(ConfigSection {
                    name,
                    // Subsection names are case-sensitive in the quoted form.
                    subsection: Some(subsection),
                    preamble: Vec::new(),
                    entries: Vec::new(),
                })
            }
            _ => Err(self.err("malformed section header")),
        }
    }

    /// Parse the contents of a quoted subsection name (the opening `"` is next).
    ///
    /// Only `\\` and `\"` are escapes here; any other `\<char>` keeps the literal
    /// character (dropping the backslash), and `\n`/`\t` are NOT interpreted.
    fn parse_subsection_name(&mut self) -> Result<String> {
        self.bump(); // consume opening '"'
        let mut out = String::new();
        loop {
            match self.bump() {
                None | Some('\n') => return Err(self.err("unterminated subsection name")),
                Some('"') => return Ok(out),
                Some('\\') => match self.bump() {
                    None | Some('\n') => {
                        return Err(self.err("unterminated subsection name"));
                    }
                    Some(other) => out.push(other),
                },
                Some(other) => out.push(other),
            }
        }
    }

    /// Parse a `name` or `name = value` entry. The first character of the name is
    /// the next character.
    fn parse_entry(&mut self) -> Result<ConfigEntry> {
        let mut indent = String::new();
        while matches!(self.peek(), Some(' ') | Some('\t')) {
            indent.push(self.bump().expect("peeked whitespace"));
        }
        if indent.is_empty() {
            indent.push('\t');
        }
        let mut key = String::new();
        while let Some(ch) = self.peek() {
            if ch.is_ascii_alphanumeric() || ch == '-' {
                key.push(ch);
                self.bump();
            } else {
                break;
            }
        }
        if !is_config_name(&key) {
            return Err(self.err(format!("invalid variable name {key}")));
        }
        self.skip_blanks();
        match self.peek() {
            // Bare variable: boolean true. Nothing but a comment or EOL may follow.
            None => Ok(ConfigEntry {
                preamble: Vec::new(),
                indent,
                key,
                value: None,
                comment: None,
            }),
            Some('\n') => {
                self.bump();
                Ok(ConfigEntry {
                    preamble: Vec::new(),
                    indent,
                    key,
                    value: None,
                    comment: None,
                })
            }
            Some('=') => {
                self.bump();
                let (value, comment) = self.parse_value()?;
                Ok(ConfigEntry {
                    preamble: Vec::new(),
                    indent,
                    key,
                    value: Some(value),
                    comment,
                })
            }
            Some(ch) => Err(self.err(format!("expected '=' after variable name, found {ch:?}"))),
        }
    }

    /// Parse a variable value after the `=`.
    ///
    /// Handles: leading/trailing whitespace trimming (outside quotes), double
    /// quotes that preserve spaces, the escapes `\n \t \b \" \\`, line
    /// continuation via a trailing `\`, and inline `#`/`;` comments (outside
    /// quotes). Quoted runs and unquoted runs may be mixed within one value.
    fn parse_value(&mut self) -> Result<(String, Option<String>)> {
        let mut out = String::new();
        let mut comment = None;
        // Number of trailing whitespace chars currently buffered in `out` that
        // should be dropped if the value ends here (outside quotes).
        let mut trailing_ws = 0usize;
        let mut leading = true;
        let mut in_quotes = false;
        loop {
            match self.peek() {
                None => break,
                Some('\n') if !in_quotes => {
                    self.bump();
                    break;
                }
                Some('\n') => return Err(self.err("newline inside quoted value")),
                Some('"') => {
                    self.bump();
                    in_quotes = !in_quotes;
                    leading = false;
                }
                Some('\\') => {
                    self.bump();
                    match self.bump() {
                        // Line continuation: backslash immediately before a newline.
                        Some('\n') => {}
                        Some('\r') if self.peek() == Some('\n') => {
                            self.bump();
                        }
                        Some('n') => {
                            out.push('\n');
                            trailing_ws = 0;
                            leading = false;
                        }
                        Some('t') => {
                            out.push('\t');
                            trailing_ws = 0;
                            leading = false;
                        }
                        Some('b') => {
                            out.push('\u{0008}');
                            trailing_ws = 0;
                            leading = false;
                        }
                        Some('"') => {
                            out.push('"');
                            trailing_ws = 0;
                            leading = false;
                        }
                        Some('\\') => {
                            out.push('\\');
                            trailing_ws = 0;
                            leading = false;
                        }
                        Some(other) => {
                            return Err(self.err(format!("invalid escape sequence \\{other}")));
                        }
                        // A backslash right at end-of-input is a continuation with
                        // nothing to continue onto; git tolerates this.
                        None => break,
                    }
                }
                // Comments terminate an unquoted value.
                Some('#') | Some(';') if !in_quotes => {
                    self.bump();
                    comment = Some(self.parse_comment_text());
                    break;
                }
                Some(ch @ (' ' | '\t')) if !in_quotes => {
                    self.bump();
                    if leading {
                        // Drop leading whitespace entirely.
                    } else {
                        out.push(ch);
                        trailing_ws += 1;
                    }
                }
                Some('\r') if !in_quotes => {
                    self.bump();
                    match self.peek() {
                        Some('\n') | None => {
                            // Trailing CR before end-of-line is a line ending, not value data.
                        }
                        _ if !leading => {
                            out.push('\r');
                            trailing_ws = 0;
                            leading = false;
                        }
                        _ => {}
                    }
                }
                Some(ch) => {
                    self.bump();
                    out.push(ch);
                    trailing_ws = 0;
                    leading = false;
                }
            }
        }
        if in_quotes {
            return Err(self.err("unterminated quoted value"));
        }
        // Trim trailing unquoted whitespace that was buffered.
        out.truncate(out.len() - trailing_ws);
        Ok((out, comment))
    }

    fn parse_comment_text(&mut self) -> String {
        while matches!(self.peek(), Some(' ') | Some('\t')) {
            self.bump();
        }
        let mut comment = String::new();
        while let Some(ch) = self.peek() {
            if ch == '\n' {
                self.bump();
                break;
            }
            comment.push(ch);
            self.bump();
        }
        comment
    }
}

/// Quote and escape a config value the way git's writer does.
///
/// The value is wrapped in double quotes only when it begins or ends with a space
/// or contains a `#` or `;` (which would otherwise start a comment). Independently
/// of quoting, `\` becomes `\\`, `"` becomes `\"`, tab becomes `\t`, and newline
/// becomes `\n`; other characters (including backspace) are emitted verbatim, just
/// as git does. The result always round-trips back through the parser to the
/// original value.
fn quote_config_value(value: &str) -> String {
    let needs_quotes = value.starts_with(' ')
        || value.ends_with(' ')
        || value.contains('\r')
        || value.bytes().any(|byte| matches!(byte, b'#' | b';'));
    let mut out = String::new();
    if needs_quotes {
        out.push('"');
    }
    for ch in value.chars() {
        match ch {
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            other => out.push(other),
        }
    }
    if needs_quotes {
        out.push('"');
    }
    out
}

/// Escape a subsection name for a `[section "subsection"]` header.
///
/// Only `\` and `"` are escaped (to `\\` and `\"`); all other characters are
/// emitted verbatim, matching git's section-header writer. (Newlines and tabs
/// cannot legally appear in a subsection name.)
fn escape_config_subsection(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            other => out.push(other),
        }
    }
    out
}

/// Parse a string as a git boolean, returning `None` if it is not a valid boolean.
///
/// Implements git's `git_config_bool` rules so the CLI can share one source of
/// truth. The keywords `true`/`yes`/`on`/`1` are `true` and `false`/`no`/`off`/`0`
/// are `false` (case-insensitive). An empty string is `false`. Any other value
/// that parses as an integer (see [`parse_config_int`]) is `true` when non-zero
/// and `false` when zero; everything else returns `None`.
///
/// Note: a *bare* key with no `=` is boolean `true`, but that is represented as a
/// `None` value at the [`ConfigEntry`] level and handled by [`GitConfig::get_bool`];
/// this function only classifies an explicit value string.
pub fn parse_config_bool(value: &str) -> Option<bool> {
    let trimmed = value.trim();
    if trimmed.eq_ignore_ascii_case("true")
        || trimmed.eq_ignore_ascii_case("yes")
        || trimmed.eq_ignore_ascii_case("on")
        || trimmed == "1"
    {
        return Some(true);
    }
    if trimmed.eq_ignore_ascii_case("false")
        || trimmed.eq_ignore_ascii_case("no")
        || trimmed.eq_ignore_ascii_case("off")
        || trimmed == "0"
        || trimmed.is_empty()
    {
        return Some(false);
    }
    // Fall back to git's bool-from-int behaviour: any integer is true unless zero.
    parse_config_int(trimmed).map(|number| number != 0)
}

/// Parse a string as a git integer, returning `None` if it is not a valid integer.
///
/// Implements git's `git_parse_long`/unit handling so the CLI can share one source
/// of truth. A single trailing `k`/`m`/`g` suffix (case-insensitive) multiplies by
/// 1024, 1024², or 1024³ respectively. Decimal (optionally signed), hexadecimal
/// (`0x`), and octal (`0`-prefixed) bases are accepted, just like `strtol`.
/// Overflow on the multiplication or the base parse yields `None`.
pub fn parse_config_int(value: &str) -> Option<i64> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let (digits, multiplier) = match trimmed.as_bytes().last().copied() {
        Some(b'k' | b'K') => (&trimmed[..trimmed.len() - 1], 1024_i64),
        Some(b'm' | b'M') => (&trimmed[..trimmed.len() - 1], 1024_i64 * 1024),
        Some(b'g' | b'G') => (&trimmed[..trimmed.len() - 1], 1024_i64 * 1024 * 1024),
        _ => (trimmed, 1_i64),
    };
    // git requires the unit suffix to immediately follow the digits (no space),
    // so `digits` is parsed as-is rather than re-trimmed.
    parse_c_long(digits)?.checked_mul(multiplier)
}

/// Parse an optionally-signed integer in decimal, hex (`0x`), or octal (`0`)
/// notation, mirroring C's `strtol` with base 0 as git uses for config integers.
fn parse_c_long(text: &str) -> Option<i64> {
    let (negative, rest) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    if rest.is_empty() {
        return None;
    }
    let magnitude = if let Some(hex) = rest.strip_prefix("0x").or_else(|| rest.strip_prefix("0X")) {
        i64::from_str_radix(hex, 16).ok()?
    } else if rest.len() > 1 && rest.starts_with('0') {
        i64::from_str_radix(rest, 8).ok()?
    } else {
        rest.parse::<i64>().ok()?
    };
    if negative {
        magnitude.checked_neg()
    } else {
        Some(magnitude)
    }
}

/// The result of interpreting a config value with git's `--bool-or-int` typing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigBoolOrInt {
    /// The value is a recognised boolean keyword (`true`/`false`/`yes`/...).
    Bool(bool),
    /// The value is an integer (possibly with a `k`/`m`/`g` unit suffix).
    Int(i64),
}

/// Parse a string with git's `--bool-or-int` typing rules.
///
/// A value that is a boolean *keyword* (`true`/`false`/`yes`/`no`/`on`/`off`, or an
/// empty string) is returned as [`ConfigBoolOrInt::Bool`]; otherwise an integer
/// value (see [`parse_config_int`]) is returned as [`ConfigBoolOrInt::Int`]. The
/// bare numbers `0` and `1` are treated as integers, matching git. An empty string
/// is `Bool(false)` (as git treats `key =`). Anything that is neither a boolean
/// keyword nor an integer returns `None`.
pub fn parse_config_bool_or_int(value: &str) -> Option<ConfigBoolOrInt> {
    let trimmed = value.trim();
    if trimmed.eq_ignore_ascii_case("true")
        || trimmed.eq_ignore_ascii_case("yes")
        || trimmed.eq_ignore_ascii_case("on")
    {
        return Some(ConfigBoolOrInt::Bool(true));
    }
    if trimmed.eq_ignore_ascii_case("false")
        || trimmed.eq_ignore_ascii_case("no")
        || trimmed.eq_ignore_ascii_case("off")
        || trimmed.is_empty()
    {
        return Some(ConfigBoolOrInt::Bool(false));
    }
    parse_config_int(trimmed).map(ConfigBoolOrInt::Int)
}

fn is_config_name(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

fn eq_ignore_ascii_case(left: &str, right: &str) -> bool {
    left.eq_ignore_ascii_case(right)
}

/// Load the *effective* configuration for a repository, merging the system,
/// global, and repository config files in git's precedence order.
///
/// The returned [`GitConfig`] concatenates the sections of each file, lowest
/// precedence first (system, then global, then repository), so the existing
/// last-one-wins semantics of [`GitConfig::get`]/[`GitConfig::get_bool`] yield
/// the correct effective value and [`GitConfig::get_all`] returns every value in
/// git's order. `include`/`includeIf` directives are resolved per file via
/// [`load_config_with_includes`] using `context`.
///
/// File discovery mirrors git exactly:
/// * **system**: `$GIT_CONFIG_SYSTEM` when set, otherwise `/etc/gitconfig`. The
///   system file is skipped entirely when `GIT_CONFIG_NOSYSTEM` is set to a
///   git-true boolean (e.g. `1`).
/// * **global**: `$GIT_CONFIG_GLOBAL` when set (used on its own), otherwise both
///   `$XDG_CONFIG_HOME/git/config` (falling back to `~/.config/git/config`) and
///   then `~/.gitconfig` — the latter taking precedence.
/// * **repository**: `<common_git_dir>/config`.
///
/// `~` is expanded using `$HOME`. Missing files are skipped silently, matching
/// git's behaviour. This does **not** include `-c`/`GIT_CONFIG_COUNT`
/// command-line overrides, which are higher precedence and layered on by the
/// caller (the CLI) on top of this result.
pub fn load_effective_config(
    common_git_dir: &Path,
    context: &ConfigIncludeContext,
) -> Result<GitConfig> {
    let mut sections = Vec::new();
    for path in effective_config_paths(common_git_dir) {
        load_config_file(&path, context, 0, false, &mut sections)?;
    }
    Ok(GitConfig {
        preamble: Vec::new(),
        suffix: Vec::new(),
        sections,
    })
}

/// Load the config layers consulted before command dispatch (alias resolution):
/// system, global, and repository when `common_git_dir` is known.
///
/// This mirrors git's pre-command config lookup: outside a repository only
/// system and global files are read; inside a repository the local config is
/// included as well. Command-line `-c` overrides are *not* included here — the
/// caller layers those on top via [`GitConfig::get`] precedence or a separate
/// override lookup.
pub fn load_pre_dispatch_config(
    common_git_dir: Option<&Path>,
    context: &ConfigIncludeContext,
) -> Result<GitConfig> {
    let mut sections = Vec::new();
    for path in pre_dispatch_config_paths(common_git_dir) {
        load_config_file(&path, context, 0, false, &mut sections)?;
    }
    Ok(GitConfig {
        preamble: Vec::new(),
        suffix: Vec::new(),
        sections,
    })
}

/// Config file paths for pre-dispatch lookup (system, global, optional repo).
fn pre_dispatch_config_paths(common_git_dir: Option<&Path>) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(system) = system_config_path() {
        paths.push(system);
    }
    paths.extend(global_config_paths());
    if let Some(common_git_dir) = common_git_dir {
        paths.push(common_git_dir.join("config"));
    }
    paths
}

/// Compute the ordered list of config files that make up the effective config,
/// lowest precedence (system) first and highest (repository) last.
fn effective_config_paths(common_git_dir: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(system) = system_config_path() {
        paths.push(system);
    }
    paths.extend(global_config_paths());
    paths.push(common_git_dir.join("config"));
    paths
}

/// The system config file, or `None` when `GIT_CONFIG_NOSYSTEM` disables it.
fn system_config_path() -> Option<PathBuf> {
    if env_bool("GIT_CONFIG_NOSYSTEM") {
        return None;
    }
    match non_empty_env("GIT_CONFIG_SYSTEM") {
        Some(path) => Some(PathBuf::from(path)),
        None => Some(PathBuf::from("/etc/gitconfig")),
    }
}

/// The global config file(s), in precedence order (XDG first, `~/.gitconfig`
/// last so it wins). When `$GIT_CONFIG_GLOBAL` is set it replaces both.
fn global_config_paths() -> Vec<PathBuf> {
    if let Some(global) = non_empty_env("GIT_CONFIG_GLOBAL") {
        return vec![PathBuf::from(global)];
    }
    let mut paths = Vec::new();
    if let Some(xdg) = xdg_config_path() {
        paths.push(xdg);
    }
    if let Some(home) = home_dir() {
        paths.push(PathBuf::from(home).join(".gitconfig"));
    }
    paths
}

/// `$XDG_CONFIG_HOME/git/config`, falling back to `~/.config/git/config`.
fn xdg_config_path() -> Option<PathBuf> {
    if let Some(xdg) = non_empty_env("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(xdg).join("git").join("config"));
    }
    home_dir().map(|home| {
        PathBuf::from(home)
            .join(".config")
            .join("git")
            .join("config")
    })
}

/// Read an environment variable, treating unset and empty as absent (git's
/// convention for path-valued environment variables).
fn non_empty_env(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(value) if !value.is_empty() => Some(value),
        _ => None,
    }
}

/// Evaluate an environment variable as a git boolean (`git_env_bool` with a
/// default of false): unset is false, and a set value is parsed with
/// [`parse_config_bool`] (an unrecognised value is treated as true, matching
/// git's `git_config_bool_or_int` fallback for non-empty strings).
fn env_bool(name: &str) -> bool {
    match std::env::var(name) {
        Ok(value) => parse_config_bool(&value).unwrap_or(!value.is_empty()),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn config_parses_sections_values_and_comments() {
        let config = GitConfig::parse(
            br#"
[core]
    filemode = true
    bare = false ; comment
[remote "origin"]
    url = "https://example.invalid/repo.git"
    fetch = +refs/heads/*:refs/remotes/origin/*
[feature]
    enabled
"#,
        )
        .expect("test operation should succeed");
        assert_eq!(config.get_bool("core", None, "filemode"), Some(true));
        assert_eq!(config.get_bool("core", None, "bare"), Some(false));
        assert_eq!(
            config.get("remote", Some("origin"), "url"),
            Some("https://example.invalid/repo.git")
        );
        assert_eq!(config.get_bool("feature", None, "enabled"), Some(true));
    }

    #[test]
    fn config_reports_repository_object_format() {
        let config = GitConfig::parse(b"[extensions]\n\tobjectformat = sha256\n")
            .expect("test operation should succeed");
        assert_eq!(
            config
                .repository_object_format()
                .expect("test operation should succeed"),
            ObjectFormat::Sha256
        );
    }

    #[test]
    fn config_canonical_writer_round_trips() {
        let config = GitConfig {
            preamble: Vec::new(),
            suffix: Vec::new(),
            sections: vec![ConfigSection::new(
                "remote",
                Some("origin repo".into()),
                vec![ConfigEntry::new(
                    "url",
                    Some("https://example.invalid/repo.git".into()),
                )],
            )],
        };
        let parsed =
            GitConfig::parse(&config.to_canonical_bytes()).expect("test operation should succeed");
        assert_eq!(parsed, config);
    }

    // ----- gitconfig format compliance tests -----

    /// Convenience: parse and fetch the single `core.x` value (panicking on parse
    /// errors is fine here because each input is a known-good fixture).
    fn parse_core_x(input: &str) -> Option<String> {
        GitConfig::parse(input.as_bytes())
            .expect("test operation should succeed")
            .get("core", None, "x")
            .map(str::to_string)
    }

    #[test]
    fn config_section_name_is_case_insensitive() {
        let config =
            GitConfig::parse(b"[Core]\n\tBar = value\n").expect("test operation should succeed");
        assert_eq!(config.get("core", None, "bar"), Some("value"));
        assert_eq!(config.get("CORE", None, "BAR"), Some("value"));
        // Stored names preserve spelling for faithful rewrites.
        assert_eq!(config.sections[0].name, "Core");
        assert_eq!(config.sections[0].entries[0].key, "Bar");
    }

    #[test]
    fn config_subsection_name_is_case_sensitive() {
        let config = GitConfig::parse(b"[remote \"Origin\"]\n\turl = x\n")
            .expect("test operation should succeed");
        assert_eq!(config.get("remote", Some("Origin"), "url"), Some("x"));
        // Case-mismatched subsection must not match.
        assert_eq!(config.get("remote", Some("origin"), "url"), None);
    }

    #[test]
    fn config_subsection_accepts_escaped_quote_and_backslash() {
        // [remote "with\"quote"] -> subsection is with"quote
        let config = GitConfig::parse(b"[remote \"with\\\"quote\"]\n\turl = x\n")
            .expect("test operation should succeed");
        assert_eq!(
            config.sections[0].subsection.as_deref(),
            Some("with\"quote")
        );
        assert_eq!(config.get("remote", Some("with\"quote"), "url"), Some("x"));

        // [remote "a\\b"] -> subsection is a\b
        let config = GitConfig::parse(b"[remote \"a\\\\b\"]\n\turl = y\n")
            .expect("test operation should succeed");
        assert_eq!(config.sections[0].subsection.as_deref(), Some("a\\b"));
    }

    #[test]
    fn config_subsection_unknown_escape_keeps_literal_char() {
        // In a subsection only \\ and \" are real escapes; \n is a literal "n",
        // NOT a newline (unlike a value).
        let config = GitConfig::parse(b"[remote \"a\\nb\"]\n\turl = x\n")
            .expect("test operation should succeed");
        assert_eq!(config.sections[0].subsection.as_deref(), Some("anb"));
    }

    #[test]
    fn config_dotted_subsection_is_case_insensitive() {
        // Deprecated [section.subsection] form: subsection is lower-cased.
        let config =
            GitConfig::parse(b"[core.Sub]\n\tbar = x\n").expect("test operation should succeed");
        assert_eq!(config.sections[0].name, "core");
        assert_eq!(config.sections[0].subsection.as_deref(), Some("sub"));
        assert_eq!(config.get("core", Some("sub"), "bar"), Some("x"));
        // The original (mixed) case must not match.
        assert_eq!(config.get("core", Some("Sub"), "bar"), None);
    }

    #[test]
    fn config_dotted_subsection_keeps_inner_dots() {
        // Everything after the first dot is the subsection, dots and all.
        let config =
            GitConfig::parse(b"[a.b.c]\n\tk = v\n").expect("test operation should succeed");
        assert_eq!(config.sections[0].name, "a");
        assert_eq!(config.sections[0].subsection.as_deref(), Some("b.c"));
    }

    #[test]
    fn config_bare_variable_is_boolean_true() {
        let config = GitConfig::parse(b"[core]\n\tflag\n").expect("test operation should succeed");
        assert_eq!(config.sections[0].entries[0].value, None);
        assert_eq!(config.get_bool("core", None, "flag"), Some(true));
        // A bare key has no string value.
        assert_eq!(config.get("core", None, "flag"), None);
    }

    #[test]
    fn config_explicit_empty_value_is_boolean_false() {
        // `x =` (with the equals) is an empty value, which git treats as false,
        // distinct from a bare key with no equals (true).
        let config = GitConfig::parse(b"[core]\n\tx =\n").expect("test operation should succeed");
        assert_eq!(config.sections[0].entries[0].value.as_deref(), Some(""));
        assert_eq!(config.get_bool("core", None, "x"), Some(false));
    }

    #[test]
    fn config_value_unquoted_trims_surrounding_whitespace() {
        assert_eq!(
            parse_core_x("[core]\n\tx =    hello world   \n").as_deref(),
            Some("hello world")
        );
    }

    #[test]
    fn config_value_quotes_preserve_spaces() {
        assert_eq!(
            parse_core_x("[core]\n\tx = \"  spaced  \"\n").as_deref(),
            Some("  spaced  ")
        );
    }

    #[test]
    fn config_value_mixes_quoted_and_unquoted_runs() {
        assert_eq!(
            parse_core_x("[core]\n\tx = a\" b \"c\n").as_deref(),
            Some("a b c")
        );
        assert_eq!(
            parse_core_x("[core]\n\tx = \"ab\"   cd\n").as_deref(),
            Some("ab   cd")
        );
        assert_eq!(
            parse_core_x("[core]\n\tx = a\"\"b\n").as_deref(),
            Some("ab")
        );
    }

    #[test]
    fn config_value_processes_escapes_in_unquoted_and_quoted() {
        // Escapes are processed in both unquoted and quoted values.
        assert_eq!(
            parse_core_x("[core]\n\tx = a\\tb\\nc\n").as_deref(),
            Some("a\tb\nc")
        );
        assert_eq!(
            parse_core_x("[core]\n\tx = \"a\\tb\"\n").as_deref(),
            Some("a\tb")
        );
        assert_eq!(
            parse_core_x("[core]\n\tx = a\\bb\n").as_deref(),
            Some("a\u{8}b")
        );
        assert_eq!(
            parse_core_x("[core]\n\tx = a\\\"b\n").as_deref(),
            Some("a\"b")
        );
        assert_eq!(
            parse_core_x("[core]\n\tx = a\\\\b\n").as_deref(),
            Some("a\\b")
        );
    }

    #[test]
    fn config_value_rejects_unknown_escape() {
        // \z is not a valid escape, in either quoted or unquoted values.
        assert!(GitConfig::parse(b"[core]\n\tx = a\\zb\n").is_err());
        assert!(GitConfig::parse(b"[core]\n\tx = \"a\\zb\"\n").is_err());
    }

    #[test]
    fn config_value_line_continuation_joins_lines() {
        // A trailing backslash continues the value onto the next physical line.
        assert_eq!(
            parse_core_x("[core]\n\tx = a\\\n b\n").as_deref(),
            Some("a b")
        );
    }

    #[test]
    fn config_value_continuation_inside_quotes() {
        // The continuation also works inside a quoted span.
        assert_eq!(
            parse_core_x("[core]\n\tx = \"a\\\n b\"\n").as_deref(),
            Some("a b")
        );
    }

    #[test]
    fn config_value_inline_comments_stripped_outside_quotes() {
        assert_eq!(
            parse_core_x("[core]\n\tx = val ; comment\n").as_deref(),
            Some("val")
        );
        assert_eq!(
            parse_core_x("[core]\n\tx = val # comment\n").as_deref(),
            Some("val")
        );
        // Comment characters inside quotes are literal.
        assert_eq!(
            parse_core_x("[core]\n\tx = \"a#b\"\n").as_deref(),
            Some("a#b")
        );
        assert_eq!(
            parse_core_x("[core]\n\tx = \"ab\" ; c\n").as_deref(),
            Some("ab")
        );
    }

    #[test]
    fn config_bare_key_with_inline_comment_is_error() {
        // git rejects a comment after a value-less key.
        assert!(GitConfig::parse(b"[core]\n\tflag ; comment\n").is_err());
        assert!(GitConfig::parse(b"[core]\n\tflag # comment\n").is_err());
    }

    #[test]
    fn config_unterminated_quote_is_error() {
        assert!(GitConfig::parse(b"[core]\n\tx = \"ab\n").is_err());
    }

    #[test]
    fn config_trailing_backslash_at_eof_is_tolerated() {
        // A trailing backslash with no following line just ends the value.
        assert_eq!(parse_core_x("[core]\n\tx = a\\").as_deref(), Some("a"));
    }

    #[test]
    fn config_handles_crlf_line_endings() {
        assert_eq!(parse_core_x("[core]\r\n\tx = y\r\n").as_deref(), Some("y"));
    }

    #[test]
    fn config_value_preserves_trailing_cr() {
        assert_eq!(
            parse_core_x("[core]\n\tx = \"bar\r\"\n").as_deref(),
            Some("bar\r")
        );
        let config = GitConfig::parse(b"[core]\n\tx = \"bar\r\"\n")
            .expect("test operation should succeed");
        assert_eq!(
            String::from_utf8(config.to_canonical_bytes()).expect("utf8"),
            "[core]\n\tx = \"bar\r\"\n"
        );
        let config = GitConfig {
            sections: vec![ConfigSection::new(
                String::from("core"),
                None,
                vec![ConfigEntry::new(
                    String::from("foo"),
                    Some(format!("bar{}", '\r')),
                )],
            )],
            ..GitConfig::default()
        };
        assert_eq!(
            String::from_utf8(config.to_canonical_bytes()).expect("utf8"),
            "[core]\n\tfoo = \"bar\r\"\n"
        );
    }

    #[test]
    fn config_no_spaces_around_equals() {
        assert_eq!(parse_core_x("[core]\n\tx=y\n").as_deref(), Some("y"));
    }

    #[test]
    fn config_multi_valued_keys_preserve_order_and_duplicates() {
        let config = GitConfig::parse(b"[core]\n\tx = 1\n\tx = 2\n\tx = 1\n")
            .expect("test operation should succeed");
        assert_eq!(
            config.get_all("core", None, "x"),
            vec![Some("1"), Some("2"), Some("1")]
        );
        // Last value wins for the scalar getter.
        assert_eq!(config.get("core", None, "x"), Some("1"));
    }

    #[test]
    fn config_get_all_spans_multiple_sections_in_order() {
        let config = GitConfig::parse(b"[core]\n\tx = a\n[other]\n\ty = z\n[core]\n\tx = b\n")
            .expect("test operation should succeed");
        assert_eq!(
            config.get_all("core", None, "x"),
            vec![Some("a"), Some("b")]
        );
    }

    #[test]
    fn config_rejects_value_before_section() {
        assert!(GitConfig::parse(b"\tx = y\n").is_err());
    }

    #[test]
    fn config_rejects_invalid_names() {
        // An underscore is not allowed in section or variable names.
        assert!(GitConfig::parse(b"[core]\n\tx_y = 1\n").is_err());
        assert!(GitConfig::parse(b"[a_b]\n\tx = 1\n").is_err());
    }

    #[test]
    fn config_variable_name_must_start_with_letter() {
        // git requires variable names to begin with an alphabetic character.
        assert!(GitConfig::parse(b"[core]\n\t1x = y\n").is_err());
        assert!(GitConfig::parse(b"[core]\n\t-x = y\n").is_err());
        // ...but a letter followed by digits/hyphens is fine.
        assert_eq!(parse_core_x("[core]\n\tx = ok\n").as_deref(), Some("ok"));
        let config =
            GitConfig::parse(b"[core]\n\tx1-y = z\n").expect("test operation should succeed");
        assert_eq!(config.get("core", None, "x1-y"), Some("z"));
    }

    #[test]
    fn config_section_name_may_start_with_digit() {
        // Unlike variable names, section names may begin with a digit.
        let config =
            GitConfig::parse(b"[1core]\n\tx = y\n").expect("test operation should succeed");
        assert_eq!(config.get("1core", None, "x"), Some("y"));
    }

    #[test]
    fn config_comments_and_blank_lines_are_preserved() {
        let source = b"# top\n; also\n\n[core]\n\n\tx = y # inline\n# trailing\n";
        let config = GitConfig::parse(source).expect("test operation should succeed");
        assert_eq!(config.get("core", None, "x"), Some("y"));
        assert_eq!(
            config.preamble,
            vec![
                ConfigPreambleLine::Comment {
                    sigil: '#',
                    text: "top".into(),
                },
                ConfigPreambleLine::Comment {
                    sigil: ';',
                    text: "also".into(),
                },
                ConfigPreambleLine::Blank,
            ]
        );
        assert_eq!(
            config.sections[0].entries[0].preamble,
            vec![ConfigPreambleLine::Blank]
        );
        assert_eq!(config.sections[0].entries[0].comment.as_deref(), Some("inline"));
        assert_eq!(
            config.suffix,
            vec![ConfigPreambleLine::Comment {
                sigil: '#',
                text: "trailing".into(),
            }]
        );
        let preserved = config.to_preserved_bytes();
        let reparsed = GitConfig::parse(&preserved).expect("test operation should succeed");
        assert_eq!(reparsed, config);
        assert_eq!(preserved, source);
    }

    // ----- bool / int / bool-or-int coercion -----

    #[test]
    fn parse_config_bool_keywords() {
        for truthy in ["true", "TRUE", "yes", "Yes", "on", "ON", "1"] {
            assert_eq!(parse_config_bool(truthy), Some(true), "{truthy}");
        }
        for falsy in ["false", "FALSE", "no", "No", "off", "OFF", "0", ""] {
            assert_eq!(parse_config_bool(falsy), Some(false), "{falsy}");
        }
    }

    #[test]
    fn parse_config_bool_accepts_integers() {
        // Non-zero integers are true, zero is false (git's bool-from-int rule).
        assert_eq!(parse_config_bool("5"), Some(true));
        assert_eq!(parse_config_bool("-3"), Some(true));
        assert_eq!(parse_config_bool("0"), Some(false));
        assert_eq!(parse_config_bool("0x10"), Some(true));
        // Non-numeric, non-keyword strings are not booleans.
        assert_eq!(parse_config_bool("foo"), None);
    }

    #[test]
    fn parse_config_int_units_and_bases() {
        assert_eq!(parse_config_int("1k"), Some(1024));
        assert_eq!(parse_config_int("1K"), Some(1024));
        assert_eq!(parse_config_int("1m"), Some(1024 * 1024));
        assert_eq!(parse_config_int("1M"), Some(1024 * 1024));
        assert_eq!(parse_config_int("2g"), Some(2 * 1024 * 1024 * 1024));
        assert_eq!(parse_config_int("1G"), Some(1024 * 1024 * 1024));
        assert_eq!(parse_config_int("5"), Some(5));
        assert_eq!(parse_config_int("-5"), Some(-5));
        assert_eq!(parse_config_int("0x10"), Some(16));
        assert_eq!(parse_config_int("010"), Some(8));
    }

    #[test]
    fn parse_config_int_rejects_invalid() {
        assert_eq!(parse_config_int(""), None);
        assert_eq!(parse_config_int("foo"), None);
        assert_eq!(parse_config_int("1 k"), None);
        assert_eq!(parse_config_int("1.5"), None);
        // Overflow on the unit multiplication is rejected rather than wrapping.
        assert_eq!(parse_config_int("9999999999999999999g"), None);
    }

    #[test]
    fn parse_config_bool_or_int_typing() {
        assert_eq!(
            parse_config_bool_or_int("yes"),
            Some(ConfigBoolOrInt::Bool(true))
        );
        assert_eq!(
            parse_config_bool_or_int("off"),
            Some(ConfigBoolOrInt::Bool(false))
        );
        // git treats a bare empty value as false here too.
        assert_eq!(
            parse_config_bool_or_int(""),
            Some(ConfigBoolOrInt::Bool(false))
        );
        // Bare numbers (including 0 and 1) are integers, not booleans.
        assert_eq!(parse_config_bool_or_int("5"), Some(ConfigBoolOrInt::Int(5)));
        assert_eq!(parse_config_bool_or_int("0"), Some(ConfigBoolOrInt::Int(0)));
        assert_eq!(parse_config_bool_or_int("1"), Some(ConfigBoolOrInt::Int(1)));
        assert_eq!(
            parse_config_bool_or_int("1k"),
            Some(ConfigBoolOrInt::Int(1024))
        );
        assert_eq!(parse_config_bool_or_int("foo"), None);
    }

    // ----- serialization / round-trip -----

    #[test]
    fn config_canonical_value_quoting_matches_git() {
        // (value, expected serialized form of the value portion)
        let cases = [
            ("simple", "simple"),
            ("a b c", "a b c"),         // internal spaces: no quotes
            ("  lead", "\"  lead\""),   // leading space: quote
            ("trail  ", "\"trail  \""), // trailing space: quote
            ("a#b", "\"a#b\""),         // '#' forces quotes
            ("a;b", "\"a;b\""),         // ';' forces quotes
            ("a\"b", "a\\\"b"),         // embedded quote: escape, no surrounding quotes
            ("a\\b", "a\\\\b"),         // backslash escaped
            ("a\tb", "a\\tb"),          // tab escaped, no surrounding quotes
            ("a\nb", "a\\nb"),          // newline escaped
        ];
        for (value, expected) in cases {
            let config = GitConfig {
                preamble: Vec::new(),
                suffix: Vec::new(),
                sections: vec![ConfigSection::new(
                    "core",
                    None,
                    vec![ConfigEntry::new("x", Some(value.to_string()))],
                )],
            };
            let bytes = config.to_canonical_bytes();
            let text = String::from_utf8(bytes).expect("test operation should succeed");
            let expected_line = format!("\tx = {expected}\n");
            assert!(
                text.contains(&expected_line),
                "value {value:?} serialized to {text:?}, expected to contain {expected_line:?}"
            );
        }
    }

    #[test]
    fn config_subsection_header_only_escapes_quote_and_backslash() {
        let config = GitConfig {
            preamble: Vec::new(),
            suffix: Vec::new(),
            sections: vec![ConfigSection::new(
                "remote",
                Some("a\"b\\c".into()),
                vec![ConfigEntry::new("url", Some("x".into()))],
            )],
        };
        let text =
            String::from_utf8(config.to_canonical_bytes()).expect("test operation should succeed");
        assert!(
            text.starts_with("[remote \"a\\\"b\\\\c\"]\n"),
            "unexpected header: {text:?}"
        );
    }

    #[test]
    fn config_round_trip_is_stable_for_tricky_values() {
        // parse -> serialize -> parse must be a fixpoint and preserve the value.
        let values = [
            "simple",
            "a b c",
            "  leading and trailing  ",
            "with#hash",
            "with;semi",
            "with\"quote",
            "with\\backslash",
            "with\ttab",
            "with\nnewline",
            "  # ; \" \\ \t mixed  ",
            "",
        ];
        for value in values {
            let original = GitConfig {
                preamble: Vec::new(),
                suffix: Vec::new(),
                sections: vec![ConfigSection::new(
                    "core",
                    Some("a b\"c".into()),
                    vec![
                        ConfigEntry::new("x", Some(value.to_string())),
                        // A bare boolean-true key should survive the round trip.
                        ConfigEntry::new("flag", None),
                    ],
                )],
            };
            let serialized = original.to_canonical_bytes();
            let reparsed = GitConfig::parse(&serialized).expect("test operation should succeed");
            assert_eq!(reparsed, original, "value {value:?} did not round-trip");
            // Serializing again must be byte-identical (stable fixpoint).
            assert_eq!(
                reparsed.to_canonical_bytes(),
                serialized,
                "value {value:?} is not a serialization fixpoint"
            );
        }
    }

    #[test]
    fn config_round_trip_preserves_multi_value_order() {
        let original = GitConfig {
            preamble: Vec::new(),
            suffix: Vec::new(),
            sections: vec![ConfigSection::new(
                "core",
                None,
                vec![
                    ConfigEntry::new("x", Some("first".into())),
                    ConfigEntry::new("x", Some("second".into())),
                    ConfigEntry::new("x", Some("first".into())),
                ],
            )],
        };
        let reparsed = GitConfig::parse(&original.to_canonical_bytes())
            .expect("test operation should succeed");
        assert_eq!(reparsed, original);
        assert_eq!(
            reparsed.get_all("core", None, "x"),
            vec![Some("first"), Some("second"), Some("first")]
        );
    }

    fn unique_temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("sley-{name}-{}-{nanos}", std::process::id()))
    }

    /// Build a unique scratch directory under the system temp dir and create it.
    fn unique_include_dir(tag: &str) -> PathBuf {
        let dir = unique_temp_dir(tag);
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn glob_matcher_handles_stars_classes_and_double_star() {
        // Single star does not cross path separators.
        assert!(glob_match("foo*", "foobar", false));
        assert!(glob_match("*bar", "foobar", false));
        assert!(!glob_match("foo*", "foo/bar", false));
        // `?` matches one non-slash char.
        assert!(glob_match("f?o", "foo", false));
        assert!(!glob_match("f?o", "f/o", false));
        // Character classes and ranges.
        assert!(glob_match("[a-c]oo", "boo", false));
        assert!(!glob_match("[a-c]oo", "zoo", false));
        assert!(glob_match("[!a-c]oo", "zoo", false));
        // `**` crosses separators, including zero directories.
        assert!(glob_match("/home/**", "/home/user/work/.git", false));
        assert!(glob_match("/home/**", "/home", false));
        assert!(glob_match("**/foo/.git", "/a/b/foo/.git", false));
        assert!(glob_match("**/foo/.git", "/foo/.git", false));
        assert!(glob_match("a/**/b", "a/b", false));
        assert!(glob_match("a/**/b", "a/x/y/b", false));
        assert!(!glob_match("a/**/b", "a/xb", false));
        // Case-insensitive matching.
        assert!(glob_match("/Home/**", "/home/user/.git", true));
        assert!(!glob_match("/Home/**", "/home/user/.git", false));
    }

    #[test]
    fn config_include_unconditional_merges_and_overrides() {
        let dir = unique_include_dir("inc-uncond");
        let main = dir.join("config");
        let extra = dir.join("extra.cfg");
        fs::write(
            &main,
            format!(
                "[core]\n\tfilemode = false\n[include]\n\tpath = {}\n",
                extra.display()
            ),
        )
        .expect("test operation should succeed");
        // The included file overrides filemode and adds a new value.
        fs::write(&extra, "[core]\n\tfilemode = true\n\tbig = yes\n")
            .expect("test operation should succeed");

        let ctx = ConfigIncludeContext::default();
        let config = load_config_with_includes(&main, &ctx).expect("test operation should succeed");
        assert_eq!(config.get_bool("core", None, "filemode"), Some(true));
        assert_eq!(config.get_bool("core", None, "big"), Some(true));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn config_include_relative_path_resolves_against_including_file() {
        let dir = unique_include_dir("inc-rel");
        let sub = dir.join("sub");
        fs::create_dir_all(&sub).expect("test operation should succeed");
        let main = dir.join("config");
        // Relative path is resolved against the including file's directory.
        fs::write(&main, "[include]\n\tpath = sub/child.cfg\n")
            .expect("test operation should succeed");
        fs::write(sub.join("child.cfg"), "[user]\n\temail = a@b.c\n")
            .expect("test operation should succeed");

        let ctx = ConfigIncludeContext::default();
        let config = load_config_with_includes(&main, &ctx).expect("test operation should succeed");
        assert_eq!(config.get("user", None, "email"), Some("a@b.c"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn config_include_missing_file_is_ignored() {
        let dir = unique_include_dir("inc-missing");
        let main = dir.join("config");
        fs::write(
            &main,
            "[core]\n\tfilemode = true\n[include]\n\tpath = does-not-exist.cfg\n",
        )
        .expect("test operation should succeed");

        let ctx = ConfigIncludeContext::default();
        let config = load_config_with_includes(&main, &ctx).expect("test operation should succeed");
        // No error, and the existing value is preserved.
        assert_eq!(config.get_bool("core", None, "filemode"), Some(true));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn config_include_if_gitdir_match_and_non_match() {
        let dir = unique_include_dir("inc-gitdir");
        let work = dir.join("work");
        fs::create_dir_all(&work).expect("test operation should succeed");
        let main = dir.join("config");
        let work_git = work.join(".git");
        fs::write(
            &main,
            format!(
                "[includeIf \"gitdir:{}/\"]\n\tpath = matched.cfg\n",
                work.display()
            ),
        )
        .expect("test operation should succeed");
        fs::write(dir.join("matched.cfg"), "[user]\n\tname = work\n")
            .expect("test operation should succeed");

        // git_dir under the pattern: condition matches.
        let matching = ConfigIncludeContext::new(Some(work_git.clone()), None);
        let config =
            load_config_with_includes(&main, &matching).expect("test operation should succeed");
        assert_eq!(config.get("user", None, "name"), Some("work"));

        // git_dir elsewhere: condition does not match, nothing is spliced.
        let other = ConfigIncludeContext::new(Some(dir.join("other/.git")), None);
        let config =
            load_config_with_includes(&main, &other).expect("test operation should succeed");
        assert_eq!(config.get("user", None, "name"), None);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn config_include_if_gitdir_case_insensitive() {
        let dir = unique_include_dir("inc-gitdir-i");
        let main = dir.join("config");
        fs::write(
            &main,
            "[includeIf \"gitdir/i:/SOME/Path/**\"]\n\tpath = ci.cfg\n",
        )
        .expect("test operation should succeed");
        fs::write(dir.join("ci.cfg"), "[user]\n\tname = ci\n")
            .expect("test operation should succeed");

        let ctx = ConfigIncludeContext::new(Some(PathBuf::from("/some/path/repo/.git")), None);
        let config = load_config_with_includes(&main, &ctx).expect("test operation should succeed");
        assert_eq!(config.get("user", None, "name"), Some("ci"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn config_include_if_onbranch_match() {
        let dir = unique_include_dir("inc-onbranch");
        let main = dir.join("config");
        fs::write(
            &main,
            "[includeIf \"onbranch:feature/*\"]\n\tpath = feat.cfg\n",
        )
        .expect("test operation should succeed");
        fs::write(dir.join("feat.cfg"), "[user]\n\tname = feature\n")
            .expect("test operation should succeed");

        // Matching branch.
        let on = ConfigIncludeContext::new(None, Some("feature/login".into()));
        let config = load_config_with_includes(&main, &on).expect("test operation should succeed");
        assert_eq!(config.get("user", None, "name"), Some("feature"));

        // Non-matching branch (slash boundary: `*` does not cross `/`).
        let off = ConfigIncludeContext::new(None, Some("main".into()));
        let config = load_config_with_includes(&main, &off).expect("test operation should succeed");
        assert_eq!(config.get("user", None, "name"), None);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn config_include_recursion_depth_limit() {
        let dir = unique_include_dir("inc-depth");
        // Build a chain longer than the depth limit; each file includes the next.
        let total = CONFIG_MAX_INCLUDE_DEPTH + 3;
        for i in 0..total {
            let path = dir.join(format!("c{i}.cfg"));
            let next = dir.join(format!("c{}.cfg", i + 1));
            fs::write(
                &path,
                format!(
                    "[s{i}]\n\tk = v{i}\n[include]\n\tpath = {}\n",
                    next.display()
                ),
            )
            .expect("test operation should succeed");
        }
        let entry = dir.join("c0.cfg");
        let ctx = ConfigIncludeContext::default();
        let err = load_config_with_includes(&entry, &ctx).expect_err("test operation should fail");
        assert!(matches!(err, GitError::InvalidFormat(_)), "got {err:?}");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn config_resolve_includes_on_parsed_value() {
        let dir = unique_include_dir("inc-parsed");
        let extra = dir.join("extra.cfg");
        fs::write(&extra, "[user]\n\temail = parsed@x.y\n").expect("test operation should succeed");
        let parsed =
            GitConfig::parse(format!("[include]\n\tpath = {}\n", extra.display()).as_bytes())
                .expect("test operation should succeed");
        // The parser leaves the include unresolved.
        assert_eq!(parsed.get("user", None, "email"), None);
        // Resolving against the base dir splices it in.
        let resolved = parsed
            .resolve_includes(&dir, &ConfigIncludeContext::default())
            .expect("test operation should succeed");
        assert_eq!(resolved.get("user", None, "email"), Some("parsed@x.y"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn config_nested_include_resolves_within_depth() {
        let dir = unique_include_dir("inc-nested");
        let main = dir.join("config");
        let mid = dir.join("mid.cfg");
        let leaf = dir.join("leaf.cfg");
        fs::write(&main, format!("[include]\n\tpath = {}\n", mid.display()))
            .expect("test operation should succeed");
        fs::write(&mid, format!("[include]\n\tpath = {}\n", leaf.display()))
            .expect("test operation should succeed");
        fs::write(&leaf, "[deep]\n\tvalue = ok\n").expect("test operation should succeed");

        let ctx = ConfigIncludeContext::default();
        let config = load_config_with_includes(&main, &ctx).expect("test operation should succeed");
        assert_eq!(config.get("deep", None, "value"), Some("ok"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn config_include_if_hasconfig_remote_url_match_and_non_match() {
        let dir = unique_include_dir("inc-hasconfig");
        let include_this = dir.join("include-this");
        let dont_include = dir.join("dont-include-that");
        fs::write(
            &include_this,
            "[user]\n\tthis = this-is-included\n",
        )
        .expect("test operation should succeed");
        fs::write(
            &dont_include,
            "[user]\n\tthat = that-is-not-included\n",
        )
        .expect("test operation should succeed");
        let main = dir.join("config");
        fs::write(
            &main,
            format!(
                "[includeIf \"hasconfig:remote.*.url:foourl\"]\n\tpath = {}\n\
                 [includeIf \"hasconfig:remote.*.url:barurl\"]\n\tpath = {}\n\
                 [remote \"foo\"]\n\turl = foourl\n",
                include_this.display(),
                dont_include.display()
            ),
        )
        .expect("test operation should succeed");

        let ctx = ConfigIncludeContext::default();
        let config =
            load_config_with_includes(&main, &ctx).expect("test operation should succeed");
        assert_eq!(config.get("user", None, "this"), Some("this-is-included"));
        assert_eq!(config.get("user", None, "that"), None);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn config_include_if_hasconfig_respects_order_within_file() {
        let dir = unique_include_dir("inc-hasconfig-order");
        let include_file = dir.join("include-two-three");
        fs::write(
            &include_file,
            "[user]\n\ttwo = included-config\n\tthree = included-config\n",
        )
        .expect("test operation should succeed");
        let main = dir.join("config");
        fs::write(
            &main,
            format!(
                "[remote \"foo\"]\n\turl = foourl\n\
                 [user]\n\tone = main-config\n\ttwo = main-config\n\
                 [includeIf \"hasconfig:remote.*.url:foourl\"]\n\tpath = {}\n\
                 [user]\n\tthree = main-config\n",
                include_file.display()
            ),
        )
        .expect("test operation should succeed");

        let ctx = ConfigIncludeContext::default();
        let config =
            load_config_with_includes(&main, &ctx).expect("test operation should succeed");
        assert_eq!(config.get("user", None, "one"), Some("main-config"));
        assert_eq!(config.get("user", None, "two"), Some("included-config"));
        assert_eq!(config.get("user", None, "three"), Some("main-config"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn config_include_if_hasconfig_remote_url_globs() {
        let dir = unique_include_dir("inc-hasconfig-globs");
        let write_user = |name: &str, key: &str| {
            fs::write(dir.join(name), format!("[user]\n\t{key} = yes\n"))
                .expect("test operation should succeed");
        };
        write_user("double-star-start", "dss");
        write_user("double-star-end", "dse");
        write_user("double-star-middle", "dsm");
        write_user("single-star-middle", "ssm");
        write_user("no", "no");

        let main = dir.join("config");
        fs::write(
            &main,
            format!(
                "[remote \"foo\"]\n\turl = https://foo/bar/baz\n\
                 [includeIf \"hasconfig:remote.*.url:**/baz\"]\n\tpath = {}\n\
                 [includeIf \"hasconfig:remote.*.url:**/nomatch\"]\n\tpath = {}\n\
                 [includeIf \"hasconfig:remote.*.url:https:/**\"]\n\tpath = {}\n\
                 [includeIf \"hasconfig:remote.*.url:nomatch:/**\"]\n\tpath = {}\n\
                 [includeIf \"hasconfig:remote.*.url:https:/**/baz\"]\n\tpath = {}\n\
                 [includeIf \"hasconfig:remote.*.url:https:/**/nomatch\"]\n\tpath = {}\n\
                 [includeIf \"hasconfig:remote.*.url:https://*/bar/baz\"]\n\tpath = {}\n\
                 [includeIf \"hasconfig:remote.*.url:https://*/baz\"]\n\tpath = {}\n",
                dir.join("double-star-start").display(),
                dir.join("no").display(),
                dir.join("double-star-end").display(),
                dir.join("no").display(),
                dir.join("double-star-middle").display(),
                dir.join("no").display(),
                dir.join("single-star-middle").display(),
                dir.join("no").display(),
            ),
        )
        .expect("test operation should succeed");

        let ctx = ConfigIncludeContext::default();
        let config =
            load_config_with_includes(&main, &ctx).expect("test operation should succeed");
        assert_eq!(config.get("user", None, "dss"), Some("yes"));
        assert_eq!(config.get("user", None, "dse"), Some("yes"));
        assert_eq!(config.get("user", None, "dsm"), Some("yes"));
        assert_eq!(config.get("user", None, "ssm"), Some("yes"));
        assert_eq!(config.get("user", None, "no"), None);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn config_include_if_hasconfig_forbids_remote_url_in_included_file() {
        let dir = unique_include_dir("inc-hasconfig-forbid");
        let include_file = dir.join("include-with-url");
        fs::write(&include_file, "[remote \"bar\"]\n\turl = barurl\n")
            .expect("test operation should succeed");
        let main = dir.join("config");
        fs::write(
            &main,
            format!(
                "[remote \"foo\"]\n\turl = foourl\n\
                 [includeIf \"hasconfig:remote.*.url:foourl\"]\n\tpath = {}\n",
                include_file.display()
            ),
        )
        .expect("test operation should succeed");

        let ctx = ConfigIncludeContext::default();
        let err = load_config_with_includes(&main, &ctx).expect_err("test operation should fail");
        assert!(
            matches!(err, GitError::InvalidFormat(ref message) if message.contains(
                "remote URLs cannot be configured in file directly or indirectly included by includeIf.hasconfig:remote.*.url"
            )),
            "got {err:?}"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn config_include_if_unknown_hasconfig_does_not_match() {
        let dir = unique_include_dir("inc-hasconfig-unknown");
        let include_file = dir.join("extra.cfg");
        fs::write(&include_file, "[user]\n\tname = included\n")
            .expect("test operation should succeed");
        let main = dir.join("config");
        fs::write(
            &main,
            format!(
                "[includeIf \"hasconfig:core.repositoryformatversion:0\"]\n\tpath = {}\n",
                include_file.display()
            ),
        )
        .expect("test operation should succeed");

        let ctx = ConfigIncludeContext::default();
        let config =
            load_config_with_includes(&main, &ctx).expect("test operation should succeed");
        assert_eq!(config.get("user", None, "name"), None);
        fs::remove_dir_all(&dir).ok();
    }

    // NOTE: `load_effective_config`'s file discovery reads process-global
    // environment variables (`HOME`, `GIT_CONFIG_*`). The workspace forbids
    // `unsafe_code`, so these tests cannot mutate the environment in-process
    // without racing the parallel test runner. End-to-end discovery (HOME-based
    // global fallback, `GIT_CONFIG_NOSYSTEM`, repo/`-c` overrides) is therefore
    // covered hermetically by the CLI's subprocess interop test
    // (`commit_identity_falls_back_to_global_gitconfig_like_upstream_git`),
    // which sets the environment only on the child process. Here we cover the
    // merge/precedence *semantics* that the loader relies on without touching
    // the environment.

    /// Concatenate config layers the way `load_effective_config` does (lowest
    /// precedence first) so the document below mirrors a system+global+repo
    /// merge without performing any environment-dependent file discovery.
    fn merge_layers(layers: &[&str]) -> GitConfig {
        let mut sections = Vec::new();
        for layer in layers {
            sections.extend(
                GitConfig::parse(layer.as_bytes())
                    .expect("test operation should succeed")
                    .sections,
            );
        }
        GitConfig {
            preamble: Vec::new(),
            suffix: Vec::new(),
            sections,
        }
    }

    #[test]
    fn effective_config_paths_are_ordered_system_global_repo() {
        // Independent of the environment, the repository config is always last
        // (highest precedence) and the system file, when present, is first.
        let repo = Path::new("/tmp/sley-effective-paths/repo");
        let paths = effective_config_paths(repo);
        assert_eq!(
            paths.last(),
            Some(&repo.join("config")),
            "repository config must be the highest-precedence (last) layer"
        );
        // The repository layer is always present; system/global depend on the
        // environment and machine, so we only assert relative ordering here.
        let repo_index = paths
            .iter()
            .position(|path| path == &repo.join("config"))
            .expect("repo config present");
        assert_eq!(repo_index, paths.len() - 1);
    }

    #[test]
    fn effective_merge_semantics_are_last_layer_wins() {
        // system -> global -> repo, mirroring the loader's concatenation order.
        let config = merge_layers(&[
            "[user]\n\tname = System\n[layer]\n\tsystem = yes\n",
            "[user]\n\tname = Global\n[layer]\n\tglobal = yes\n",
            "[user]\n\tname = Repo\n[layer]\n\trepo = yes\n",
        ]);
        // Repository (last) layer wins for a single-valued get.
        assert_eq!(config.get("user", None, "name"), Some("Repo"));
        // get_all preserves lowest-precedence-first ordering.
        assert_eq!(
            config.get_all("user", None, "name"),
            vec![Some("System"), Some("Global"), Some("Repo")]
        );
        // Each layer's distinct keys all survive the merge.
        assert_eq!(config.get("layer", None, "system"), Some("yes"));
        assert_eq!(config.get("layer", None, "global"), Some("yes"));
        assert_eq!(config.get("layer", None, "repo"), Some("yes"));
    }

    #[test]
    fn env_bool_matches_git_env_bool_semantics() {
        // parse_config_bool drives env_bool; verify the boolean keywords git
        // accepts for `GIT_CONFIG_NOSYSTEM` map as expected.
        assert_eq!(parse_config_bool("1"), Some(true));
        assert_eq!(parse_config_bool("true"), Some(true));
        assert_eq!(parse_config_bool("yes"), Some(true));
        assert_eq!(parse_config_bool("0"), Some(false));
        assert_eq!(parse_config_bool("false"), Some(false));
        // An empty value is false (git treats `key =` as false).
        assert_eq!(parse_config_bool(""), Some(false));
    }
}
