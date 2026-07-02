//! Shared pathspec primitive for sley.
//!
//! This crate owns the byte-faithful port of git's wildmatch engine
//! (`wildmatch.c::dowild`) and the single-item pathspec matcher
//! (`match_pathspec_item`), plus a [`Pathspec`] type that parses git's
//! pathspec *magic* prefixes (`:(exclude)`, `:(icase)`, `:(literal)`,
//! `:(glob)`, `:(top)`, `:(attr:...)`, and the shorthand `:!`/`:^`/`:/`).
//!
//! Four clusters consume this primitive: the rev-walk (`sley-rev`), diff,
//! the worktree walker (`sley-worktree`, which re-exports the engine for its
//! `ls-files` path), and the CLI. Keeping the wildmatch port and the magic
//! parser in one low-level crate (depending only on `sley-core`) means there
//! is exactly one implementation of git's matching semantics to keep in sync
//! with the 2.54 oracle.
//!
//! STAGE-A scope: parsing + per-path `matches`. The TREESAME / history
//! simplification that *consumes* a `Pathspec` to prune the rev-walk is
//! STAGE-B; this crate only provides the matching primitive that stage will
//! drive.

use sley_core::GitError;
use std::cell::Cell;
use std::fs;
use std::path::Path;

/// A parsed pathspec element: a single pattern plus its magic flags.
///
/// Mirrors git's `struct pathspec_item` for the subset sley needs today.
/// Construct with [`PathspecElement::parse`]; query with
/// [`PathspecElement::matches`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathspecElement {
    /// The match pattern with any magic prefix stripped (git's `item.match`).
    pattern: Vec<u8>,
    /// `:(exclude)` / `:!` / `:^` — this element subtracts from the set.
    exclude: bool,
    /// `:(icase)` — case-insensitive matching.
    icase: bool,
    /// `:(literal)` — wildcards are matched literally (no globbing).
    literal: bool,
    /// `:(glob)` — pathname-aware globbing (`**` required to cross `/`).
    glob: bool,
    /// `:(top)` / `:/` — match from the repository root (sley already matches
    /// repo-relative paths from the root, so this is parsed and surfaced but
    /// does not change single-path matching; it affects prefix handling that
    /// the consuming cluster applies).
    top: bool,
    /// `:(attr:...)` attribute requirements, stored verbatim. Attribute-based
    /// selection is not yet evaluated (STAGE-B+); the labels are retained so a
    /// pathspec carrying them round-trips and the consumer can reject/honor
    /// them explicitly rather than silently dropping them.
    attrs: Vec<Vec<u8>>,
    attr_requirements: Vec<PathspecAttrRequirement>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathspecAttrRequirement {
    Set(Vec<u8>),
    Unset(Vec<u8>),
    Unspecified(Vec<u8>),
    Value { name: Vec<u8>, value: Vec<u8> },
}

impl PathspecElement {
    /// Parse one pathspec argument, honoring git's magic prefixes.
    ///
    /// Recognizes both the long form `:(magic1,magic2,...)pattern` and the
    /// shorthand sigils `:!`/`:^` (exclude), `:/` (top). Defaults
    /// (`literal`/`glob`/`icase` from the global `--*-pathspecs` flags) are
    /// supplied via `defaults` and overridden per-element by any explicit
    /// magic. Unknown long-form magic words are an error, matching git.
    pub fn parse(arg: &[u8], defaults: PathspecMatchMagic) -> Result<Self, PathspecParseError> {
        let mut exclude = false;
        let mut icase = defaults.icase;
        let mut literal = defaults.literal;
        let mut glob = defaults.glob;
        let mut top = false;
        let mut attrs: Vec<Vec<u8>> = Vec::new();
        let mut attr_requirements: Vec<PathspecAttrRequirement> = Vec::new();
        let mut explicit_literal = false;
        let mut explicit_glob = false;

        let rest = if defaults.literal_pathspecs {
            arg
        } else if let Some(after) = arg.strip_prefix(b":(") {
            // Long form: :(magic[,magic...])pattern
            let close = after
                .iter()
                .position(|&c| c == b')')
                .ok_or(PathspecParseError::UnterminatedMagic)?;
            let magic = &after[..close];
            for word in split_magic(magic) {
                match word.as_slice() {
                    b"exclude" => exclude = true,
                    b"icase" => icase = true,
                    b"literal" => {
                        explicit_literal = true;
                        literal = true;
                        glob = false;
                    }
                    b"glob" => {
                        explicit_glob = true;
                        glob = true;
                        literal = false;
                    }
                    b"top" => top = true,
                    other => {
                        if let Some(attr) = other.strip_prefix(b"attr:") {
                            if !attrs.is_empty() {
                                return Err(PathspecParseError::MultipleAttrMagic);
                            }
                            attrs.push(attr.to_vec());
                            attr_requirements = parse_attr_requirements(attr)?;
                        } else if other.is_empty() {
                            // Empty magic word (e.g. trailing comma) — ignore,
                            // matching git's lenient split.
                        } else {
                            return Err(PathspecParseError::UnknownMagic(other.to_vec()));
                        }
                    }
                }
            }
            &after[close + 1..]
        } else if let Some(after) = arg.strip_prefix(b":") {
            // Shorthand sigils. git consumes a run of leading sigils.
            let mut idx = 0;
            while idx < after.len() {
                match after[idx] {
                    b'!' | b'^' => exclude = true,
                    b'/' => top = true,
                    _ => break,
                }
                idx += 1;
            }
            &after[idx..]
        } else {
            arg
        };

        // `:(glob)` and `:(literal)` are mutually exclusive in git.
        if (glob && literal) || (explicit_glob && explicit_literal) {
            return Err(PathspecParseError::GlobLiteralConflict);
        }

        Ok(PathspecElement {
            pattern: rest.to_vec(),
            exclude,
            icase,
            literal,
            glob,
            top,
            attrs,
            attr_requirements,
        })
    }

