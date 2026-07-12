//! Pattern matching: sparse cone (`SparseMatcher`), wildmatch, and the `.gitattributes` matcher + parser.
//!
//! Split out of `lib.rs` in the wave-47 mechanical refactor: a pure code move
//! (no function body changed); all items are re-exported from `lib.rs`.
use super::*;
use crate::ignore::*;
use crate::index::*;
use crate::index_io::*;
use crate::types_admin::*;

/// Decides whether a worktree path is included by a [`SparseCheckout`].
///
/// In [`SparseCheckoutMode::Full`] the sparse patterns are compiled with the
/// same `.gitignore` grammar used elsewhere in this crate ([`IgnorePattern`]);
/// a path is *in cone* when the last matching pattern is positive. In
/// [`SparseCheckoutMode::Cone`] the patterns are reduced to a set of recursive
/// directory prefixes plus a flag for whether top-level files are kept, and
/// inclusion is decided by literal prefix containment.
#[derive(Debug)]
pub(crate) enum SparseMatcher {
    Full { patterns: Vec<IgnorePattern> },
    Cone(ConeMatcher),
}

#[derive(Debug, Default)]
pub(crate) struct ConeMatcher {
    /// `true` when files directly at the repository root are in cone (`/*`).
    root_files: bool,
    /// Directory prefixes (without leading or trailing `/`) whose entire
    /// subtree is in cone, e.g. `dir1/dir2`.
    recursive_dirs: Vec<Vec<u8>>,
    /// Parent directories that are in cone only for their direct files
    /// (the `/dir/*` guard Git emits so intermediate directories keep their
    /// own files). Stored without leading or trailing `/`.
    parent_dirs: Vec<Vec<u8>>,
}

impl SparseMatcher {
    pub(crate) fn new(sparse: &SparseCheckout, mode: SparseCheckoutMode) -> Self {
        let resolved = match mode {
            SparseCheckoutMode::Auto => {
                if patterns_are_cone(&sparse.patterns) {
                    SparseCheckoutMode::Cone
                } else {
                    SparseCheckoutMode::Full
                }
            }
            other => other,
        };
        match resolved {
            SparseCheckoutMode::Cone => SparseMatcher::Cone(ConeMatcher::compile(&sparse.patterns)),
            // `Auto` has been resolved above; everything else is full matching.
            _ => {
                let mut patterns = Vec::new();
                for pattern in &sparse.patterns {
                    push_ignore_pattern(&mut patterns, pattern, &[], b"sparse-checkout", 0);
                }
                SparseMatcher::Full { patterns }
            }
        }
    }

    /// Returns `true` when the given file path should be present in the
    /// worktree under this sparse specification.
    pub(crate) fn includes_file(&self, path: &[u8]) -> bool {
        match self {
            SparseMatcher::Full { patterns } => {
                let mut end = path.len();
                let mut is_dir = false;
                while end > 0 {
                    let candidate = &path[..end];
                    let mut matched = None;
                    for pattern in patterns {
                        if sparse_pattern_matches_candidate(pattern, candidate, is_dir) {
                            matched = Some(!pattern.negated);
                        }
                    }
                    if let Some(included) = matched {
                        return included;
                    }
                    let Some(slash) = candidate.iter().rposition(|byte| *byte == b'/') else {
                        break;
                    };
                    end = slash;
                    is_dir = true;
                }
                false
            }
            SparseMatcher::Cone(cone) => cone.includes_file(path),
        }
    }
}

fn sparse_pattern_matches_candidate(pattern: &IgnorePattern, path: &[u8], is_dir: bool) -> bool {
    if !pattern.directory_only {
        return pattern.matches(path, is_dir);
    }
    if !is_dir {
        return false;
    }
    sparse_directory_pattern_matches_candidate(pattern, path)
}

fn sparse_directory_pattern_matches_candidate(pattern: &IgnorePattern, path: &[u8]) -> bool {
    let path = if pattern.base.is_empty() {
        path
    } else {
        let Some(rest) = path
            .strip_prefix(pattern.base.as_slice())
            .and_then(|rest| rest.strip_prefix(b"/"))
        else {
            return false;
        };
        rest
    };
    if pattern.anchored || pattern.has_slash {
        return pattern.match_path(path);
    }
    pattern.match_segment(path_basename(path))
}

impl ConeMatcher {
    fn compile(patterns: &[Vec<u8>]) -> Self {
        let mut matcher = ConeMatcher::default();
        let mut positive_dirs = Vec::new();
        let mut guarded_parent_dirs = BTreeSet::new();
        for raw in patterns {
            let line = sparse_clean_line(raw);
            if line.is_empty() || line.starts_with(b"#") {
                continue;
            }
            if line.starts_with(b"!") {
                if let Some(rest) = line.strip_prefix(b"!/")
                    && let Some(dir) = rest.strip_suffix(b"/*/")
                    && !dir.is_empty()
                {
                    guarded_parent_dirs.insert(unescape_sparse_cone_dir(dir));
                }
                continue;
            }
            if line == b"/*" {
                matcher.root_files = true;
                continue;
            }
            // `/dir/` -> recursive subtree.
            if let Some(rest) = line.strip_prefix(b"/")
                && let Some(dir) = rest.strip_suffix(b"/")
                && !dir.is_empty()
            {
                positive_dirs.push(unescape_sparse_cone_dir(dir));
                continue;
            }
            // `/dir/*` -> direct files of `dir` only (parent guard).
            if let Some(rest) = line.strip_prefix(b"/")
                && let Some(dir) = rest.strip_suffix(b"/*")
                && !dir.is_empty()
            {
                matcher.parent_dirs.push(unescape_sparse_cone_dir(dir));
                continue;
            }
        }
        for dir in positive_dirs {
            if guarded_parent_dirs.contains(&dir) {
                matcher.parent_dirs.push(dir);
            } else {
                matcher.recursive_dirs.push(dir);
            }
        }
        matcher
    }

    pub(crate) fn includes_file(&self, path: &[u8]) -> bool {
        let parent = match path.iter().rposition(|byte| *byte == b'/') {
            Some(index) => &path[..index],
            None => {
                // A path with no slash is a top-level file.
                return self.root_files;
            }
        };
        if self
            .recursive_dirs
            .iter()
            .any(|dir| path_is_under_dir(path, dir))
        {
            return true;
        }
        self.parent_dirs.iter().any(|dir| dir.as_slice() == parent)
    }
}

/// Strips a CR, leading/trailing whitespace, and an optional trailing slash is
/// preserved (cone patterns are slash sensitive) from a raw sparse line.
pub(crate) fn sparse_clean_line(raw: &[u8]) -> &[u8] {
    let line = raw.strip_suffix(b"\r").unwrap_or(raw);
    trim_ascii_whitespace(line)
}

/// Returns `true` when `path` is the directory `dir` itself or lives anywhere
/// beneath it.
pub(crate) fn path_is_under_dir(path: &[u8], dir: &[u8]) -> bool {
    if dir.is_empty() {
        return true;
    }
    path.strip_prefix(dir)
        .is_some_and(|rest| rest.first() == Some(&b'/'))
}

/// Heuristic used by [`SparseCheckoutMode::Auto`]: the pattern set is cone
/// shaped when every (non-comment, non-blank) line is one of the restricted
/// cone forms Git emits.
pub(crate) fn patterns_are_cone(patterns: &[Vec<u8>]) -> bool {
    let mut saw_pattern = false;
    for raw in patterns {
        let line = sparse_clean_line(raw);
        if line.is_empty() || line.starts_with(b"#") {
            continue;
        }
        saw_pattern = true;
        let body = line.strip_prefix(b"!").unwrap_or(line);
        let is_cone_shaped = body == b"/*"
            || body == b"/*/"
            || (body.starts_with(b"/")
                && (body.ends_with(b"/") || body.ends_with(b"/*"))
                && !sparse_has_unescaped_glob_meta(body));
        if !is_cone_shaped {
            return false;
        }
    }
    saw_pattern
}

/// Detects glob metacharacters that disqualify a line from cone interpretation.
/// A single trailing `/*` is allowed by the caller and handled separately.
pub(crate) fn sparse_has_unescaped_glob_meta(body: &[u8]) -> bool {
    let trimmed = body.strip_suffix(b"/*").unwrap_or(body);
    for (index, byte) in trimmed.iter().enumerate() {
        if !matches!(*byte, b'*' | b'?' | b'[' | b']' | b'\\') {
            continue;
        }
        let prev = index.checked_sub(1).and_then(|i| trimmed.get(i)).copied();
        let next = trimmed.get(index + 1).copied();
        if prev == Some(b'\\') {
            continue;
        }
        if *byte == b'\\' && matches!(next, Some(b'*' | b'?' | b'[' | b'\\')) {
            continue;
        }
        return true;
    }
    false
}

pub(crate) fn unescape_sparse_cone_dir(path: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(path.len());
    let mut iter = path.iter().copied();
    while let Some(byte) = iter.next() {
        if byte == b'\\'
            && let Some(next @ (b'*' | b'?' | b'[' | b'\\')) = iter.next()
        {
            out.push(next);
            continue;
        }
        out.push(byte);
    }
    out
}

pub(crate) fn read_core_excludes_file(root: &Path, patterns: &mut Vec<IgnorePattern>) -> bool {
    let Ok(config) = sley_config::read_repo_config(&root.join(".git"), None) else {
        return false;
    };
    let Some(value) = config.get("core", None, "excludesFile") else {
        return false;
    };
    let path = expand_core_excludes_file(root, value);
    read_ignore_patterns(path, patterns, &[], value.as_bytes());
    true
}

pub(crate) fn expand_core_excludes_file(root: &Path, value: &str) -> PathBuf {
    let path = Path::new(value);
    if path.is_absolute() {
        return path.to_path_buf();
    }
    if let Some(rest) = value.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    root.join(path)
}

pub(crate) fn read_default_global_excludes_file(patterns: &mut Vec<IgnorePattern>) {
    if let Some(config_home) = std::env::var_os("XDG_CONFIG_HOME")
        && !config_home.is_empty()
    {
        let path = PathBuf::from(config_home).join("git").join("ignore");
        let source = path.to_string_lossy().into_owned();
        read_ignore_patterns(path, patterns, &[], source.as_bytes());
        return;
    }
    if let Some(home) = std::env::var_os("HOME") {
        let path = PathBuf::from(home)
            .join(".config")
            .join("git")
            .join("ignore");
        let source = path.to_string_lossy().into_owned();
        read_ignore_patterns(path, patterns, &[], source.as_bytes());
    }
}

pub(crate) fn collect_per_directory_patterns_into_matcher(
    root: &Path,
    dir: &Path,
    names: &[String],
    matcher: &mut IgnoreMatcher,
) -> Result<()> {
    for name in names {
        let path = dir.join(name);
        let relative = dir.strip_prefix(root).map_err(|_| {
            GitError::InvalidPath(format!("path {} is outside worktree", dir.display()))
        })?;
        let base = git_path_bytes(relative)?;
        let mut source = base.clone();
        if !source.is_empty() {
            source.push(b'/');
        }
        source.extend_from_slice(name.as_bytes());
        read_per_directory_ignore_patterns_into_matcher(&path, matcher, &base, &source)?;
    }

    let mut entries = fs::read_dir(dir)?.collect::<std::result::Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        if path.file_name().and_then(|name| name.to_str()) == Some(".git") {
            continue;
        }
        let metadata = entry.file_type()?;
        if metadata.is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            let relative = path.strip_prefix(root).map_err(|_| {
                GitError::InvalidPath(format!("path {} is outside worktree", path.display()))
            })?;
            let git_path = git_path_bytes(relative)?;
            if !matcher.is_ignored(&git_path, true) {
                collect_per_directory_patterns_into_matcher(root, &path, names, matcher)?;
            }
        }
    }
    Ok(())
}

pub(crate) fn read_ignore_patterns(
    path: impl AsRef<Path>,
    patterns: &mut Vec<IgnorePattern>,
    base: &[u8],
    source: &[u8],
) {
    let Ok(contents) = fs::read(path) else {
        return;
    };
    for (line, raw) in contents.split(|byte| *byte == b'\n').enumerate() {
        push_ignore_pattern(patterns, raw, base, source, line + 1);
    }
}

pub(crate) fn read_per_directory_ignore_patterns_into_matcher(
    path: impl AsRef<Path>,
    matcher: &mut IgnoreMatcher,
    base: &[u8],
    source: &[u8],
) -> Result<()> {
    let path = path.as_ref();
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(GitError::Io(err.to_string())),
    };
    if metadata.file_type().is_symlink() {
        return Err(GitError::Command(format!(
            "unable to access '{}'",
            path.display()
        )));
    }
    if !metadata.is_file() {
        return Ok(());
    }
    let contents = fs::read(path)?;
    for (line, raw) in contents.split(|byte| *byte == b'\n').enumerate() {
        matcher.push_raw_pattern(raw, base, source, line + 1);
    }
    Ok(())
}