    /// Whether this element is an `:(exclude)` element.
    pub fn is_exclude(&self) -> bool {
        self.exclude
    }

    /// Whether this element carries `:(top)` / `:/` magic.
    pub fn is_top(&self) -> bool {
        self.top
    }

    /// The attribute requirements carried by `:(attr:...)`, if any.
    pub fn attrs(&self) -> &[Vec<u8>] {
        &self.attrs
    }

    pub fn attr_requirements(&self) -> &[PathspecAttrRequirement] {
        &self.attr_requirements
    }

    /// Whether this element carries case-insensitive matching.
    pub fn is_icase(&self) -> bool {
        self.icase
    }

    /// Whether this element carries glob magic.
    pub fn is_glob(&self) -> bool {
        self.glob
    }

    /// The bare match pattern (magic prefix stripped).
    pub fn pattern(&self) -> &[u8] {
        &self.pattern
    }

    /// The [`PathspecMatchMagic`] this element matches under.
    pub fn magic(&self) -> PathspecMatchMagic {
        PathspecMatchMagic {
            literal: self.literal,
            glob: self.glob,
            icase: self.icase,
            literal_pathspecs: false,
        }
    }

    /// Whether `name` (a repo-relative path, no leading slash) is selected by
    /// this single element, ignoring its exclude polarity. Use
    /// [`Pathspec::matches`] for the combined include/exclude semantics.
    pub fn matches_path(&self, name: &[u8]) -> bool {
        pathspec_item_matches(&self.pattern, name, self.magic())
    }

    pub fn with_pattern(mut self, pattern: Vec<u8>) -> Self {
        self.pattern = pattern;
        self
    }
}

/// A full pathspec: an ordered list of [`PathspecElement`]s combining positive
/// (include) and `:(exclude)` patterns.
///
/// Semantics (git `match_pathspec`): a path matches when at least one
/// non-exclude element selects it AND no exclude element selects it. An
/// all-exclude (or empty) pathspec matches everything not excluded — matching
/// git, where `git log -- ':(exclude)foo'` keeps every path but `foo`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pathspec {
    elements: Vec<PathspecElement>,
}

impl Pathspec {
    /// Parse a list of raw pathspec arguments under the given global magic
    /// defaults (from `--{glob,noglob,literal,icase}-pathspecs`).
    pub fn parse<I, S>(args: I, defaults: PathspecMatchMagic) -> Result<Self, PathspecParseError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<[u8]>,
    {
        let mut elements = Vec::new();
        for arg in args {
            elements.push(PathspecElement::parse(arg.as_ref(), defaults)?);
        }
        Ok(Pathspec { elements })
    }

    pub fn from_elements(elements: Vec<PathspecElement>) -> Self {
        Self { elements }
    }

    /// An empty pathspec matches every path.
    pub fn is_empty(&self) -> bool {
        self.elements.is_empty()
    }

    /// The parsed elements, in order.
    pub fn elements(&self) -> &[PathspecElement] {
        &self.elements
    }

    /// Whether `path` (repo-relative, no leading slash) is selected.
    ///
    /// An empty pathspec, or one with only excludes, matches any path the
    /// excludes don't subtract — exactly git's `match_pathspec` behavior.
    pub fn matches(&self, path: &[u8]) -> bool {
        if self.elements.is_empty() {
            return true;
        }
        let mut have_include = false;
        let mut included = false;
        for element in &self.elements {
            if element.exclude {
                if element.matches_path(path) {
                    return false;
                }
            } else {
                have_include = true;
                if element.matches_path(path) {
                    included = true;
                }
            }
        }
        // With at least one include, the path must hit one of them. With only
        // excludes, anything not excluded is kept.
        if have_include { included } else { true }
    }
}

pub struct LsFilesPathFilter {
    pub original: String,
    pub recursive: bool,
    pub is_glob: bool,
    pub element: PathspecElement,
    pub matched: Cell<bool>,
}

impl LsFilesPathFilter {
    pub fn is_exclude(&self) -> bool {
        self.element.is_exclude()
    }

    pub fn matches(&self, path: &[u8]) -> bool {
        // Byte-exact git `match_pathspec_item` for the tracked-index path. Handles
        // exact / directory-prefix / wildcard matching under the active magic.
        let path_no_slash = path.strip_suffix(b"/").unwrap_or(path);
        self.element.matches_path(path)
            || (path_no_slash.len() != path.len() && self.element.matches_path(path_no_slash))
    }
}

pub fn pathspec_filters_match(filters: &[LsFilesPathFilter], path: &[u8]) -> bool {
    pathspec_filters_match_with(filters, path, |filter, path| filter.matches(path))
}

pub fn pathspec_filters_have_include(filters: &[LsFilesPathFilter]) -> bool {
    filters.iter().any(|filter| !filter.is_exclude())
}