pub(crate) fn push_ignore_pattern(
    patterns: &mut Vec<IgnorePattern>,
    raw: &[u8],
    base: &[u8],
    source: &[u8],
    line_number: usize,
) {
    if let Some(pattern) = parse_ignore_pattern(raw, base, source, line_number) {
        patterns.push(pattern);
    }
}

pub(crate) fn parse_ignore_pattern(
    raw: &[u8],
    base: &[u8],
    source: &[u8],
    line_number: usize,
) -> Option<IgnorePattern> {
    let raw = if line_number == 1 {
        raw.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(raw)
    } else {
        raw
    };
    let mut line = raw.strip_suffix(b"\r").unwrap_or(raw).to_vec();
    normalize_ignore_trailing_spaces(&mut line);
    let original = line.clone();
    let mut line = line.as_slice();
    if line.is_empty() || line.starts_with(b"#") {
        return None;
    }
    let negated = if line.starts_with(b"\\#") || line.starts_with(b"\\!") {
        line = &line[1..];
        false
    } else if let Some(pattern) = line.strip_prefix(b"!") {
        line = pattern;
        true
    } else {
        false
    };
    let directory_only = line.ends_with(b"/");
    let pattern = if directory_only {
        line.strip_suffix(b"/").unwrap_or(line)
    } else {
        line
    };
    let (anchored, pattern) = if let Some(pattern) = pattern.strip_prefix(b"/") {
        (true, pattern)
    } else {
        (false, pattern)
    };
    // A leading `**/` followed by a slash-free segment is, per gitignore,
    // identical to the bare segment ("match in all directories"): `**/Pods` ≡
    // `Pods`, `**/*.jks` ≡ `*.jks`. Collapse it so the pattern matches the
    // basename directly (a literal/suffix compare) instead of paying for the
    // `**` wildcard engine on the full path — verified against `git check-ignore`.
    let pattern = match pattern.strip_prefix(b"**/") {
        Some(rest) if !rest.is_empty() && !rest.contains(&b'/') => rest,
        _ => pattern,
    };
    if pattern.is_empty() {
        return None;
    }
    let match_kind = classify_ignore_pattern(pattern);
    let glob_literal_prefix_len = if match_kind == MatchKind::Glob {
        pattern
            .iter()
            .position(|byte| matches!(byte, b'*' | b'?' | b'[' | b'\\'))
            .unwrap_or(pattern.len())
    } else {
        0
    };
    Some(IgnorePattern {
        base: base.to_vec(),
        pattern: pattern.to_vec(),
        original,
        source: source.to_vec(),
        line_number,
        negated,
        directory_only,
        anchored,
        has_slash: pattern.contains(&b'/'),
        match_kind,
        glob_literal_prefix_len,
    })
}

pub(crate) fn normalize_ignore_trailing_spaces(line: &mut Vec<u8>) {
    while line.last() == Some(&b' ') {
        let space_index = line.len() - 1;
        let backslashes = line[..space_index]
            .iter()
            .rev()
            .take_while(|byte| **byte == b'\\')
            .count();
        if backslashes % 2 == 1 {
            line.remove(space_index - 1);
            break;
        }
        line.pop();
    }
}

impl IgnorePattern {
    pub(crate) fn bucket_kind(&self) -> IgnoreBucketKind {
        if self.match_kind == MatchKind::PathSuffix {
            return if self.directory_only {
                IgnoreBucketKind::DirectoryPathSuffixBasename
            } else {
                IgnoreBucketKind::PathSuffixBasename
            };
        }
        if (self.anchored || self.has_slash) && self.match_kind == MatchKind::Literal {
            return if self.directory_only {
                IgnoreBucketKind::DirectoryLiteralPathBasename
            } else {
                IgnoreBucketKind::LiteralPathBasename
            };
        }
        if self.has_slash
            && self.match_kind == MatchKind::Glob
            && !self.directory_only
            && !path_component_has_glob_meta(path_basename(&self.pattern))
        {
            return IgnoreBucketKind::GlobPathLiteralBasename;
        }
        if self.has_slash
            && self.match_kind == MatchKind::Glob
            && self.directory_only
            && !path_component_has_glob_meta(path_basename(&self.pattern))
        {
            return IgnoreBucketKind::GlobDirectoryLiteralBasename;
        }
        if self.has_slash && self.match_kind == MatchKind::Glob {
            return match (
                self.directory_only,
                final_component_match_kind(&self.pattern),
            ) {
                (false, MatchKind::Suffix) => IgnoreBucketKind::GlobPathSuffixBasename,
                (false, MatchKind::Prefix) => IgnoreBucketKind::GlobPathPrefixBasename,
                (true, MatchKind::Suffix) => IgnoreBucketKind::GlobDirectorySuffixBasename,
                (true, MatchKind::Prefix) => IgnoreBucketKind::GlobDirectoryPrefixBasename,
                _ => IgnoreBucketKind::Other,
            };
        }
        if self.anchored || self.has_slash {
            return IgnoreBucketKind::Other;
        }
        match (self.directory_only, self.match_kind) {
            (false, MatchKind::Literal) => IgnoreBucketKind::LiteralBasename,
            (true, MatchKind::Literal) => IgnoreBucketKind::DirectoryLiteralBasename,
            (false, MatchKind::Suffix) => IgnoreBucketKind::SuffixBasename,
            (false, MatchKind::Prefix) => IgnoreBucketKind::PrefixBasename,
            _ => IgnoreBucketKind::Other,
        }
    }

    pub(crate) fn base_matches(&self, path: &[u8]) -> bool {
        if self.base.is_empty() {
            return true;
        }
        path.strip_prefix(self.base.as_slice())
            .is_some_and(|rest| rest.starts_with(b"/"))
    }

    pub(crate) fn to_match(&self) -> IgnoreMatch {
        IgnoreMatch {
            source: self.source.clone(),
            line_number: self.line_number,
            pattern: self.original.clone(),
            ignored: !self.negated,
        }
    }

    fn matches(&self, path: &[u8], is_dir: bool) -> bool {
        let basename = path_basename(path);
        self.matches_with_basename(path, basename, is_dir)
    }

    pub(crate) fn glob_literal_prefix_matches(
        &self,
        path: &[u8],
        basename: &[u8],
        is_dir: bool,
    ) -> bool {
        if self.match_kind != MatchKind::Glob {
            return true;
        }
        if self.glob_literal_prefix_len == 0 {
            return true;
        }
        let prefix = &self.pattern[..self.glob_literal_prefix_len];
        let scoped_path = if self.base.is_empty() {
            path
        } else {
            let Some(rest) = path
                .strip_prefix(self.base.as_slice())
                .and_then(|rest| rest.strip_prefix(b"/"))
            else {
                return false;
            };
            rest
        };
        if self.anchored || self.has_slash {
            return scoped_path.starts_with(prefix);
        }
        if self.directory_only && !is_dir {
            return true;
        }
        basename.starts_with(prefix)
    }

    pub(crate) fn matches_with_basename(&self, path: &[u8], basename: &[u8], is_dir: bool) -> bool {
        let path = if self.base.is_empty() {
            path
        } else {
            let Some(rest) = path
                .strip_prefix(self.base.as_slice())
                .and_then(|rest| rest.strip_prefix(b"/"))
            else {
                return false;
            };
            rest
        };
        if self.directory_only {
            return self.matches_directory(path, is_dir);
        }
        if self.anchored || self.has_slash {
            return self.match_segment(path);
        }
        self.match_segment(basename)
    }

    fn matches_directory(&self, path: &[u8], is_dir: bool) -> bool {
        if self.anchored || self.has_slash {
            if is_dir && self.match_path(path) {
                return true;
            }
            // For a *file* path, a directory-only pattern can only apply
            // through an *ancestor* directory of the file: the leaf is matched
            // only because it lives inside a directory the pattern excludes
            // (e.g. `/tmp-*/` excludes `tmp-info-only`, so `tmp-info-only/x`
            // is excluded too). Upstream git models this through directory
            // traversal — `last_matching_pattern` skips a MUSTBEDIR pattern for
            // a non-directory leaf (`dtype != DT_DIR`), and a file is excluded
            // only when one of its parent directories is excluded.
            //
            // A *negated* directory-only pattern (`!data/**/`) re-includes a
            // directory but, per git, does NOT re-include the files inside it
            // (git's docs: "it is not possible to re-include a file if a parent
            // directory of that file is excluded" — re-including the dir with
            // `!dir/` still requires an explicit `!dir/*` to reach its files).
            // So a negated directory-only pattern must never match a file via
            // its ancestor, otherwise it wrongly wins the leaf scan and
            // un-ignores a file that an earlier positive pattern ignored
            // (t0008-ignores "directories and ** matches": `data/**` +
            // `!data/**/` must leave `data/data1/file1` ignored).
            if self.negated {
                return false;
            }
            return path
                .iter()
                .enumerate()
                .any(|(idx, byte)| *byte == b'/' && self.match_path(&path[..idx]));
        }
        let mut components = path.split(|byte| *byte == b'/').peekable();
        while let Some(component) = components.next() {
            if self.match_segment(component) && (is_dir || components.peek().is_some()) {
                return true;
            }
        }
        false
    }

    fn match_path(&self, value: &[u8]) -> bool {
        match self.match_kind {
            MatchKind::Literal => self.pattern == value,
            MatchKind::Suffix => !value.contains(&b'/') && value.ends_with(&self.pattern[1..]),
            MatchKind::Prefix => {
                !value.contains(&b'/') && value.starts_with(&self.pattern[..self.pattern.len() - 1])
            }
            MatchKind::PathSuffix => {
                let suffix = &self.pattern[3..];
                value
                    .strip_suffix(suffix)
                    .is_some_and(|prefix| prefix.is_empty() || prefix.ends_with(b"/"))
            }
            MatchKind::Glob => wildcard_path_matches(&self.pattern, value),
        }
    }

    /// Match a slash-free `value` (a basename or path component) against this
    /// pattern. Literal and simple `*X`/`X*` patterns resolve with a direct
    /// comparison; only complex globs pay for the allocating wildcard engine.
    fn match_segment(&self, value: &[u8]) -> bool {
        self.match_path(value)
    }
}

thread_local! {
    /// Reused dynamic-programming scratch for [`wildcard_path_matches`]. Flat
    /// `(pattern.len()+1) * (value.len()+1)` grid of memoised results, kept across
    /// calls so the hot ignore/attribute matching loop never reallocates.
    static WILDCARD_MEMO: RefCell<Vec<Option<bool>>> = const { RefCell::new(Vec::new()) };
}

pub(crate) fn wildcard_path_matches(pattern: &[u8], value: &[u8]) -> bool {
    let stride = value.len() + 1;
    let cells = (pattern.len() + 1) * stride;
    WILDCARD_MEMO.with_borrow_mut(|memo| {
        // One reused allocation; clearing then resizing fills the grid with `None`.
        memo.clear();
        memo.resize(cells, None);
        wildcard_path_matches_from(pattern, value, 0, 0, memo, stride)
    })
}

pub(crate) fn wildcard_path_matches_from(
    pattern: &[u8],
    value: &[u8],
    pattern_index: usize,
    value_index: usize,
    memo: &mut [Option<bool>],
    stride: usize,
) -> bool {
    let cell = pattern_index * stride + value_index;
    if let Some(cached) = memo[cell] {
        return cached;
    }
    let matched = if pattern_index == pattern.len() {
        value_index == value.len()
    } else {
        match pattern[pattern_index] {
            b'*' if pattern.get(pattern_index + 1) == Some(&b'*') => wildcard_double_star_matches(
                pattern,
                value,
                pattern_index,
                value_index,
                memo,
                stride,
            ),
            b'*' => {
                if wildcard_path_matches_from(
                    pattern,
                    value,
                    pattern_index + 1,
                    value_index,
                    memo,
                    stride,
                ) {
                    true
                } else {
                    let mut next = value_index;
                    while next < value.len() && value[next] != b'/' {
                        next += 1;
                        if wildcard_path_matches_from(
                            pattern,
                            value,
                            pattern_index + 1,
                            next,
                            memo,
                            stride,
                        ) {
                            return true;
                        }
                    }
                    false
                }
            }
            b'?' => {
                value_index < value.len()
                    && value[value_index] != b'/'
                    && wildcard_path_matches_from(
                        pattern,
                        value,
                        pattern_index + 1,
                        value_index + 1,
                        memo,
                        stride,
                    )
            }
            b'[' => {
                if value_index < value.len() && value[value_index] != b'/' {
                    if let Some((class_matches, next_pattern_index)) =
                        wildcard_class_matches(pattern, pattern_index, value[value_index])
                    {
                        class_matches
                            && wildcard_path_matches_from(
                                pattern,
                                value,
                                next_pattern_index,
                                value_index + 1,
                                memo,
                                stride,
                            )
                    } else {
                        value[value_index] == b'['
                            && wildcard_path_matches_from(
                                pattern,
                                value,
                                pattern_index + 1,
                                value_index + 1,
                                memo,
                                stride,
                            )
                    }
                } else {
                    false
                }
            }
            b'\\' if pattern_index + 1 < pattern.len() => {
                value_index < value.len()
                    && pattern[pattern_index + 1] == value[value_index]
                    && wildcard_path_matches_from(
                        pattern,
                        value,
                        pattern_index + 2,
                        value_index + 1,
                        memo,
                        stride,
                    )
            }
            literal => {
                value_index < value.len()
                    && literal == value[value_index]
                    && wildcard_path_matches_from(
                        pattern,
                        value,
                        pattern_index + 1,
                        value_index + 1,
                        memo,
                        stride,
                    )
            }
        }
    };
    memo[cell] = Some(matched);
    matched
}