pub fn pathspec_filters_match_with(
    filters: &[LsFilesPathFilter],
    path: &[u8],
    mut matches: impl FnMut(&LsFilesPathFilter, &[u8]) -> bool,
) -> bool {
    let mut have_include = false;
    let mut included = false;
    for filter in filters {
        if filter.is_exclude() {
            if matches(filter, path) {
                filter.matched.set(true);
                return false;
            }
        } else {
            have_include = true;
            if matches(filter, path) {
                filter.matched.set(true);
                included = true;
            }
        }
    }
    !have_include || included
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathspecAttributeState {
    Set,
    Unset,
    Value(Vec<u8>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathspecAttributeCheck {
    pub attribute: Vec<u8>,
    pub state: Option<PathspecAttributeState>,
}

pub fn pathspec_attrs_match_with(
    element: &PathspecElement,
    checks: impl FnOnce(&[Vec<u8>]) -> Vec<PathspecAttributeCheck>,
) -> bool {
    let requirements = element.attr_requirements();
    if requirements.is_empty() {
        return true;
    }
    let requested = requirements
        .iter()
        .map(|requirement| match requirement {
            PathspecAttrRequirement::Set(name)
            | PathspecAttrRequirement::Unset(name)
            | PathspecAttrRequirement::Unspecified(name) => name.clone(),
            PathspecAttrRequirement::Value { name, .. } => name.clone(),
        })
        .collect::<Vec<_>>();
    let checks = checks(&requested);
    requirements.iter().all(|requirement| {
        let (name, expected) = match requirement {
            PathspecAttrRequirement::Set(name) => (name, AttrRequirementKind::Set),
            PathspecAttrRequirement::Unset(name) => (name, AttrRequirementKind::Unset),
            PathspecAttrRequirement::Unspecified(name) => (name, AttrRequirementKind::Unspecified),
            PathspecAttrRequirement::Value { name, value } => {
                (name, AttrRequirementKind::Value(value))
            }
        };
        let state = checks
            .iter()
            .find(|check| &check.attribute == name)
            .and_then(|check| check.state.as_ref());
        match expected {
            AttrRequirementKind::Set => matches!(state, Some(PathspecAttributeState::Set)),
            AttrRequirementKind::Unset => matches!(state, Some(PathspecAttributeState::Unset)),
            AttrRequirementKind::Unspecified => state.is_none(),
            AttrRequirementKind::Value(value) => {
                matches!(state, Some(PathspecAttributeState::Value(actual)) if actual == value)
            }
        }
    })
}

enum AttrRequirementKind<'a> {
    Set,
    Unset,
    Unspecified,
    Value(&'a [u8]),
}

pub fn parse_normalized_pathspec_element(
    prefix: &[u8],
    arg: &str,
    magic: PathspecMatchMagic,
) -> sley_core::Result<PathspecElement> {
    let element = PathspecElement::parse(arg.as_bytes(), magic)
        .map_err(|err| GitError::Command(format!("bad pathspec: {err}")))?;
    let base = if element.is_top() {
        b"".as_slice()
    } else {
        prefix
    };
    let pattern = normalize_ls_files_pathspec(base, &String::from_utf8_lossy(element.pattern()))?;
    Ok(element.with_pattern(pattern))
}

pub fn normalized_revwalk_pathspec(
    cwd: &Path,
    worktree_root: Option<&Path>,
    pathspecs: &[String],
    magic: PathspecMatchMagic,
) -> sley_core::Result<Pathspec> {
    let (prefix, root_and_cwd) = if let Some(root) = worktree_root {
        let root = fs::canonicalize(root)?;
        let cwd = fs::canonicalize(cwd)?;
        let prefix = cwd
            .strip_prefix(&root)
            .map(|relative| relative.to_string_lossy().replace('\\', "/").into_bytes())
            .unwrap_or_default();
        (prefix, Some((root, cwd)))
    } else {
        (Vec::new(), None)
    };
    let elements = pathspecs
        .iter()
        .map(|spec| {
            let parse_spec = match root_and_cwd.as_ref() {
                Some((root, cwd)) => normalize_absolute_pathspec_arg(root, cwd, spec)?,
                None => spec.to_string(),
            };
            parse_normalized_pathspec_element(&prefix, &parse_spec, magic)
        })
        .collect::<sley_core::Result<Vec<_>>>()?;
    Ok(Pathspec::from_elements(elements))
}

fn normalize_absolute_pathspec_arg(
    root: &Path,
    cwd: &Path,
    arg: &str,
) -> sley_core::Result<String> {
    let path = Path::new(arg);
    if !path.is_absolute() {
        return Ok(arg.to_string());
    }
    let absolute = fs::canonicalize(path)?;
    let relative = absolute
        .strip_prefix(root)
        .map_err(|_| GitError::InvalidPath(format!("pathspec {arg} is outside worktree")))?;
    let repo_path = relative.to_string_lossy().replace('\\', "/");
    if repo_path.is_empty() {
        return Ok(":/".to_string());
    }
    if cwd == root {
        return Ok(repo_path);
    }
    Ok(format!(":(top){repo_path}"))
}

pub fn normalize_ls_files_pathspec(prefix: &[u8], arg: &str) -> sley_core::Result<Vec<u8>> {
    let mut components = prefix
        .split(|byte| *byte == b'/')
        .filter(|component| !component.is_empty())
        .map(Vec::from)
        .collect::<Vec<_>>();
    for component in Path::new(arg).components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                components.pop().ok_or_else(|| {
                    GitError::InvalidPath(format!("pathspec {arg} is outside worktree"))
                })?;
            }
            std::path::Component::Normal(name) => {
                components.push(name.to_string_lossy().as_bytes().to_vec());
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                return Err(GitError::Unsupported(
                    "ls-files pathspecs currently support relative paths".into(),
                ));
            }
        }
    }
    Ok(components.join(&b'/'))
}

/// Split a `:(...)` magic body on commas (git's `parse_long_magic` separator).
fn split_magic(body: &[u8]) -> Vec<Vec<u8>> {
    let mut words = Vec::new();
    let mut word = Vec::new();
    let mut escaped = false;
    for &byte in body {
        if escaped {
            word.push(byte);
            escaped = false;
        } else if byte == b'\\' {
            word.push(byte);
            escaped = true;
        } else if byte == b',' {
            words.push(std::mem::take(&mut word));
        } else {
            word.push(byte);
        }
    }
    words.push(word);
    words
}