pub(crate) fn wildcard_double_star_matches(
    pattern: &[u8],
    value: &[u8],
    pattern_index: usize,
    value_index: usize,
    memo: &mut [Option<bool>],
    stride: usize,
) -> bool {
    let after_stars = pattern_index + 2;
    if pattern.get(after_stars) == Some(&b'/') {
        if wildcard_path_matches_from(pattern, value, after_stars + 1, value_index, memo, stride) {
            return true;
        }
        for next in value_index..value.len() {
            if value[next] == b'/'
                && wildcard_path_matches_from(
                    pattern,
                    value,
                    after_stars + 1,
                    next + 1,
                    memo,
                    stride,
                )
            {
                return true;
            }
        }
        return false;
    }
    for next in value_index..=value.len() {
        if wildcard_path_matches_from(pattern, value, after_stars, next, memo, stride) {
            return true;
        }
    }
    false
}

pub(crate) fn wildcard_class_matches(
    pattern: &[u8],
    start: usize,
    value: u8,
) -> Option<(bool, usize)> {
    let mut index = start + 1;
    let negated = matches!(pattern.get(index), Some(b'!' | b'^'));
    if negated {
        index += 1;
    }
    let class_start = index;
    let end = pattern[class_start..]
        .iter()
        .position(|byte| *byte == b']')
        .map(|position| class_start + position)?;
    if end == class_start {
        return None;
    }
    let mut matched = false;
    while index < end {
        if index + 2 < end && pattern[index + 1] == b'-' {
            let lower = pattern[index].min(pattern[index + 2]);
            let upper = pattern[index].max(pattern[index + 2]);
            matched |= lower <= value && value <= upper;
            index += 3;
        } else {
            matched |= pattern[index] == value;
            index += 1;
        }
    }
    Some((if negated { !matched } else { matched }, end + 1))
}

#[derive(Debug, Default)]
pub(crate) struct AttributeMatcher {
    pub(crate) patterns: Vec<AttributePattern>,
    pub(crate) attribute_order: BTreeMap<Vec<u8>, usize>,
    pub(crate) macros: BTreeMap<Vec<u8>, Vec<AttributeAssignment>>,
    pub(crate) ignore_case: bool,
}

#[derive(Debug)]
pub(crate) struct AttributePattern {
    base: Vec<u8>,
    pattern: Vec<u8>,
    ignore_case_pattern: Option<Vec<u8>>,
    anchored: bool,
    has_slash: bool,
    assignments: Vec<AttributeAssignment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AttributeAssignment {
    attribute: Vec<u8>,
    state: Option<AttributeState>,
}

impl AttributeMatcher {
    pub(crate) fn from_worktree_root(root: &Path) -> Result<Self> {
        let mut matcher = Self::default();
        let git_dir = root.join(".git");
        matcher.configure_case_sensitivity(&git_dir);
        if !matcher.read_configured_attributes(root, &git_dir) {
            matcher.read_default_global_attributes();
        }
        collect_attribute_patterns(root, root, &mut matcher)?;
        read_attribute_patterns(
            git_dir.join("info").join("attributes"),
            &mut matcher,
            &[],
            b".git/info/attributes",
            false,
        );
        Ok(matcher)
    }

    /// Builds only the repository-wide attribute sources — `core.attributesFile`
    /// (or the default global) and `$GIT_DIR/info/attributes` — *without* walking
    /// the worktree for `.gitattributes`. The caller is expected to fold each
    /// directory's `.gitattributes` into the matcher as it descends (see
    /// [`read_dir_attribute_patterns`]), so status/diff read the tree exactly once
    /// instead of doing a separate full-tree attribute pass. Lower-priority sources
    /// are added first, so in-tree patterns added during the walk take precedence —
    /// matching git's lookup order.
    pub(crate) fn from_worktree_base(root: &Path) -> Self {
        let mut matcher = Self::default();
        let git_dir = root.join(".git");
        matcher.configure_case_sensitivity(&git_dir);
        if !matcher.read_configured_attributes(root, &git_dir) {
            matcher.read_default_global_attributes();
        }
        read_attribute_patterns(
            git_dir.join("info").join("attributes"),
            &mut matcher,
            &[],
            b".git/info/attributes",
            false,
        );
        matcher
    }

    pub(crate) fn attributes_for_path(
        &self,
        path: &[u8],
        requested: &[Vec<u8>],
        all: bool,
    ) -> Vec<AttributeCheck> {
        let mut states = BTreeMap::<Vec<u8>, Option<AttributeState>>::new();
        for pattern in &self.patterns {
            if !pattern.matches(path, self.ignore_case) {
                continue;
            }
            for assignment in &pattern.assignments {
                self.apply_attribute_assignment(&mut states, assignment);
            }
        }
        if all {
            let mut checks = states
                .into_iter()
                .filter_map(|(attribute, state)| {
                    state.map(|state| AttributeCheck {
                        attribute,
                        state: Some(state),
                    })
                })
                .collect::<Vec<_>>();
            checks.sort_by(|left, right| {
                attribute_all_rank(&left.attribute, &self.attribute_order)
                    .cmp(&attribute_all_rank(&right.attribute, &self.attribute_order))
                    .then_with(|| left.attribute.cmp(&right.attribute))
            });
            return checks;
        }
        requested
            .iter()
            .map(|attribute| AttributeCheck {
                attribute: attribute.clone(),
                state: states.get(attribute).cloned().flatten(),
            })
            .collect()
    }