fn parse_attr_requirements(
    body: &[u8],
) -> Result<Vec<PathspecAttrRequirement>, PathspecParseError> {
    if body.is_empty() {
        return Err(PathspecParseError::EmptyAttrMagic);
    }
    let mut requirements = Vec::new();
    for raw in body.split(|byte| byte.is_ascii_whitespace()) {
        if raw.is_empty() {
            continue;
        }
        requirements.push(parse_attr_requirement(raw)?);
    }
    if requirements.is_empty() {
        return Err(PathspecParseError::EmptyAttrMagic);
    }
    Ok(requirements)
}

fn parse_attr_requirement(raw: &[u8]) -> Result<PathspecAttrRequirement, PathspecParseError> {
    if let Some(rest) = raw.strip_prefix(b"-") {
        if rest.contains(&b'=') {
            return Err(PathspecParseError::InvalidAttrSpec(raw.to_vec()));
        }
        validate_attr_name(rest)?;
        return Ok(PathspecAttrRequirement::Unset(rest.to_vec()));
    }
    if let Some(rest) = raw.strip_prefix(b"!") {
        if rest.contains(&b'=') {
            return Err(PathspecParseError::InvalidAttrSpec(raw.to_vec()));
        }
        validate_attr_name(rest)?;
        return Ok(PathspecAttrRequirement::Unspecified(rest.to_vec()));
    }
    if let Some(equal) = raw.iter().position(|byte| *byte == b'=') {
        let name = &raw[..equal];
        let value = unescape_attr_value(&raw[equal + 1..])?;
        validate_attr_name(name)?;
        return Ok(PathspecAttrRequirement::Value {
            name: name.to_vec(),
            value,
        });
    }
    validate_attr_name(raw)?;
    Ok(PathspecAttrRequirement::Set(raw.to_vec()))
}

fn validate_attr_name(name: &[u8]) -> Result<(), PathspecParseError> {
    if name.is_empty()
        || !name
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(*byte, b'-' | b'_' | b'.'))
    {
        return Err(PathspecParseError::InvalidAttrSpec(name.to_vec()));
    }
    Ok(())
}

fn unescape_attr_value(value: &[u8]) -> Result<Vec<u8>, PathspecParseError> {
    let mut out = Vec::with_capacity(value.len());
    let mut idx = 0usize;
    while idx < value.len() {
        if value[idx] != b'\\' {
            out.push(value[idx]);
            idx += 1;
            continue;
        }
        let Some(&next) = value.get(idx + 1) else {
            return Err(PathspecParseError::AttrValueTrailingBackslash);
        };
        if next != b',' {
            return Err(PathspecParseError::AttrValueUnsupportedBackslash);
        }
        out.push(next);
        idx += 2;
    }
    Ok(out)
}

/// Error parsing a pathspec magic prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathspecParseError {
    /// A `:(` was not closed by a `)`.
    UnterminatedMagic,
    /// A long-form magic word git does not recognize.
    UnknownMagic(Vec<u8>),
    /// `:(glob)` and `:(literal)` were both requested.
    GlobLiteralConflict,
    EmptyAttrMagic,
    MultipleAttrMagic,
    InvalidAttrSpec(Vec<u8>),
    AttrValueTrailingBackslash,
    AttrValueUnsupportedBackslash,
}

impl core::fmt::Display for PathspecParseError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            PathspecParseError::UnterminatedMagic => {
                write!(f, "Missing ')' at end of pathspec magic")
            }
            PathspecParseError::UnknownMagic(word) => {
                write!(
                    f,
                    "Invalid pathspec magic '{}'",
                    String::from_utf8_lossy(word)
                )
            }
            PathspecParseError::GlobLiteralConflict => {
                write!(f, "'literal' and 'glob' are incompatible")
            }
            PathspecParseError::EmptyAttrMagic => write!(f, "empty attr magic is not allowed"),
            PathspecParseError::MultipleAttrMagic => {
                write!(f, "Only one 'attr:' specification is allowed")
            }
            PathspecParseError::InvalidAttrSpec(spec) => write!(
                f,
                "invalid attribute specification '{}'",
                String::from_utf8_lossy(spec)
            ),
            PathspecParseError::AttrValueTrailingBackslash => {
                write!(
                    f,
                    "Escape character '\\' not allowed as last character in attr value"
                )
            }
            PathspecParseError::AttrValueUnsupportedBackslash => {
                write!(f, "Only '\\,' is supported for value matching")
            }
        }
    }
}

impl std::error::Error for PathspecParseError {}

/// Pathspec match magic, mirroring git's `PATHSPEC_LITERAL`/`PATHSPEC_GLOB`/
/// `PATHSPEC_ICASE`. Constructed from the global `--{glob,noglob,icase,literal}-pathspecs`
/// options. Drives [`pathspec_item_matches`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PathspecMatchMagic {
    pub literal: bool,
    pub glob: bool,
    pub icase: bool,
    /// `--literal-pathspecs` / `GIT_LITERAL_PATHSPECS`: the entire pathspec is
    /// literal, including leading `:(...)` magic syntax.
    pub literal_pathspecs: bool,
}

/// git `is_glob_special`: characters that make a pathspec a wildcard.
fn is_glob_special(c: u8) -> bool {
    matches!(c, b'*' | b'?' | b'[' | b'\\')
}

/// git `simple_length`: length of the literal prefix before the first glob-special
/// character (or end of string).
fn simple_length(s: &[u8]) -> usize {
    for (i, &c) in s.iter().enumerate() {
        if is_glob_special(c) {
            return i;
        }
    }
    s.len()
}