    fn push_attribute_order(&mut self, attribute: &[u8]) {
        let next = self.attribute_order.len();
        self.attribute_order
            .entry(attribute.to_vec())
            .or_insert(next);
    }

    fn apply_attribute_assignment(
        &self,
        states: &mut BTreeMap<Vec<u8>, Option<AttributeState>>,
        assignment: &AttributeAssignment,
    ) {
        let mut stack = vec![assignment.clone()];
        let mut expanded = 0usize;
        while let Some(assignment) = stack.pop() {
            states.insert(assignment.attribute.clone(), assignment.state.clone());
            if assignment.state != Some(AttributeState::Set) {
                continue;
            }
            let Some(macro_assignments) = self.macros.get(&assignment.attribute) else {
                continue;
            };
            expanded += 1;
            if expanded > 10000 {
                break;
            }
            for macro_assignment in macro_assignments.iter().rev() {
                stack.push(macro_assignment.clone());
            }
        }
    }

    pub(crate) fn configure_case_sensitivity(&mut self, git_dir: &Path) {
        let Ok(config) = sley_config::read_repo_config(git_dir, None) else {
            return;
        };
        self.ignore_case = config.get_bool("core", None, "ignorecase").unwrap_or(false);
    }

    pub(crate) fn read_configured_attributes(&mut self, root: &Path, git_dir: &Path) -> bool {
        let Ok(config) = sley_config::read_repo_config(git_dir, None) else {
            return false;
        };
        let Some(value) = config.get("core", None, "attributesFile") else {
            return false;
        };
        let path = expand_core_excludes_file(root, value);
        read_attribute_patterns(path, self, &[], value.as_bytes(), false);
        true
    }

    pub(crate) fn read_default_global_attributes(&mut self) {
        if let Some(config_home) = std::env::var_os("XDG_CONFIG_HOME")
            && !config_home.is_empty()
        {
            let path = PathBuf::from(config_home).join("git").join("attributes");
            let source = path.to_string_lossy().into_owned();
            read_attribute_patterns(path, self, &[], source.as_bytes(), false);
            return;
        }
        if let Some(home) = std::env::var_os("HOME") {
            let path = PathBuf::from(home)
                .join(".config")
                .join("git")
                .join("attributes");
            let source = path.to_string_lossy().into_owned();
            read_attribute_patterns(path, self, &[], source.as_bytes(), false);
        }
    }
}

pub(crate) fn read_dir_ignore_patterns_for_base(
    dir: &Path,
    base: &[u8],
    matcher: &mut IgnoreMatcher,
) -> Result<()> {
    let mut source = base.to_vec();
    if !source.is_empty() {
        source.push(b'/');
    }
    source.extend_from_slice(b".gitignore");
    read_per_directory_ignore_patterns_into_matcher(dir.join(".gitignore"), matcher, base, &source)
}

/// Fold `dir`'s `.gitattributes` (if any) into `matcher`, scoped to `dir`'s path
/// within `root`. Used both by the eager full-tree pass and by the status/diff
/// worktree walk as it descends, so the tree is read for attributes exactly once.
pub(crate) fn read_dir_attribute_patterns(
    root: &Path,
    dir: &Path,
    matcher: &mut AttributeMatcher,
) -> Result<()> {
    let relative = dir.strip_prefix(root).map_err(|_| {
        GitError::InvalidPath(format!("path {} is outside worktree", dir.display()))
    })?;
    let base = git_path_bytes(relative)?;
    read_dir_attribute_patterns_for_base(dir, &base, matcher)
}

pub(crate) fn read_dir_attribute_patterns_for_base(
    dir: &Path,
    base: &[u8],
    matcher: &mut AttributeMatcher,
) -> Result<()> {
    let mut source = base.to_vec();
    if !source.is_empty() {
        source.push(b'/');
    }
    source.extend_from_slice(b".gitattributes");
    read_attribute_patterns(dir.join(".gitattributes"), matcher, base, &source, true);
    Ok(())
}

pub(crate) fn collect_attribute_patterns(
    root: &Path,
    dir: &Path,
    matcher: &mut AttributeMatcher,
) -> Result<()> {
    read_dir_attribute_patterns(root, dir, matcher)?;

    let mut entries = fs::read_dir(dir)?.collect::<std::result::Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        if path.file_name().and_then(|name| name.to_str()) == Some(".git") {
            continue;
        }
        if entry.metadata()?.is_dir() {
            collect_attribute_patterns(root, &path, matcher)?;
        }
    }
    Ok(())
}

pub(crate) fn read_attribute_patterns(
    path: impl AsRef<Path>,
    matcher: &mut AttributeMatcher,
    base: &[u8],
    source: &[u8],
    nofollow: bool,
) {
    let path = path.as_ref();
    if nofollow
        && let Ok(metadata) = fs::symlink_metadata(path)
        && metadata.file_type().is_symlink()
    {
        eprintln!(
            "warning: unable to access '{}': Too many levels of symbolic links",
            String::from_utf8_lossy(source)
        );
        return;
    }
    let Ok(contents) = fs::read(path) else {
        return;
    };
    read_attribute_patterns_from_bytes(&contents, matcher, base, source);
}

pub(crate) fn read_attribute_patterns_from_bytes(
    contents: &[u8],
    matcher: &mut AttributeMatcher,
    base: &[u8],
    source: &[u8],
) {
    for (index, raw) in contents.split(|byte| *byte == b'\n').enumerate() {
        if raw.len() >= 2048 {
            eprintln!(
                "warning: ignoring overly long attributes line {}",
                index + 1
            );
            continue;
        }
        push_attribute_pattern(matcher, raw, base, source, index + 1);
    }
}

pub(crate) fn collect_attribute_patterns_from_tree(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    base: Vec<u8>,
    matcher: &mut AttributeMatcher,
) -> Result<()> {
    let object = read_expected_object(db, tree_oid, ObjectType::Tree)?;
    let mut entries = Tree::parse(format, &object.body)?.entries;
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    for entry in &entries {
        if entry.name == b".gitattributes" && tree_entry_object_type(entry.mode) == ObjectType::Blob
        {
            let object = db.read_object(&entry.oid).map_err(|err| {
                expect_missing_object_kind(err, entry.oid, MissingObjectKind::Blob)
            })?;
            if object.object_type == ObjectType::Blob {
                let source = attribute_source_for_base(&base);
                read_attribute_patterns_from_bytes(&object.body, matcher, &base, &source);
            }
        }
    }
    for entry in entries {
        if tree_entry_object_type(entry.mode) != ObjectType::Tree {
            continue;
        }
        let mut child_base = base.clone();
        if !child_base.is_empty() {
            child_base.push(b'/');
        }
        child_base.extend_from_slice(entry.name.as_bytes());
        collect_attribute_patterns_from_tree(db, format, &entry.oid, child_base, matcher)?;
    }
    Ok(())
}