/// Case-aware byte comparison up to `n` bytes, honoring `icase` (git `ps_strncmp`).
fn ps_strncmp(icase: bool, a: &[u8], b: &[u8], n: usize) -> bool {
    // Returns true when the first `n` bytes are EQUAL (mirrors `!strncmp`).
    let a = &a[..a.len().min(n)];
    let b = &b[..b.len().min(n)];
    if a.len() < n && b.len() < n && a.len() != b.len() {
        return false;
    }
    let len = n.min(a.len()).min(b.len());
    for i in 0..len {
        let (mut ca, mut cb) = (a[i], b[i]);
        if icase {
            ca = ca.to_ascii_lowercase();
            cb = cb.to_ascii_lowercase();
        }
        if ca != cb {
            return false;
        }
    }
    true
}

/// True if `path` contains a glob-special character.
pub fn pathspec_is_glob(path: &[u8]) -> bool {
    path.iter().any(|byte| is_glob_special(*byte))
}

/// Port of git's `match_pathspec_item` for the single-pathspec / single-name case
/// (no prefix, no attr magic). `match_` is the pathspec, `name` is the candidate
/// path. Returns whether the pathspec selects `name` (exactly, as a directory
/// prefix, or via wildmatch). Byte-for-byte faithful to git 2.54 for the
/// `ls-files -- <pathspec>` path that t3070 exercises.
pub fn pathspec_item_matches(match_: &[u8], name: &[u8], magic: PathspecMatchMagic) -> bool {
    let icase = magic.icase;
    let matchlen = match_.len();
    let namelen = name.len();

    // nowildcard_len: with LITERAL magic the whole pattern is literal.
    let nowildcard_len = if magic.literal {
        matchlen
    } else {
        simple_length(match_)
    };

    // Empty pathspec matches everything (git: `if (!*match) return MATCHED_RECURSIVELY`).
    if matchlen == 0 {
        return true;
    }

    // Literal-prefix comparison.
    if matchlen <= namelen && ps_strncmp(icase, match_, name, matchlen) {
        if matchlen == namelen {
            return true; // MATCHED_EXACTLY
        }
        if match_[matchlen - 1] == b'/' || name[matchlen] == b'/' {
            return true; // MATCHED_RECURSIVELY
        }
    } else if match_[matchlen - 1] == b'/'
        && namelen == matchlen - 1
        && ps_strncmp(icase, match_, name, namelen)
    {
        // DO_MATCH_DIRECTORY case: pathspec `foo/` vs name `foo`.
        return true;
    }

    // Wildcard match — git `git_fnmatch(item, match, name, nowildcard_len)`.
    if nowildcard_len < matchlen {
        // git strips the literal prefix off BOTH pattern and name before running
        // wildmatch (so `foo**` vs `foo/bba/arr` becomes `**` vs `/bba/arr`).
        if nowildcard_len > 0 && !ps_strncmp(icase, match_, name, nowildcard_len) {
            return false;
        }
        let pat = &match_[nowildcard_len..];
        if name.len() < nowildcard_len {
            return false;
        }
        let str_ = &name[nowildcard_len..];

        let flags = if magic.glob && !magic.literal {
            WM_PATHNAME | if icase { WM_CASEFOLD } else { 0 }
        } else {
            // Default pathspec (no glob magic): pathmatch semantics.
            if icase { WM_CASEFOLD } else { 0 }
        };
        if wildmatch(pat, str_, flags) {
            return true;
        }
    }

    false
}

/// Case-insensitive match flag (git `WM_CASEFOLD`).
pub const WM_CASEFOLD: u32 = 1;
/// Pathname-aware match flag (git `WM_PATHNAME`): `*`/`?` do not cross `/`,
/// `**` is required to span directory separators.
pub const WM_PATHNAME: u32 = 2;

const WM_MATCH: i32 = 0;
const WM_NOMATCH: i32 = 1;
const WM_ABORT_ALL: i32 = -1;
const WM_ABORT_TO_STARSTAR: i32 = -2;

#[inline]
fn wm_isascii(c: u8) -> bool {
    c < 0x80
}
#[inline]
fn wm_isupper(c: u8) -> bool {
    wm_isascii(c) && c.is_ascii_uppercase()
}
#[inline]
fn wm_islower(c: u8) -> bool {
    wm_isascii(c) && c.is_ascii_lowercase()
}
#[inline]
fn wm_tolower(c: u8) -> u8 {
    c.to_ascii_lowercase()
}
#[inline]
fn wm_toupper(c: u8) -> u8 {
    c.to_ascii_uppercase()
}
#[inline]
fn wm_is_glob_special(c: u8) -> bool {
    matches!(c, b'*' | b'?' | b'[' | b'\\')
}

fn wm_cc_eq(class: &[u8], lit: &[u8]) -> bool {
    class == lit
}