pub(crate) fn collect_attribute_patterns_from_index(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    matcher: &mut AttributeMatcher,
) -> Result<()> {
    let index_path = repository_index_path(git_dir);
    if !index_path.exists() {
        return Ok(());
    }
    // Attribute lookup needs the logical index contents, including
    // `.gitattributes` files represented by collapsed sparse directories. Use
    // an in-memory view: observable index expansion and on-disk mutation would
    // violate `check-attr`'s sparse-index contract.
    let mut index = Index::parse(&fs::read(index_path)?, format)?;
    expand_sparse_index_view(&mut index, db, format)?;
    let mut entries = index.entries;
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    for entry in entries {
        let is_attributes_file =
            entry.path == b".gitattributes" || entry.path.as_bytes().ends_with(b"/.gitattributes");
        if index_entry_stage(&entry) != 0
            || tree_entry_object_type(entry.mode) != ObjectType::Blob
            || !is_attributes_file
        {
            continue;
        }
        let base = match entry.path.as_bytes().strip_suffix(b".gitattributes") {
            Some(b"") => Vec::new(),
            Some(parent) => parent.strip_suffix(b"/").unwrap_or(parent).to_vec(),
            None => continue,
        };
        let object = db
            .read_object(&entry.oid)
            .map_err(|err| expect_missing_object_kind(err, entry.oid, MissingObjectKind::Blob))?;
        if object.object_type == ObjectType::Blob {
            read_attribute_patterns_from_bytes(&object.body, matcher, &base, entry.path.as_bytes());
        }
    }
    Ok(())
}

pub(crate) fn attribute_source_for_base(base: &[u8]) -> Vec<u8> {
    let mut source = base.to_vec();
    if !source.is_empty() {
        source.push(b'/');
    }
    source.extend_from_slice(b".gitattributes");
    source
}

pub(crate) fn push_attribute_pattern(
    matcher: &mut AttributeMatcher,
    raw: &[u8],
    base: &[u8],
    source: &[u8],
    line_number: usize,
) {
    let line = raw.strip_suffix(b"\r").unwrap_or(raw);
    let line = trim_ascii_whitespace(line);
    if line.is_empty() || line.starts_with(b"#") {
        return;
    }
    let Some((raw_pattern, fields)) = split_attribute_line(line) else {
        return;
    };
    if let Some(macro_name) = raw_pattern.strip_prefix(b"[attr]") {
        if macro_name.is_empty() {
            return;
        }
        if is_reserved_attribute_name(macro_name) {
            report_invalid_attribute_name(macro_name, source, line_number);
            return;
        }
        let mut assignments = Vec::new();
        for field in fields {
            push_attribute_assignments(
                &mut assignments,
                &field,
                &matcher.macros,
                source,
                line_number,
            );
        }
        matcher.push_attribute_order(macro_name);
        for assignment in &assignments {
            matcher.push_attribute_order(&assignment.attribute);
        }
        matcher.macros.insert(macro_name.to_vec(), assignments);
        return;
    }
    let mut assignments = Vec::new();
    for field in fields {
        push_attribute_assignments(
            &mut assignments,
            &field,
            &matcher.macros,
            source,
            line_number,
        );
    }
    if assignments.is_empty() {
        return;
    }
    for assignment in &assignments {
        matcher.push_attribute_order(&assignment.attribute);
    }
    if raw_pattern.starts_with(b"!") {
        eprintln!(
            "warning: Negative patterns are ignored in git attributes\nUse '\\!' for literal leading exclamation."
        );
        return;
    }
    let raw_pattern = raw_pattern
        .strip_prefix(br"\!")
        .map(|pattern| {
            let mut literal = Vec::with_capacity(pattern.len() + 1);
            literal.push(b'!');
            literal.extend_from_slice(pattern);
            literal
        })
        .unwrap_or(raw_pattern);
    let (anchored, pattern) = if let Some(pattern) = raw_pattern.strip_prefix(b"/") {
        (true, pattern)
    } else {
        (false, raw_pattern.as_slice())
    };
    if pattern.is_empty() {
        return;
    }
    matcher.patterns.push(AttributePattern {
        base: base.to_vec(),
        pattern: pattern.to_vec(),
        ignore_case_pattern: matcher.ignore_case.then(|| ascii_lowercase(pattern)),
        anchored,
        has_slash: pattern.contains(&b'/'),
        assignments,
    });
}

pub(crate) fn push_attribute_assignments(
    assignments: &mut Vec<AttributeAssignment>,
    field: &[u8],
    macros: &BTreeMap<Vec<u8>, Vec<AttributeAssignment>>,
    source: &[u8],
    line_number: usize,
) {
    if let Some(macro_assignments) = macros.get(field) {
        assignments.push(AttributeAssignment {
            attribute: field.to_vec(),
            state: Some(AttributeState::Set),
        });
        assignments.extend(macro_assignments.iter().cloned());
        return;
    }
    if field == b"binary" {
        assignments.push(AttributeAssignment {
            attribute: b"binary".to_vec(),
            state: Some(AttributeState::Set),
        });
        assignments.push(AttributeAssignment {
            attribute: b"diff".to_vec(),
            state: Some(AttributeState::Unset),
        });
        assignments.push(AttributeAssignment {
            attribute: b"merge".to_vec(),
            state: Some(AttributeState::Unset),
        });
        assignments.push(AttributeAssignment {
            attribute: b"text".to_vec(),
            state: Some(AttributeState::Unset),
        });
        return;
    }
    if let Some(attribute) = field.strip_prefix(b"-") {
        if !attribute.is_empty() {
            if is_reserved_attribute_name(attribute) {
                report_invalid_attribute_name(attribute, source, line_number);
                return;
            }
            assignments.push(AttributeAssignment {
                attribute: attribute.to_vec(),
                state: Some(AttributeState::Unset),
            });
        }
        return;
    }
    if let Some(attribute) = field.strip_prefix(b"!") {
        if !attribute.is_empty() {
            if is_reserved_attribute_name(attribute) {
                report_invalid_attribute_name(attribute, source, line_number);
                return;
            }
            assignments.push(AttributeAssignment {
                attribute: attribute.to_vec(),
                state: None,
            });
        }
        return;
    }
    if let Some(equal) = field.iter().position(|byte| *byte == b'=') {
        let attribute = &field[..equal];
        let value = &field[equal + 1..];
        if !attribute.is_empty() {
            if is_reserved_attribute_name(attribute) {
                report_invalid_attribute_name(attribute, source, line_number);
                return;
            }
            assignments.push(AttributeAssignment {
                attribute: attribute.to_vec(),
                state: Some(AttributeState::Value(value.to_vec())),
            });
        }
        return;
    }
    if is_reserved_attribute_name(field) {
        report_invalid_attribute_name(field, source, line_number);
        return;
    }
    assignments.push(AttributeAssignment {
        attribute: field.to_vec(),
        state: Some(AttributeState::Set),
    });
}