fn wm_class_matches(class: &[u8], t_ch: u8, flags: u32) -> Option<bool> {
    // Returns Some(matched) for a recognized class, or None for a malformed
    // class name (caller maps to WM_ABORT_ALL).
    let m = if wm_cc_eq(class, b"alnum") {
        wm_isascii(t_ch) && t_ch.is_ascii_alphanumeric()
    } else if wm_cc_eq(class, b"alpha") {
        wm_isascii(t_ch) && t_ch.is_ascii_alphabetic()
    } else if wm_cc_eq(class, b"blank") {
        wm_isascii(t_ch) && (t_ch == b' ' || t_ch == b'\t')
    } else if wm_cc_eq(class, b"cntrl") {
        wm_isascii(t_ch) && t_ch.is_ascii_control()
    } else if wm_cc_eq(class, b"digit") {
        wm_isascii(t_ch) && t_ch.is_ascii_digit()
    } else if wm_cc_eq(class, b"graph") {
        wm_isascii(t_ch) && t_ch.is_ascii_graphic()
    } else if wm_cc_eq(class, b"lower") {
        wm_islower(t_ch)
    } else if wm_cc_eq(class, b"print") {
        // ISPRINT: printable including space (0x20..=0x7e).
        wm_isascii(t_ch) && (0x20..=0x7e).contains(&t_ch)
    } else if wm_cc_eq(class, b"punct") {
        wm_isascii(t_ch) && t_ch.is_ascii_punctuation()
    } else if wm_cc_eq(class, b"space") {
        wm_isascii(t_ch) && t_ch.is_ascii_whitespace()
    } else if wm_cc_eq(class, b"upper") {
        wm_isupper(t_ch) || ((flags & WM_CASEFOLD) != 0 && wm_islower(t_ch))
    } else if wm_cc_eq(class, b"xdigit") {
        wm_isascii(t_ch) && t_ch.is_ascii_hexdigit()
    } else {
        return None;
    };
    Some(m)
}