pub(crate) fn split_attribute_line(line: &[u8]) -> Option<(Vec<u8>, Vec<Vec<u8>>)> {
    let mut index = 0;
    while line.get(index).is_some_and(u8::is_ascii_whitespace) {
        index += 1;
    }
    if index == line.len() || line[index] == b'#' {
        return None;
    }
    let pattern = if line[index] == b'"' {
        match c_unquote_prefix(&line[index..]) {
            Some((pattern, consumed)) => {
                index += consumed;
                pattern
            }
            None => {
                let start = index;
                while index < line.len() && !line[index].is_ascii_whitespace() {
                    index += 1;
                }
                line[start..index].to_vec()
            }
        }
    } else {
        let start = index;
        while index < line.len() && !line[index].is_ascii_whitespace() {
            index += 1;
        }
        line[start..index].to_vec()
    };
    let fields = line[index..]
        .split(|byte| byte.is_ascii_whitespace())
        .filter(|field| !field.is_empty())
        .map(Vec::from)
        .collect();
    Some((pattern, fields))
}

pub(crate) fn c_unquote_prefix(input: &[u8]) -> Option<(Vec<u8>, usize)> {
    if input.first() != Some(&b'"') {
        return None;
    }
    let mut out = Vec::new();
    let mut index = 1;
    while index < input.len() {
        match input[index] {
            b'"' => return Some((out, index + 1)),
            b'\\' if index + 1 < input.len() => {
                index += 1;
                let byte = match input[index] {
                    b'a' => 0x07,
                    b'b' => 0x08,
                    b'f' => 0x0c,
                    b'n' => b'\n',
                    b'r' => b'\r',
                    b't' => b'\t',
                    b'v' => 0x0b,
                    other => other,
                };
                out.push(byte);
            }
            byte => out.push(byte),
        }
        index += 1;
    }
    None
}

pub(crate) fn is_reserved_attribute_name(attribute: &[u8]) -> bool {
    attribute.starts_with(b"builtin_")
}

pub(crate) fn report_invalid_attribute_name(attribute: &[u8], source: &[u8], line_number: usize) {
    // Upstream parses each `.gitattributes` into its attr stack exactly once, so
    // a bad name warns once per file. sley re-reads attribute sources per query
    // (e.g. `git add` resolves the staged file's attributes more than once),
    // which would duplicate the warning; suppress exact repeats within the
    // process, keyed on (name, source, line) so distinct files still each warn.
    static REPORTED: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<Vec<u8>>>> =
        std::sync::OnceLock::new();
    let mut key = attribute.to_vec();
    key.push(0);
    key.extend_from_slice(source);
    key.push(0);
    key.extend_from_slice(line_number.to_string().as_bytes());
    if let Ok(mut seen) = REPORTED
        .get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
        .lock()
        && !seen.insert(key)
    {
        return;
    }
    eprintln!(
        "{} is not a valid attribute name: {}:{}",
        String::from_utf8_lossy(attribute),
        String::from_utf8_lossy(source),
        line_number
    );
}

pub(crate) fn attribute_all_rank(
    attribute: &[u8],
    order: &BTreeMap<Vec<u8>, usize>,
) -> (usize, usize, Vec<u8>) {
    let rank = match attribute {
        b"binary" => 0,
        b"diff" => 1,
        b"merge" => 2,
        b"text" => 3,
        b"eol" => 5,
        _ => 4,
    };
    let order = order.get(attribute).copied().unwrap_or(usize::MAX);
    (rank, order, attribute.to_vec())
}

pub(crate) fn trim_ascii_whitespace(mut value: &[u8]) -> &[u8] {
    while value.first().is_some_and(u8::is_ascii_whitespace) {
        value = &value[1..];
    }
    while value.last().is_some_and(u8::is_ascii_whitespace) {
        value = &value[..value.len() - 1];
    }
    value
}

impl AttributePattern {
    fn matches(&self, path: &[u8], ignore_case: bool) -> bool {
        let path = if self.base.is_empty() {
            path
        } else {
            match strip_attribute_base(path, &self.base, ignore_case) {
                Some(rest) => rest,
                None => return false,
            }
        };
        let folded_pattern;
        let folded_path;
        let (pattern_ref, path_ref) = if ignore_case {
            folded_path = ascii_lowercase(path);
            let pattern_ref = if let Some(pattern) = self.ignore_case_pattern.as_deref() {
                pattern
            } else {
                folded_pattern = ascii_lowercase(&self.pattern);
                folded_pattern.as_slice()
            };
            (pattern_ref, folded_path.as_slice())
        } else {
            (self.pattern.as_slice(), path)
        };
        if self.anchored || self.has_slash {
            return wildcard_path_matches(pattern_ref, path_ref);
        }
        path_ref
            .rsplit(|byte| *byte == b'/')
            .next()
            .is_some_and(|basename| wildcard_path_matches(pattern_ref, basename))
    }
}

pub(crate) fn strip_attribute_base<'a>(
    path: &'a [u8],
    base: &[u8],
    ignore_case: bool,
) -> Option<&'a [u8]> {
    if path.len() <= base.len() || path.get(base.len()) != Some(&b'/') {
        return None;
    }
    let prefix = &path[..base.len()];
    let matches = if ignore_case {
        prefix.eq_ignore_ascii_case(base)
    } else {
        prefix == base
    };
    matches.then_some(&path[base.len() + 1..])
}

pub(crate) fn ascii_lowercase(value: &[u8]) -> Vec<u8> {
    value.iter().map(u8::to_ascii_lowercase).collect()
}