/// Faithful port of git's `wildmatch.c::dowild`. Returns one of the internal
/// `WM_*` codes (`WM_MATCH`, `WM_NOMATCH`, `WM_ABORT_ALL`, `WM_ABORT_TO_STARSTAR`).
fn dowild(pattern: &[u8], text: &[u8], flags: u32) -> i32 {
    let p = pattern;
    let mut pi = 0usize;
    let mut ti = 0usize;

    while pi < p.len() {
        let mut p_ch = p[pi];
        let t_ch_raw = if ti < text.len() { text[ti] } else { 0 };
        let mut t_ch = t_ch_raw;

        if t_ch == 0 && p_ch != b'*' {
            return WM_ABORT_ALL;
        }
        if (flags & WM_CASEFOLD) != 0 && wm_isupper(t_ch) {
            t_ch = wm_tolower(t_ch);
        }
        if (flags & WM_CASEFOLD) != 0 && wm_isupper(p_ch) {
            p_ch = wm_tolower(p_ch);
        }

        match p_ch {
            b'?' => {
                if (flags & WM_PATHNAME) != 0 && t_ch == b'/' {
                    return WM_NOMATCH;
                }
                // fallthrough: advance both
                pi += 1;
                ti += 1;
                continue;
            }
            b'*' => {
                pi += 1;
                let match_slash: bool;
                if pi < p.len() && p[pi] == b'*' {
                    let prev_p = pi; // index of the second '*'
                    while pi < p.len() && p[pi] == b'*' {
                        pi += 1;
                    }
                    if (flags & WM_PATHNAME) == 0 {
                        match_slash = true;
                    } else if (prev_p < 2 || p[prev_p - 2] == b'/')
                        && (pi == p.len()
                            || p[pi] == b'/'
                            || (p[pi] == b'\\' && pi + 1 < p.len() && p[pi + 1] == b'/'))
                    {
                        if pi < p.len()
                            && p[pi] == b'/'
                            && dowild(&p[pi + 1..], &text[ti..], flags) == WM_MATCH
                        {
                            return WM_MATCH;
                        }
                        match_slash = true;
                    } else {
                        match_slash = false;
                    }
                } else {
                    match_slash = (flags & WM_PATHNAME) == 0;
                }

                if pi == p.len() {
                    // Trailing "**" matches everything; trailing "*" matches only
                    // if there are no more slashes.
                    if !match_slash && text[ti..].contains(&b'/') {
                        return WM_ABORT_TO_STARSTAR;
                    }
                    return WM_MATCH;
                } else if !match_slash && p[pi] == b'/' {
                    // _one_ asterisk followed by a slash with WM_PATHNAME matches
                    // the next directory.
                    match text[ti..].iter().position(|&c| c == b'/') {
                        None => return WM_ABORT_ALL,
                        Some(off) => {
                            ti += off; // point at the slash; consumed by loop end
                        }
                    }
                    // emulate `break` then the for-loop's `text++; p++` increment:
                    pi += 1;
                    ti += 1;
                    continue;
                }

                // The matching loop.
                let mut cur_t = ti;
                loop {
                    let mut tc = if cur_t < text.len() { text[cur_t] } else { 0 };
                    if tc == 0 {
                        break;
                    }
                    if !wm_is_glob_special(p[pi]) {
                        let mut pc = p[pi];
                        if (flags & WM_CASEFOLD) != 0 && wm_isupper(pc) {
                            pc = wm_tolower(pc);
                        }
                        loop {
                            tc = if cur_t < text.len() { text[cur_t] } else { 0 };
                            if tc == 0 {
                                break;
                            }
                            if !(match_slash || tc != b'/') {
                                break;
                            }
                            let mut tcf = tc;
                            if (flags & WM_CASEFOLD) != 0 && wm_isupper(tcf) {
                                tcf = wm_tolower(tcf);
                            }
                            if tcf == pc {
                                break;
                            }
                            cur_t += 1;
                        }
                        // Recompute the casefolded tc for the comparison below.
                        let tc_cmp = {
                            let raw = if cur_t < text.len() { text[cur_t] } else { 0 };
                            if (flags & WM_CASEFOLD) != 0 && wm_isupper(raw) {
                                wm_tolower(raw)
                            } else {
                                raw
                            }
                        };
                        if tc_cmp != pc {
                            if match_slash {
                                return WM_ABORT_ALL;
                            } else {
                                return WM_ABORT_TO_STARSTAR;
                            }
                        }
                    }
                    let matched = dowild(&p[pi..], &text[cur_t..], flags);
                    if matched != WM_NOMATCH {
                        if !match_slash || matched != WM_ABORT_TO_STARSTAR {
                            return matched;
                        }
                    } else {
                        let cur_raw = if cur_t < text.len() { text[cur_t] } else { 0 };
                        if !match_slash && cur_raw == b'/' {
                            return WM_ABORT_TO_STARSTAR;
                        }
                    }
                    cur_t += 1;
                }
                return WM_ABORT_ALL;
            }
            b'[' => {
                pi += 1;
                let mut p_ch2 = if pi < p.len() { p[pi] } else { 0 };
                if p_ch2 == b'^' {
                    p_ch2 = b'!';
                }
                let negated = p_ch2 == b'!';
                if negated {
                    pi += 1;
                    p_ch2 = if pi < p.len() { p[pi] } else { 0 };
                }
                let mut prev_ch: u8 = 0;
                let mut matched = false;
                loop {
                    if p_ch2 == 0 {
                        return WM_ABORT_ALL;
                    }
                    let mut next_prev: u8 = p_ch2;
                    let mut skip_class = false;
                    if p_ch2 == b'\\' {
                        pi += 1;
                        p_ch2 = if pi < p.len() { p[pi] } else { 0 };
                        if p_ch2 == 0 {
                            return WM_ABORT_ALL;
                        }
                        if t_ch == p_ch2 {
                            matched = true;
                        }
                        next_prev = p_ch2;
                    } else if p_ch2 == b'-' && prev_ch != 0 && pi + 1 < p.len() && p[pi + 1] != b']'
                    {
                        pi += 1;
                        p_ch2 = p[pi];
                        if p_ch2 == b'\\' {
                            pi += 1;
                            p_ch2 = if pi < p.len() { p[pi] } else { 0 };
                            if p_ch2 == 0 {
                                return WM_ABORT_ALL;
                            }
                        }
                        if t_ch <= p_ch2 && t_ch >= prev_ch {
                            matched = true;
                        } else if (flags & WM_CASEFOLD) != 0 && wm_islower(t_ch) {
                            let t_up = wm_toupper(t_ch);
                            if t_up <= p_ch2 && t_up >= prev_ch {
                                matched = true;
                            }
                        }
                        next_prev = 0;
                    } else if p_ch2 == b'[' && pi + 1 < p.len() && p[pi + 1] == b':' {
                        // [:class:]
                        let s = pi + 2;
                        let mut scan = s;
                        loop {
                            if scan >= p.len() {
                                break;
                            }
                            if p[scan] == b']' {
                                break;
                            }
                            scan += 1;
                        }
                        pi = scan;
                        p_ch2 = if pi < p.len() { p[pi] } else { 0 };
                        if p_ch2 == 0 {
                            return WM_ABORT_ALL;
                        }
                        // i = p - s - 1 (length of class name); require trailing ':'
                        let class_end = pi; // index of ']'
                        if class_end < s + 1 || p[class_end - 1] != b':' {
                            // Not a real [:class:]; treat '[' as a literal set member.
                            pi = s.wrapping_sub(2);
                            p_ch2 = b'[';
                            if t_ch == p_ch2 {
                                matched = true;
                            }
                            skip_class = true;
                            next_prev = p_ch2;
                        } else {
                            let class = &p[s..class_end - 1];
                            match wm_class_matches(class, t_ch, flags) {
                                Some(true) => matched = true,
                                Some(false) => {}
                                None => return WM_ABORT_ALL,
                            }
                            next_prev = 0;
                        }
                    } else if t_ch == p_ch2 {
                        matched = true;
                    }

                    let _ = skip_class;
                    // next: advance to the next class char
                    prev_ch = next_prev;
                    pi += 1;
                    p_ch2 = if pi < p.len() { p[pi] } else { 0 };
                    if p_ch2 == b']' {
                        break;
                    }
                }
                if matched == negated || ((flags & WM_PATHNAME) != 0 && t_ch == b'/') {
                    return WM_NOMATCH;
                }
                pi += 1;
                ti += 1;
                continue;
            }
            b'\\' => {
                // Literal match with the following character. p[pi+1]=='\0'
                // failure is handled by the default arm below.
                pi += 1;
                let lit = if pi < p.len() { p[pi] } else { 0 };
                let lit = if (flags & WM_CASEFOLD) != 0 && wm_isupper(lit) {
                    wm_tolower(lit)
                } else {
                    lit
                };
                if t_ch != lit {
                    return WM_NOMATCH;
                }
                pi += 1;
                ti += 1;
                continue;
            }
            _ => {
                if t_ch != p_ch {
                    return WM_NOMATCH;
                }
                pi += 1;
                ti += 1;
                continue;
            }
        }
    }

    if ti < text.len() && text[ti] != 0 {
        WM_NOMATCH
    } else {
        WM_MATCH
    }
}

/// Match `pattern` against `text` with git's `wildmatch` semantics.
/// `flags` is a bitwise-OR of [`WM_CASEFOLD`] and [`WM_PATHNAME`].
pub fn wildmatch(pattern: &[u8], text: &[u8], flags: u32) -> bool {
    dowild(pattern, text, flags) == WM_MATCH
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ps(args: &[&str]) -> Pathspec {
        Pathspec::parse(
            args.iter().map(|s| s.as_bytes()),
            PathspecMatchMagic::default(),
        )
        .expect("valid pathspec")
    }

    #[test]
    fn empty_pathspec_matches_everything() {
        let p = Pathspec::default();
        assert!(p.is_empty());
        assert!(p.matches(b"any/path"));
    }

    #[test]
    fn literal_prefix_matches_directory_recursively() {
        let p = ps(&["src"]);
        assert!(p.matches(b"src"));
        assert!(p.matches(b"src/lib.rs"));
        assert!(!p.matches(b"srcs/lib.rs"));
        assert!(!p.matches(b"other"));
    }

    #[test]
    fn exclude_subtracts_from_includes() {
        let p = ps(&["src", ":(exclude)src/gen"]);
        assert!(p.matches(b"src/lib.rs"));
        assert!(!p.matches(b"src/gen/x.rs"));
    }

    #[test]
    fn exclude_shorthand_sigils() {
        for spec in [":!foo", ":^foo"] {
            let p = ps(&[spec]);
            assert!(p.elements()[0].is_exclude());
            // exclude-only pathspec keeps everything but the excluded path.
            assert!(p.matches(b"bar"));
            assert!(!p.matches(b"foo"));
        }
    }

    #[test]
    fn icase_magic_folds_case() {
        let p = ps(&[":(icase)readme"]);
        assert!(p.matches(b"README"));
        assert!(p.matches(b"readme"));
        let plain = ps(&["readme"]);
        assert!(!plain.matches(b"README"));
    }

    #[test]
    fn glob_magic_is_pathname_aware() {
        // :(glob)*.rs uses WM_PATHNAME so `*` does not cross `/`.
        let p = ps(&[":(glob)*.rs"]);
        assert!(p.matches(b"lib.rs"));
        assert!(!p.matches(b"src/lib.rs"));
        // ** spans directories under glob magic.
        let pp = ps(&[":(glob)**/*.rs"]);
        assert!(pp.matches(b"src/lib.rs"));
    }

    #[test]
    fn default_wildcard_can_cross_directory_separator() {
        let p = ps(&["*file3"]);
        assert!(p.matches(b"file3"));
        assert!(p.matches(b"subdir/file3"));

        let glob = ps(&[":(glob)*file3"]);
        assert!(glob.matches(b"file3"));
        assert!(!glob.matches(b"subdir/file3"));
    }

    #[test]
    fn literal_magic_disables_wildcards() {
        let p = ps(&[":(literal)a*b"]);
        assert!(p.matches(b"a*b"));
        assert!(!p.matches(b"axxb"));
    }

    #[test]
    fn backslash_marks_pathspec_as_glob_special() {
        assert!(pathspec_is_glob(br"a\*b"));
        assert!(pathspec_is_glob(br"a\?b"));
        assert!(pathspec_is_glob(br"a\[b"));
        assert!(!pathspec_is_glob(b"plain/path"));
    }

    #[test]
    fn escaped_wildcards_match_literal_bytes() {
        let p = ps(&[r"a\*b", r"a\?b", r"a\[b"]);
        assert!(p.matches(b"a*b"));
        assert!(p.matches(b"a?b"));
        assert!(p.matches(b"a[b"));
        assert!(!p.matches(b"axxb"));
        assert!(!p.matches(b"acb"));
    }

    #[test]
    fn explicit_glob_literal_magic_overrides_global_defaults() {
        let noglob_default = PathspecMatchMagic {
            literal: true,
            glob: false,
            icase: false,
            literal_pathspecs: false,
        };
        let glob = PathspecElement::parse(b":(glob)*.rs", noglob_default).expect("glob override");
        assert!(glob.is_glob());
        assert!(!glob.magic().literal);
        assert!(glob.matches_path(b"lib.rs"));
        assert!(!glob.matches_path(b"src/lib.rs"));

        let glob_default = PathspecMatchMagic {
            literal: false,
            glob: true,
            icase: false,
            literal_pathspecs: false,
        };
        let literal =
            PathspecElement::parse(b":(literal)*.rs", glob_default).expect("literal override");
        assert!(!literal.is_glob());
        assert!(literal.magic().literal);
        assert!(literal.matches_path(b"*.rs"));
        assert!(!literal.matches_path(b"lib.rs"));
    }

    #[test]
    fn top_magic_is_parsed() {
        let p = ps(&[":(top)src", ":/other"]);
        assert!(p.elements()[0].is_top());
        assert!(p.elements()[1].is_top());
    }

    #[test]
    fn attr_magic_is_retained() {
        let p = ps(&[":(attr:binary)data"]);
        assert_eq!(p.elements()[0].attrs(), &[b"binary".to_vec()]);
        assert_eq!(p.elements()[0].pattern(), b"data");
    }

    #[test]
    fn combined_magic_words() {
        let p = ps(&[":(exclude,icase)Cargo.lock"]);
        let el = &p.elements()[0];
        assert!(el.is_exclude());
        // exclude is case-insensitive: CARGO.LOCK is subtracted too.
        assert!(!p.matches(b"CARGO.LOCK"));
    }

    fn parse_err(arg: &[u8]) -> PathspecParseError {
        match Pathspec::parse([arg], PathspecMatchMagic::default()) {
            Ok(_) => panic!(
                "expected parse error for {:?}",
                String::from_utf8_lossy(arg)
            ),
            Err(e) => e,
        }
    }

    #[test]
    fn glob_literal_conflict_is_error() {
        assert_eq!(
            parse_err(b":(glob,literal)x"),
            PathspecParseError::GlobLiteralConflict
        );
    }

    #[test]
    fn unknown_magic_is_error() {
        assert!(matches!(
            parse_err(b":(bogus)x"),
            PathspecParseError::UnknownMagic(_)
        ));
    }

    #[test]
    fn unterminated_magic_is_error() {
        assert_eq!(
            parse_err(b":(exclude"),
            PathspecParseError::UnterminatedMagic
        );
    }

    #[test]
    fn exclude_only_keeps_unmatched() {
        let p = ps(&[":(exclude)target"]);
        assert!(p.matches(b"src/lib.rs"));
        assert!(!p.matches(b"target/debug"));
    }
}
