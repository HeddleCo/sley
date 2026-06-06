use sley_config::GitConfig;
use sley_core::{GitError, ObjectFormat, ObjectId, Result};
use sley_formats::CommitGraph;
use sley_index::Index;
use sley_object::{Commit, ObjectType, Tag, Tree};
use sley_odb::{FileObjectDatabase, ObjectPrefixResolution, ObjectReader};
use sley_refs::{FileRefStore, PackedRef, RefTarget};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevisionSpec {
    pub raw: String,
}

impl RevisionSpec {
    pub fn parse(raw: impl Into<String>) -> Result<Self> {
        let raw = raw.into();
        if raw.is_empty() {
            return Err(GitError::InvalidFormat("empty revision spec".into()));
        }
        Ok(Self { raw })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitRecord {
    pub oid: ObjectId,
    pub parents: Vec<ObjectId>,
    pub commit: Commit,
}

/// Lightweight commit-walk record: id, parents, and committer time only.
///
/// Unlike [`CommitRecord`] (which carries the whole parsed [`Commit`] and so
/// forces a read+inflate of every commit object), this is sourced from the
/// commit-graph when present — no object read — and falls back to the commit
/// object only for commits the graph does not cover. Use it for traversals that
/// need ancestry + date ordering but not the full commit (rev-list, log).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitMetadata {
    pub oid: ObjectId,
    pub parents: Vec<ObjectId>,
    /// Committer time in seconds since the epoch (the value the commit-graph
    /// records, identical to the object's committer line).
    pub commit_time: i64,
}

pub fn resolve_revision(
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    rev: &str,
) -> Result<ObjectId> {
    let git_dir = git_dir.as_ref();
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    resolve_revision_with_reader(git_dir, format, &db, rev)
}

pub fn resolve_revision_with_reader<R: ObjectReader>(
    git_dir: &Path,
    format: ObjectFormat,
    reader: &R,
    rev: &str,
) -> Result<ObjectId> {
    // `:/text` and `:[N:]path` are anchored at the start of the spec; handle them
    // before the `^`/`~` suffix machinery so the leading colon is not mistaken
    // for a normal revision name.
    if let Some(text) = rev.strip_prefix(":/") {
        return search_commit_message_all(git_dir, format, reader, text);
    }
    if let Some(rest) = rev.strip_prefix(':') {
        let (stage, path) = parse_index_stage_path(rest);
        return resolve_index_path(git_dir, format, stage, path);
    }
    // `<rev>:<path>` resolves to the object at `<path>` within `<rev>`'s tree. The
    // colon binds looser than the `^`/`~` navigation suffixes, so an unsuffixed
    // colon here means the whole left side is the revision-ish to peel to a tree.
    if let Some((rev_part, path)) = split_rev_path(rev) {
        return resolve_rev_path(git_dir, format, reader, rev_part, path);
    }
    // `@`, `@{N}`, `<branch>@{N}`, `@{u}`/`@{upstream}`, `@{push}`, and `@{-N}` are
    // resolved before the `^`/`~` suffix machinery so that a base like `HEAD@{1}^`
    // first becomes the reflog value and only then has the parent suffix applied
    // (the suffix splitter recurses back into this function on the `@{...}` base).
    if let Some(oid) = resolve_at_selector(git_dir, format, rev)? {
        return Ok(oid);
    }
    if let Some((base, suffix)) = split_revision_suffix(rev)? {
        if base.is_empty() {
            return Err(GitError::InvalidFormat(format!(
                "revision {rev} has empty base"
            )));
        }
        let base_oid = resolve_revision_with_reader(git_dir, format, reader, base)?;
        return apply_revision_suffix(git_dir, reader, format, &base_oid, suffix, rev);
    }
    resolve_revision_name(git_dir, format, rev)
}

pub struct RevisionResolver<'a, R> {
    git_dir: &'a Path,
    format: ObjectFormat,
    reader: &'a R,
}

impl<'a, R: ObjectReader> RevisionResolver<'a, R> {
    pub fn new(git_dir: &'a Path, format: ObjectFormat, reader: &'a R) -> Self {
        Self {
            git_dir,
            format,
            reader,
        }
    }

    pub fn resolve(&self, rev: &str) -> Result<ObjectId> {
        resolve_revision_with_reader(self.git_dir, self.format, self.reader, rev)
    }

    pub fn peel_to_blob(&self, rev: &str) -> Result<ObjectId> {
        let oid = self.resolve(rev)?;
        peel_tags(self.reader, self.format, &oid)
    }

    pub fn peel_to_tree(&self, rev: &str) -> Result<ObjectId> {
        let oid = self.resolve(rev)?;
        peel_to_tree(self.reader, self.format, &oid)
    }

    pub fn peel_to_commit(&self, rev: &str) -> Result<ObjectId> {
        let oid = self.resolve(rev)?;
        peel_to_commit(self.reader, self.format, &oid)
    }

    pub fn resolve_path(&self, rev: &str, path: &str) -> Result<ResolvedTreePath> {
        resolve_rev_path_entry(self.git_dir, self.format, self.reader, rev, path)
    }
}

fn resolve_revision_name(
    git_dir: &Path,
    format: sley_core::ObjectFormat,
    rev: &str,
) -> Result<ObjectId> {
    if rev.len() == format.hex_len() && rev.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return ObjectId::from_hex(format, rev);
    }
    let refs = FileRefStore::new(git_dir.to_path_buf(), format);
    if let Some(oid) = resolve_revision_ref(&refs, rev)? {
        return Ok(oid);
    }
    if rev.len() >= 4
        && rev.len() < format.hex_len()
        && rev.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        let db = FileObjectDatabase::from_git_dir(git_dir, format);
        match db.resolve_prefix(rev)? {
            ObjectPrefixResolution::Unique(oid) => return Ok(oid),
            ObjectPrefixResolution::Ambiguous(matches) => {
                let mut names = matches
                    .into_iter()
                    .map(|oid| oid.to_string())
                    .collect::<Vec<_>>();
                names.sort();
                return Err(GitError::InvalidObjectId(format!(
                    "short object ID {rev} is ambiguous: {}",
                    names.join(", ")
                )));
            }
            ObjectPrefixResolution::Missing => {}
        }
    }
    Err(GitError::NotFound(format!("revision {rev}")))
}

fn resolve_revision_ref(refs: &FileRefStore, rev: &str) -> Result<Option<ObjectId>> {
    let target = if rev == "HEAD" {
        refs.read_ref("HEAD")?
    } else if rev.starts_with("refs/") {
        refs.read_ref(rev)?
    } else {
        refs.read_ref(&format!("refs/heads/{rev}"))?
            .or(refs.read_ref(&format!("refs/tags/{rev}"))?)
    };
    match target {
        Some(RefTarget::Direct(oid)) => Ok(Some(oid)),
        Some(RefTarget::Symbolic(name)) => match refs.read_ref(&name)? {
            Some(RefTarget::Direct(oid)) => Ok(Some(oid)),
            _ => Ok(None),
        },
        None => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// `@`, `@{...}`, and `<branch>@{...}` selectors
// ---------------------------------------------------------------------------
//
// These are git's "at-mark" revision selectors:
//   * bare `@`                  -> HEAD
//   * `@{N}` / `<branch>@{N}`   -> the N-th prior value from the reflog
//   * `@{u}` / `@{upstream}`    -> the branch's configured upstream tracking ref
//   * `@{push}`                 -> the branch's push tracking ref
//   * `@{-N}`                   -> the N-th previously checked-out branch
// They are parsed ahead of the `^`/`~`/`:` suffix machinery so a base like
// `HEAD@{1}^` resolves the reflog value first and then applies the suffix.

/// Try to resolve `rev` as an at-mark selector.
///
/// Returns `Ok(None)` when `rev` is not an at-mark form (so the caller falls
/// through to the normal suffix/name handling), `Ok(Some(oid))` on a successful
/// resolution, and an error for a malformed or unsupported selector.
fn resolve_at_selector(
    git_dir: &Path,
    format: sley_core::ObjectFormat,
    rev: &str,
) -> Result<Option<ObjectId>> {
    // Bare `@` is an alias for HEAD.
    if rev == "@" {
        let refs = FileRefStore::new(git_dir.to_path_buf(), format);
        return match resolve_revision_ref(&refs, "HEAD")? {
            Some(oid) => Ok(Some(oid)),
            None => Err(GitError::NotFound("revision @".into())),
        };
    }

    // Everything else must be `<base>@{<selector>}` with the braces at the end.
    let Some(open) = rev.find("@{") else {
        return Ok(None);
    };
    let Some(inner) = rev.strip_suffix('}') else {
        return Ok(None);
    };
    // `inner` still has the `<base>@{` prefix; keep only what is inside the braces.
    let inner = &inner[open + 2..];
    let base = &rev[..open];

    // `@{-N}` is special: it names a previously checked-out branch and ignores
    // any `<base>` to its left (git only accepts a bare `@{-N}`).
    if let Some(rest) = inner.strip_prefix('-') {
        if !base.is_empty() {
            return Err(GitError::InvalidFormat(format!(
                "invalid revision selector {rev}"
            )));
        }
        let count = parse_at_count(rev, rest)?;
        return Ok(Some(resolve_previous_checkout(
            git_dir, format, count, rev,
        )?));
    }

    if inner == "u" || inner == "upstream" {
        return Ok(Some(resolve_upstream(git_dir, format, base, false, rev)?));
    }
    if inner == "push" {
        return Ok(Some(resolve_upstream(git_dir, format, base, true, rev)?));
    }
    if inner.bytes().all(|byte| byte.is_ascii_digit()) {
        let count = parse_at_count(rev, inner)?;
        return Ok(Some(resolve_reflog_nth(git_dir, format, base, count, rev)?));
    }

    // Date-based selectors such as `@{yesterday}` / `@{2 days ago}` are not
    // implemented; report them rather than silently mis-resolving.
    Err(GitError::Unsupported(format!(
        "revision selector @{{{inner}}}"
    )))
}

/// Parse the numeric portion of an `@{N}` / `@{-N}` selector.
fn parse_at_count(rev: &str, text: &str) -> Result<usize> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(GitError::InvalidFormat(format!(
            "invalid revision selector {rev}"
        )));
    }
    text.parse::<usize>()
        .map_err(|_| GitError::InvalidFormat(format!("invalid revision selector {rev}")))
}

/// Map a `<base>@{...}` base to the full ref name whose reflog should be read.
///
/// An empty base means `HEAD`; `refs/...` is used verbatim; anything else is
/// treated as a branch short-name under `refs/heads/`.
fn reflog_ref_name(base: &str) -> String {
    if base.is_empty() || base == "HEAD" {
        "HEAD".to_string()
    } else if base.starts_with("refs/") {
        base.to_string()
    } else {
        format!("refs/heads/{base}")
    }
}

/// Resolve `<base>@{N}` to the N-th prior value of `base` from its reflog.
///
/// The reflog is stored oldest-first, so `@{0}` is the most recent entry's new
/// value and `@{N}` is the new value of the entry `N` positions earlier (which
/// equals the old value recorded `N` moves ago). A reflog that is too short to
/// satisfy `N` reports a git-style "log for '<base>' only has K entries" error.
fn resolve_reflog_nth(
    git_dir: &Path,
    format: sley_core::ObjectFormat,
    base: &str,
    n: usize,
    rev: &str,
) -> Result<ObjectId> {
    let ref_name = reflog_ref_name(base);
    let refs = FileRefStore::new(git_dir.to_path_buf(), format);
    let entries = refs.read_reflog(&ref_name)?;
    if entries.is_empty() {
        return Err(GitError::NotFound(format!(
            "no reflog for '{}' to resolve {rev}",
            reflog_display_name(base)
        )));
    }
    // `@{N}` counts back from the newest entry; index `len - 1 - n`.
    let len = entries.len();
    if n >= len {
        return Err(GitError::NotFound(format!(
            "log for '{}' only has {len} entries",
            reflog_display_name(base)
        )));
    }
    Ok(entries[len - 1 - n].new_oid.clone())
}

/// Human-facing name for a reflog target in error messages (HEAD, or the branch
/// short name without the `refs/heads/` prefix, matching git's wording).
fn reflog_display_name(base: &str) -> String {
    if base.is_empty() {
        "HEAD".to_string()
    } else {
        base.to_string()
    }
}

/// Resolve `@{-N}` to the tip of the N-th previously checked-out branch.
///
/// HEAD's reflog is scanned newest-first for "checkout: moving from X to Y"
/// entries; the N-th such entry's `X` (the branch we moved *away* from) is the
/// answer, which is then resolved to its current tip.
fn resolve_previous_checkout(
    git_dir: &Path,
    format: sley_core::ObjectFormat,
    n: usize,
    rev: &str,
) -> Result<ObjectId> {
    if n == 0 {
        return Err(GitError::InvalidFormat(format!(
            "invalid revision selector {rev}"
        )));
    }
    let refs = FileRefStore::new(git_dir.to_path_buf(), format);
    let entries = refs.read_reflog("HEAD")?;
    let mut seen = 0usize;
    for entry in entries.iter().rev() {
        let Some(from) = checkout_move_source(&entry.message) else {
            continue;
        };
        seen += 1;
        if seen == n {
            let from = from.to_string();
            return resolve_revision_name(git_dir, format, &from).map_err(|_| {
                GitError::NotFound(format!(
                    "could not resolve previous branch '{from}' for {rev}"
                ))
            });
        }
    }
    Err(GitError::NotFound(format!(
        "not enough previous checkouts to resolve {rev}"
    )))
}

/// Extract the source branch `X` from a HEAD reflog message of the form
/// "checkout: moving from X to Y", or `None` for any other reflog message.
fn checkout_move_source(message: &[u8]) -> Option<&str> {
    let message = std::str::from_utf8(message).ok()?;
    let rest = message.strip_prefix("checkout: moving from ")?;
    // The remainder is "X to Y"; split on the last " to " so a branch named
    // with embedded " to " still parses (git itself uses the final separator).
    let (from, _to) = rest.rsplit_once(" to ")?;
    Some(from)
}

/// Resolve `<base>@{u}` / `@{upstream}` (when `push` is false) or `@{push}`
/// (when `push` is true) to the configured tracking ref's current value.
///
/// The branch is `base` (or the current branch when `base` is empty). The
/// tracking ref is built from `branch.<name>.remote` (or `pushRemote` for the
/// push form) plus the short name from `branch.<name>.merge`, yielding
/// `refs/remotes/<remote>/<short>`. `@{push}` falls back to the upstream remote
/// when no push-specific remote is configured.
fn resolve_upstream(
    git_dir: &Path,
    format: sley_core::ObjectFormat,
    base: &str,
    push: bool,
    rev: &str,
) -> Result<ObjectId> {
    let refs = FileRefStore::new(git_dir.to_path_buf(), format);
    let branch = if base.is_empty() {
        refs.current_branch()?.ok_or_else(|| {
            GitError::InvalidFormat(format!("HEAD is not a branch, cannot resolve {rev}"))
        })?
    } else if let Some(short) = base.strip_prefix("refs/heads/") {
        short.to_string()
    } else if base.starts_with("refs/") {
        return Err(GitError::InvalidFormat(format!(
            "{base} is not a branch, cannot resolve {rev}"
        )));
    } else {
        base.to_string()
    };

    let config = read_repo_config(git_dir)?;
    let merge = config
        .get("branch", Some(&branch), "merge")
        .ok_or_else(|| {
            GitError::NotFound(format!("no upstream configured for branch '{branch}'"))
        })?;
    let short = merge.strip_prefix("refs/heads/").unwrap_or(merge);

    // For `@{push}` prefer a push-specific remote, falling back to the upstream
    // remote (`branch.<name>.remote`) when none is set.
    let remote = if push {
        config
            .get("branch", Some(&branch), "pushRemote")
            .or_else(|| config.get("remote", None, "pushDefault"))
            .or_else(|| config.get("branch", Some(&branch), "remote"))
    } else {
        config.get("branch", Some(&branch), "remote")
    }
    .ok_or_else(|| GitError::NotFound(format!("no upstream remote for branch '{branch}'")))?;

    let tracking = format!("refs/remotes/{remote}/{short}");
    match resolve_revision_ref(&refs, &tracking)? {
        Some(oid) => Ok(oid),
        None => Err(GitError::NotFound(format!(
            "upstream tracking ref '{tracking}' for {rev} is missing"
        ))),
    }
}

/// Read the repository config (`<git_dir>/config`).
///
/// A missing config file is treated as empty rather than an error, mirroring how
/// upstream resolution behaves in a freshly created repository with no branch
/// configuration.
fn read_repo_config(git_dir: &Path) -> Result<GitConfig> {
    let path = git_dir.join("config");
    match fs::read(&path) {
        Ok(bytes) => GitConfig::parse(&bytes),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(GitConfig::default()),
        Err(err) => Err(GitError::Io(err.to_string())),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RevisionSuffix<'a> {
    Parent(usize),
    FirstParent(usize),
    Peel(PeelKind),
    /// `<rev>^{/text}` — first matching commit in `<rev>`'s first-parent ancestry.
    Search(&'a str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PeelKind {
    AnyNonTag,
    Object,
    Commit,
    Tree,
    Tag,
}

fn split_revision_suffix(rev: &str) -> Result<Option<(&str, RevisionSuffix<'_>)>> {
    let caret = rev.rfind('^');
    let tilde = rev.rfind('~');
    let Some((op, pos)) = (match (caret, tilde) {
        (Some(caret), Some(tilde)) if caret > tilde => Some(('^', caret)),
        (Some(caret), Some(tilde)) if tilde > caret => Some(('~', tilde)),
        (Some(caret), None) => Some(('^', caret)),
        (None, Some(tilde)) => Some(('~', tilde)),
        (None, None) => None,
        _ => None,
    }) else {
        return Ok(None);
    };
    let (base, suffix) = rev.split_at(pos);
    let suffix = &suffix[1..];
    match op {
        '^' => {
            if let Some(text) = parse_search_suffix(rev, suffix)? {
                return Ok(Some((base, RevisionSuffix::Search(text))));
            }
            let parent = if suffix.is_empty() {
                1
            } else if let Some(kind) = parse_peel_suffix(rev, suffix)? {
                return Ok(Some((base, RevisionSuffix::Peel(kind))));
            } else if suffix.bytes().all(|byte| byte.is_ascii_digit()) {
                parse_revision_count(rev, suffix)?
            } else {
                return Ok(None);
            };
            Ok(Some((base, RevisionSuffix::Parent(parent))))
        }
        '~' => {
            let count = if suffix.is_empty() {
                1
            } else if suffix.bytes().all(|byte| byte.is_ascii_digit()) {
                parse_revision_count(rev, suffix)?
            } else {
                return Ok(None);
            };
            Ok(Some((base, RevisionSuffix::FirstParent(count))))
        }
        _ => Ok(None),
    }
}

fn parse_peel_suffix(rev: &str, suffix: &str) -> Result<Option<PeelKind>> {
    if !suffix.starts_with('{') {
        return Ok(None);
    }
    let Some(kind) = suffix
        .strip_prefix('{')
        .and_then(|value| value.strip_suffix('}'))
    else {
        return Err(GitError::InvalidFormat(format!(
            "invalid revision peel suffix in {rev}"
        )));
    };
    let kind = match kind {
        "" => PeelKind::AnyNonTag,
        "object" => PeelKind::Object,
        "commit" => PeelKind::Commit,
        "tree" => PeelKind::Tree,
        "tag" => PeelKind::Tag,
        other => {
            return Err(GitError::Unsupported(format!(
                "revision peel suffix ^{{{other}}}"
            )));
        }
    };
    Ok(Some(kind))
}

fn parse_search_suffix<'a>(rev: &str, suffix: &'a str) -> Result<Option<&'a str>> {
    let Some(inner) = suffix.strip_prefix("{/") else {
        return Ok(None);
    };
    let Some(text) = inner.strip_suffix('}') else {
        return Err(GitError::InvalidFormat(format!(
            "invalid revision search suffix in {rev}"
        )));
    };
    Ok(Some(text))
}

fn parse_revision_count(rev: &str, text: &str) -> Result<usize> {
    text.parse::<usize>()
        .map_err(|_| GitError::InvalidFormat(format!("invalid revision suffix in {rev}")))
}

fn apply_revision_suffix<R: ObjectReader>(
    git_dir: &Path,
    reader: &R,
    format: sley_core::ObjectFormat,
    base: &ObjectId,
    suffix: RevisionSuffix<'_>,
    raw_rev: &str,
) -> Result<ObjectId> {
    match suffix {
        RevisionSuffix::Parent(parent) => {
            if parent == 0 {
                return Err(GitError::InvalidFormat(format!(
                    "invalid zero parent in {raw_rev}"
                )));
            }
            let mut graph = CommitGraphContext::load(git_dir, format);
            graph
                .commit_parents(reader, base)?
                .get(parent - 1)
                .cloned()
                .ok_or_else(|| GitError::NotFound(format!("parent {parent} of {base}")))
        }
        RevisionSuffix::FirstParent(count) => {
            let mut graph = CommitGraphContext::load(git_dir, format);
            let mut current = base.clone();
            for _ in 0..count {
                current = graph
                    .commit_parents(reader, &current)?
                    .first()
                    .cloned()
                    .ok_or_else(|| GitError::NotFound(format!("first parent of {current}")))?;
            }
            Ok(current)
        }
        RevisionSuffix::Peel(kind) => peel_revision(reader, format, base, kind),
        RevisionSuffix::Search(text) => {
            search_commit_message_first_parent(git_dir, reader, format, base, text)
        }
    }
}

// ---------------------------------------------------------------------------
// Commit-graph acceleration
// ---------------------------------------------------------------------------
//
// History walks (ancestry for `A..B`/`A...B`, `merge_bases`, `is_ancestor`, the
// `^`/`~` navigation suffixes, and `^{/text}` first-parent search) read a
// commit's parents, commit date, and generation number from the commit-graph
// when one is present, avoiding a read+inflate of every commit object from the
// odb. The graph is loaded once per walk (lazily, on first lookup) and lookups
// are keyed by oid. Any commit absent from the graph -- or the absence of a
// graph entirely -- falls back to reading the commit object, so results are
// always identical to the object-only walk.
//
// Generation numbers (topological "height", where a commit's generation is one
// greater than the maximum of its parents') let merge-base and ancestor queries
// prune branches that cannot contribute: an ancestor's generation is strictly
// smaller than its descendant's, so a candidate whose generation is already
// below a target can never reach that target and its parents need not be
// visited. A graph written without generation numbers stores generation 0 for
// every commit (GENERATION_NUMBER_ZERO); pruning is disabled in that case to
// stay correct.

/// Generation number used by git when a commit-graph has no usable generation
/// data; treated as "unknown" so it never drives pruning.
const GENERATION_NUMBER_ZERO: u32 = 0;

/// Commit metadata resolved from the commit-graph: parents (already mapped from
/// graph indices to object ids), generation number, and committer date.
#[derive(Debug, Clone)]
struct GraphCommit {
    parents: Vec<ObjectId>,
    generation: u32,
    commit_time: u64,
}

/// A walk's view of the commit-graph.
///
/// Construction is cheap and infallible (`load` only records the git dir and
/// object format); the graph file is read and parsed on the first lookup and
/// cached for the remainder of the walk. Lookups return resolved [`GraphCommit`]
/// metadata keyed by oid, or `None` when the commit is not represented (so the
/// caller falls back to the odb). If the graph file is missing, empty, or fails
/// to parse, the context degrades to "no graph" and every lookup misses, which
/// keeps walk results identical to the pure object-reading path.
struct CommitGraphContext<'a> {
    git_dir: &'a Path,
    format: sley_core::ObjectFormat,
    /// `None` until the first lookup forces a load; afterwards `Some(map)` where
    /// the map is empty iff no usable graph exists.
    commits: Option<HashMap<ObjectId, GraphCommit>>,
}

impl<'a> CommitGraphContext<'a> {
    fn load(git_dir: &'a Path, format: sley_core::ObjectFormat) -> Self {
        Self {
            git_dir,
            format,
            commits: None,
        }
    }

    /// Resolve `oid`'s graph metadata, loading and parsing the graph on first
    /// use. Returns `None` when the commit is not in the graph.
    fn lookup(&mut self, oid: &ObjectId) -> Option<&GraphCommit> {
        if self.commits.is_none() {
            self.commits = Some(load_commit_graph_map(self.git_dir, self.format));
        }
        self.commits.as_ref().and_then(|map| map.get(oid))
    }

    /// Parents of `oid` from the graph, or `None` when it is not present.
    fn parents(&mut self, oid: &ObjectId) -> Option<Vec<ObjectId>> {
        self.lookup(oid).map(|commit| commit.parents.clone())
    }

    /// Generation number of `oid`, or `None` when it is not present in the graph
    /// or the graph carries no generation numbers (generation 0). A `None`
    /// result disables generation-based pruning for that commit.
    fn generation(&mut self, oid: &ObjectId) -> Option<u32> {
        match self.lookup(oid) {
            Some(commit) if commit.generation != GENERATION_NUMBER_ZERO => Some(commit.generation),
            _ => None,
        }
    }

    /// Committer date (seconds since the epoch) recorded for `oid` in the graph,
    /// or `None` when the commit is not present. Used to order candidates
    /// without re-parsing the commit object's committer line.
    fn commit_time(&mut self, oid: &ObjectId) -> Option<i64> {
        self.lookup(oid)
            .map(|commit| i64::try_from(commit.commit_time).unwrap_or(i64::MAX))
    }

    /// Parents of `oid`: from the graph when present, otherwise read+parsed from
    /// the commit object via `reader`.
    fn commit_parents<R: ObjectReader>(
        &mut self,
        reader: &R,
        oid: &ObjectId,
    ) -> Result<Vec<ObjectId>> {
        if let Some(parents) = self.parents(oid) {
            return Ok(parents);
        }
        commit_parents(reader, self.format, oid)
    }

    /// `oid`'s parents and committer time from the graph in one lookup, or `None`
    /// when the commit is not represented (the caller then reads the object).
    fn metadata(&mut self, oid: &ObjectId) -> Option<(Vec<ObjectId>, i64)> {
        self.lookup(oid).map(|commit| {
            (
                commit.parents.clone(),
                i64::try_from(commit.commit_time).unwrap_or(i64::MAX),
            )
        })
    }
}

/// Read and parse the commit-graph for `git_dir`, returning an oid-keyed map of
/// commit metadata with parent indices resolved to object ids.
///
/// A missing graph, an unparseable graph, or a graph with internally
/// inconsistent parent indices all yield an empty map; callers then fall back to
/// reading commit objects, so a damaged or unsupported graph can never change a
/// walk's result, only its speed. Both the monolithic
/// `objects/info/commit-graph` file and a split-graph chain under
/// `objects/info/commit-graphs/` are honored; chain layers are merged into a
/// single map, and any layer that cannot be parsed standalone (e.g. one whose
/// parent edges cross into a base layer, which this reader does not resolve)
/// causes the chain to be ignored in favor of the object-reading path.
fn load_commit_graph_map(
    git_dir: &Path,
    format: sley_core::ObjectFormat,
) -> HashMap<ObjectId, GraphCommit> {
    let info = git_dir.join("objects").join("info");
    let single = info.join("commit-graph");
    if single.exists() {
        // A read/parse failure degrades to "no graph" (empty map) so callers
        // fall back to object reads; correctness never depends on the graph.
        return fs::read(&single)
            .map_err(|err| GitError::Io(err.to_string()))
            .and_then(|bytes| CommitGraph::parse(&bytes, format))
            .and_then(|graph| graph_to_map(&graph))
            .unwrap_or_default();
    }

    let chain = info.join("commit-graphs").join("commit-graph-chain");
    load_commit_graph_chain(&info, &chain, format).unwrap_or_default()
}

/// Load every layer named in a split-graph chain file and merge them.
///
/// The chain file lists one layer hash per line, base layers first. Each layer
/// lives at `commit-graphs/graph-<hash>.graph`. Layers are merged tip-last so a
/// commit rewritten in a newer layer wins; any layer that fails to parse
/// standalone aborts the whole chain (returning an error that the caller turns
/// into "no graph").
fn load_commit_graph_chain(
    info: &Path,
    chain: &Path,
    format: sley_core::ObjectFormat,
) -> Result<HashMap<ObjectId, GraphCommit>> {
    let contents = match fs::read_to_string(chain) {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(HashMap::new());
        }
        Err(err) => return Err(GitError::Io(err.to_string())),
    };
    let mut merged: HashMap<ObjectId, GraphCommit> = HashMap::new();
    for line in contents.lines() {
        let hash = line.trim();
        if hash.is_empty() {
            continue;
        }
        let layer = info
            .join("commit-graphs")
            .join(format!("graph-{hash}.graph"));
        let bytes = fs::read(&layer).map_err(|err| GitError::Io(err.to_string()))?;
        let graph = CommitGraph::parse(&bytes, format)?;
        for (oid, commit) in graph_to_map(&graph)? {
            merged.insert(oid, commit);
        }
    }
    Ok(merged)
}

/// Turn a parsed [`CommitGraph`] into an oid-keyed metadata map, resolving each
/// entry's parent indices into the parents' object ids.
fn graph_to_map(graph: &CommitGraph) -> Result<HashMap<ObjectId, GraphCommit>> {
    let mut map = HashMap::with_capacity(graph.commits.len());
    for entry in &graph.commits {
        let mut parents = Vec::with_capacity(entry.parents.len());
        for parent in &entry.parents {
            let parent = usize::try_from(*parent).map_err(|_| {
                GitError::InvalidFormat("commit-graph parent index overflow".into())
            })?;
            let parent_entry = graph.commits.get(parent).ok_or_else(|| {
                GitError::InvalidFormat("commit-graph parent points past commit table".into())
            })?;
            parents.push(parent_entry.oid.clone());
        }
        map.insert(
            entry.oid.clone(),
            GraphCommit {
                parents,
                generation: entry.generation,
                commit_time: entry.commit_time,
            },
        );
    }
    Ok(map)
}

fn commit_parents<R: ObjectReader>(
    reader: &R,
    format: sley_core::ObjectFormat,
    oid: &ObjectId,
) -> Result<Vec<ObjectId>> {
    let object = reader.read_object(oid)?;
    if object.object_type != ObjectType::Commit {
        return Err(GitError::InvalidObject(format!(
            "expected commit {oid}, found {}",
            object.object_type.as_str()
        )));
    }
    Ok(Commit::parse_ref(format, &object.body)?.parents)
}

fn peel_revision<R: ObjectReader>(
    reader: &R,
    format: sley_core::ObjectFormat,
    oid: &ObjectId,
    kind: PeelKind,
) -> Result<ObjectId> {
    match kind {
        PeelKind::AnyNonTag => peel_tags(reader, format, oid),
        PeelKind::Object => {
            reader.read_object(oid)?;
            Ok(oid.clone())
        }
        PeelKind::Commit => peel_to_commit(reader, format, oid),
        PeelKind::Tree => peel_to_tree(reader, format, oid),
        PeelKind::Tag => {
            let object = reader.read_object(oid)?;
            if object.object_type == ObjectType::Tag {
                Ok(oid.clone())
            } else {
                Err(GitError::InvalidObject(format!(
                    "expected tag {oid}, found {}",
                    object.object_type.as_str()
                )))
            }
        }
    }
}

pub fn peel_tags<R: ObjectReader>(
    reader: &R,
    format: sley_core::ObjectFormat,
    oid: &ObjectId,
) -> Result<ObjectId> {
    let object = reader.read_object(oid)?;
    if object.object_type != ObjectType::Tag {
        return Ok(oid.clone());
    }
    let tag = Tag::parse_ref(format, &object.body)?;
    peel_tags(reader, format, &tag.object)
}

pub fn peel_to_tree<R: ObjectReader>(
    reader: &R,
    format: sley_core::ObjectFormat,
    oid: &ObjectId,
) -> Result<ObjectId> {
    let object = reader.read_object(oid)?;
    match object.object_type {
        ObjectType::Tree => Ok(oid.clone()),
        ObjectType::Commit => Ok(Commit::parse_ref(format, &object.body)?.tree),
        ObjectType::Tag => {
            let tag = Tag::parse_ref(format, &object.body)?;
            peel_to_tree(reader, format, &tag.object)
        }
        other => Err(GitError::InvalidObject(format!(
            "expected tree-ish {oid}, found {}",
            other.as_str()
        ))),
    }
}

pub fn peel_to_commit<R: ObjectReader>(
    reader: &R,
    format: sley_core::ObjectFormat,
    oid: &ObjectId,
) -> Result<ObjectId> {
    let object = reader.read_object(oid)?;
    match object.object_type {
        ObjectType::Commit => Ok(oid.clone()),
        ObjectType::Tag => {
            let tag = Tag::parse_ref(format, &object.body)?;
            peel_to_commit(reader, format, &tag.object)
        }
        other => Err(GitError::InvalidObject(format!(
            "expected commit-ish {oid}, found {}",
            other.as_str()
        ))),
    }
}

pub fn pack_refs_with_auto_peel(
    git_dir: impl AsRef<Path>,
    format: sley_core::ObjectFormat,
    prune_loose: bool,
) -> Result<Vec<PackedRef>> {
    let git_dir = git_dir.as_ref();
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let refs = FileRefStore::new(git_dir, format);
    refs.pack_refs_with_peeler(prune_loose, |_, oid| {
        let peeled = peel_tags(&db, format, oid)?;
        if &peeled == oid {
            Ok(None)
        } else {
            Ok(Some(peeled))
        }
    })
}

pub fn parse_commit_parents(format: sley_core::ObjectFormat, body: &[u8]) -> Result<Vec<ObjectId>> {
    let text = std::str::from_utf8(body).map_err(|err| GitError::InvalidObject(err.to_string()))?;
    let mut parents = Vec::new();
    for line in text.lines() {
        if line.is_empty() {
            break;
        }
        if let Some(hex) = line.strip_prefix("parent ") {
            parents.push(ObjectId::from_hex(format, hex)?);
        }
    }
    Ok(parents)
}

/// Walk history from `starts`, returning [`CommitMetadata`] (id + parents +
/// committer time) for every reachable commit, in discovery order.
///
/// Parents and time come from the commit-graph when it covers a commit (no object
/// read); commits the graph omits fall back to a read+parse. This is the
/// commit-graph-accelerated counterpart of [`walk_commits`] for callers that only
/// need ancestry and ordering (rev-list, log traversal) and not the full commit.
pub fn walk_commit_metadata<R: ObjectReader>(
    git_dir: &Path,
    format: sley_core::ObjectFormat,
    reader: &R,
    starts: impl IntoIterator<Item = ObjectId>,
    first_parent: bool,
) -> Result<Vec<CommitMetadata>> {
    let mut graph = CommitGraphContext::load(git_dir, format);
    let mut seen = HashSet::new();
    let mut pending: VecDeque<ObjectId> = starts.into_iter().collect();
    let mut out = Vec::new();
    while let Some(oid) = pending.pop_front() {
        if !seen.insert(oid.clone()) {
            continue;
        }
        let (parents, commit_time) = match graph.metadata(&oid) {
            Some(metadata) => metadata,
            None => commit_metadata_from_object(reader, format, &oid)?,
        };
        // `--first-parent` follows only the first parent of each commit; otherwise
        // every parent is enqueued (matching `walk_commits`).
        if first_parent {
            pending.extend(parents.first().cloned());
        } else {
            pending.extend(parents.iter().cloned());
        }
        out.push(CommitMetadata {
            oid,
            parents,
            commit_time,
        });
    }
    Ok(out)
}

/// Parents and committer time of `oid` read from its commit object (the fallback
/// for commits absent from the commit-graph).
fn commit_metadata_from_object<R: ObjectReader>(
    reader: &R,
    format: sley_core::ObjectFormat,
    oid: &ObjectId,
) -> Result<(Vec<ObjectId>, i64)> {
    let object = reader.read_object(oid)?;
    if object.object_type != ObjectType::Commit {
        return Err(GitError::InvalidObject(format!(
            "expected commit {oid}, found {}",
            object.object_type.as_str()
        )));
    }
    let commit = Commit::parse_ref(format, &object.body)?;
    let commit_time = commit
        .committer_signature()
        .map(|signature| signature.time.seconds)
        .unwrap_or(0);
    Ok((commit.parents, commit_time))
}

pub fn walk_commits<R: ObjectReader>(
    reader: &R,
    format: sley_core::ObjectFormat,
    starts: impl IntoIterator<Item = ObjectId>,
) -> Result<Vec<CommitRecord>> {
    let mut seen = HashSet::new();
    let mut pending: VecDeque<ObjectId> = starts.into_iter().collect();
    let mut out = Vec::new();
    while let Some(oid) = pending.pop_front() {
        if !seen.insert(oid.clone()) {
            continue;
        }
        let object = reader.read_object(&oid)?;
        if object.object_type != ObjectType::Commit {
            return Err(GitError::InvalidObject(format!(
                "expected commit {oid}, found {}",
                object.object_type.as_str()
            )));
        }
        let commit = Commit::parse(format, &object.body)?;
        let parents = commit.parents.clone();
        pending.extend(parents.iter().cloned());
        out.push(CommitRecord {
            oid,
            parents,
            commit,
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// `<rev>:<path>` resolution
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTreePath {
    pub oid: ObjectId,
    pub mode: Option<u32>,
    pub object_type: ObjectType,
    pub name: Vec<u8>,
}

/// Resolve `<rev>:<path>` to the object id of `<path>` within `<rev>`'s tree.
///
/// `rev` is peeled to a tree (so a commit, tag, or tree id all work) and then
/// `path` is walked component by component. The result is the blob id for a
/// file path or the subtree id for a directory path; an empty `path` resolves
/// to the tree itself. Missing components and attempts to descend through a
/// non-tree entry both report a git-style "path '<path>' does not exist in
/// '<rev>'" error.
pub fn resolve_rev_path<R: ObjectReader>(
    git_dir: &Path,
    format: sley_core::ObjectFormat,
    reader: &R,
    rev: &str,
    path: &str,
) -> Result<ObjectId> {
    resolve_rev_path_entry(git_dir, format, reader, rev, path).map(|entry| entry.oid)
}

pub fn resolve_rev_path_entry<R: ObjectReader>(
    git_dir: &Path,
    format: ObjectFormat,
    reader: &R,
    rev: &str,
    path: &str,
) -> Result<ResolvedTreePath> {
    let rev_oid = resolve_revision_with_reader(git_dir, format, reader, rev)?;
    let tree_oid = peel_to_tree(reader, format, &rev_oid)?;
    resolve_tree_path_entry(reader, format, &tree_oid, path)
        .ok_or_else(|| GitError::NotFound(format!("path '{path}' does not exist in '{rev}'")))
}

/// Walk `path` within the tree `tree_oid`, returning the id of the entry it
/// names, or `None` if any component is missing or a component before the last
/// is not a tree. An empty `path` returns `tree_oid` unchanged.
pub fn resolve_tree_path_entry<R: ObjectReader>(
    reader: &R,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    path: &str,
) -> Option<ResolvedTreePath> {
    let mut current = tree_oid.clone();
    // Split on '/', skipping empty components so leading/trailing/duplicate
    // separators ("a//b", "/a", "dir/") behave the way git's pathspec does.
    let components: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    if components.is_empty() {
        return Some(ResolvedTreePath {
            oid: current,
            mode: None,
            object_type: ObjectType::Tree,
            name: Vec::new(),
        });
    }
    let last = components.len() - 1;
    for (idx, component) in components.iter().enumerate() {
        let object = reader.read_object(&current).ok()?;
        if object.object_type != ObjectType::Tree {
            // Cannot descend through a blob (or anything non-tree).
            return None;
        }
        let tree = Tree::parse(format, &object.body).ok()?;
        let entry = tree
            .entries
            .iter()
            .find(|entry| entry.name == component.as_bytes())?;
        let object_type = sley_object::tree_entry_object_type(entry.mode);
        if idx == last {
            return Some(ResolvedTreePath {
                oid: entry.oid.clone(),
                mode: Some(entry.mode),
                object_type,
                name: entry.name.clone(),
            });
        }
        // Intermediate component must itself be a tree to keep descending.
        if object_type != ObjectType::Tree {
            return None;
        }
        current = entry.oid.clone();
    }
    None
}

/// Split `<rev>:<path>` into its revision and path halves.
///
/// Returns `None` when the spec is not a rev/path form, i.e. when there is no
/// colon, when the colon is part of a leading `:` index spec (handled
/// elsewhere), or when the left side is empty. The split uses the first colon
/// so paths may themselves contain colons.
pub fn split_rev_path_spec(rev: &str) -> Option<(&str, &str)> {
    split_rev_path(rev)
}

fn split_rev_path(rev: &str) -> Option<(&str, &str)> {
    let colon = rev.find(':')?;
    if colon == 0 {
        return None;
    }
    Some((&rev[..colon], &rev[colon + 1..]))
}

// ---------------------------------------------------------------------------
// `:[N:]<path>` index-stage resolution
// ---------------------------------------------------------------------------

/// Parse the portion after a leading `:` into `(stage, path)`.
///
/// `:<path>` selects stage 0; `:N:<path>` (N in 0..=3) selects stage N. When
/// the leading token is not a single 0-3 digit followed by a colon the whole
/// string is treated as a stage-0 path.
fn parse_index_stage_path(rest: &str) -> (u8, &str) {
    let bytes = rest.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && matches!(bytes[0], b'0'..=b'3') {
        return (bytes[0] - b'0', &rest[2..]);
    }
    (0, rest)
}

/// Resolve `path` at `stage` in the on-disk index, returning the recorded blob
/// id. Reports git-style errors for a missing index, a path absent from the
/// index, and a path present only at other stages.
fn resolve_index_path(
    git_dir: &Path,
    format: sley_core::ObjectFormat,
    stage: u8,
    path: &str,
) -> Result<ObjectId> {
    let index_path = repository_index_path(git_dir);
    let bytes = match fs::read(&index_path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(GitError::NotFound(format!(
                "path '{path}' is not in the index"
            )));
        }
        Err(err) => return Err(GitError::Io(err.to_string())),
    };
    let index = Index::parse(&bytes, format)?;
    let mut path_exists = false;
    for entry in &index.entries {
        if entry.path != path.as_bytes() {
            continue;
        }
        path_exists = true;
        if index_entry_stage(entry) == stage {
            return Ok(entry.oid.clone());
        }
    }
    if path_exists {
        Err(GitError::NotFound(format!(
            "path '{path}' is in the index, but not at stage {stage}"
        )))
    } else {
        Err(GitError::NotFound(format!(
            "path '{path}' is not in the index"
        )))
    }
}

/// Extract the merge stage (0-3) from an index entry's flags (bits 12-13).
fn index_entry_stage(entry: &sley_index::IndexEntry) -> u8 {
    ((entry.flags >> 12) & 0x3) as u8
}

/// Locate the index file, honoring `GIT_INDEX_FILE` like the rest of git.
fn repository_index_path(git_dir: &Path) -> PathBuf {
    std::env::var_os("GIT_INDEX_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| git_dir.join("index"))
}

// ---------------------------------------------------------------------------
// Commit-message search (`:/text` and `<rev>^{/text}`)
// ---------------------------------------------------------------------------
//
// Matching is a plain substring test against the raw commit message; this crate
// has no regex dependency, so `:/text` and `^{/text}` find commits whose
// message *contains* `text` rather than matching it as a POSIX regular
// expression. An empty pattern matches the most recent candidate, mirroring
// git's "return the youngest commit" behavior for `:/`.

/// `:/text` — newest commit (across all refs) whose message contains `text`.
///
/// "Newest" is approximated by committer timestamp, falling back to the order
/// commits are discovered when timestamps are unavailable, which matches git's
/// observable behavior for the common case. The committer date is taken from the
/// commit-graph when available (it equals the value on the object's committer
/// line, so the chosen commit is unchanged) and parsed from the commit body
/// otherwise.
fn search_commit_message_all<R: ObjectReader>(
    git_dir: &Path,
    format: sley_core::ObjectFormat,
    reader: &R,
    text: &str,
) -> Result<ObjectId> {
    let starts = all_ref_commit_starts(git_dir, format, reader)?;
    let mut graph = CommitGraphContext::load(git_dir, format);
    let mut seen = HashSet::new();
    let mut pending: VecDeque<ObjectId> = starts.into_iter().collect();
    let mut best: Option<(i64, ObjectId)> = None;
    while let Some(oid) = pending.pop_front() {
        if !seen.insert(oid.clone()) {
            continue;
        }
        let object = reader.read_object(&oid)?;
        if object.object_type != ObjectType::Commit {
            return Err(GitError::InvalidObject(format!(
                "expected commit {oid}, found {}",
                object.object_type.as_str()
            )));
        }
        let commit = Commit::parse_ref(format, &object.body)?;
        pending.extend(commit.parents.iter().cloned());
        if commit_message_contains(commit.message, text) {
            let when = graph
                .commit_time(&oid)
                .or_else(|| commit_committer_time(commit.committer))
                .unwrap_or(i64::MIN);
            if best
                .as_ref()
                .is_none_or(|(best_when, _)| when >= *best_when)
            {
                best = Some((when, oid));
            }
        }
    }
    best.map(|(_, oid)| oid)
        .ok_or_else(|| GitError::NotFound(format!("no commit matching ':/{text}'")))
}

/// `<rev>^{/text}` — first commit reachable from `base` along the first-parent
/// chain whose message contains `text`.
fn search_commit_message_first_parent<R: ObjectReader>(
    git_dir: &Path,
    reader: &R,
    format: sley_core::ObjectFormat,
    base: &ObjectId,
    text: &str,
) -> Result<ObjectId> {
    let start = peel_to_commit(reader, format, base)?;
    // Commit *messages* are not stored in the commit-graph, so each candidate's
    // body is still read; the graph is only consulted to follow the first-parent
    // edge, avoiding a second parse of the same object for the linkage.
    let mut graph = CommitGraphContext::load(git_dir, format);
    let mut current = Some(start);
    let mut seen = HashSet::new();
    while let Some(oid) = current {
        if !seen.insert(oid.clone()) {
            break;
        }
        let object = reader.read_object(&oid)?;
        if object.object_type != ObjectType::Commit {
            return Err(GitError::InvalidObject(format!(
                "expected commit {oid}, found {}",
                object.object_type.as_str()
            )));
        }
        let commit = Commit::parse_ref(format, &object.body)?;
        if commit_message_contains(commit.message, text) {
            return Ok(oid);
        }
        current = graph
            .parents(&oid)
            .unwrap_or_else(|| commit.parents.clone())
            .into_iter()
            .next();
    }
    Err(GitError::NotFound(format!(
        "no commit matching '^{{/{text}}}' in first-parent history"
    )))
}

fn commit_message_contains(message: &[u8], text: &str) -> bool {
    if text.is_empty() {
        return true;
    }
    // Search the raw bytes so non-UTF-8 messages still match where possible.
    message
        .windows(text.len())
        .any(|window| window == text.as_bytes())
}

/// Best-effort committer timestamp (seconds since epoch) from a commit's
/// committer line, used only to order `:/text` candidates.
fn commit_committer_time(committer: &[u8]) -> Option<i64> {
    let line = std::str::from_utf8(committer).ok()?;
    // Format: "Name <email> <seconds> <tz>"; the timestamp is the
    // second-to-last whitespace-separated field.
    let mut fields = line.rsplit(' ');
    let _tz = fields.next()?;
    fields.next()?.parse::<i64>().ok()
}

/// Collect commit starting points from every ref (peeling tags to commits) for
/// a repository-wide `:/text` search.
fn all_ref_commit_starts<R: ObjectReader>(
    git_dir: &Path,
    format: sley_core::ObjectFormat,
    reader: &R,
) -> Result<Vec<ObjectId>> {
    let refs = FileRefStore::new(git_dir.to_path_buf(), format);
    let mut starts = Vec::new();
    let mut seen = HashSet::new();
    for reference in refs.list_refs()? {
        let oid = match reference.target {
            RefTarget::Direct(oid) => oid,
            RefTarget::Symbolic(_) => continue,
        };
        // Skip refs whose objects (or tag targets) are not present/commit-ish.
        let Ok(commit) = peel_to_commit(reader, format, &oid) else {
            continue;
        };
        if seen.insert(commit.clone()) {
            starts.push(commit);
        }
    }
    Ok(starts)
}

// ---------------------------------------------------------------------------
// Revision ranges (`A..B` and `A...B`)
// ---------------------------------------------------------------------------

/// A parsed revision range expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RevisionRange {
    /// `A..B` — commits reachable from `B` but not from `A`.
    Asymmetric { start: String, end: String },
    /// `A...B` — commits reachable from exactly one of `A`/`B` (symmetric
    /// difference).
    Symmetric { left: String, right: String },
}

/// Parse `A..B` / `A...B` range syntax.
///
/// Returns `None` when `spec` is not a range. An omitted side defaults to
/// `HEAD` (so `..B`, `A..`, `...B`, etc. behave like git). `...` is checked
/// before `..` so the symmetric form is not misread as an asymmetric one. A
/// trailing `..`/`...` with the wrong number of dots (more than two/three) is
/// rejected as a malformed range.
pub fn parse_revision_range(spec: &str) -> Option<RevisionRange> {
    if let Some((left, right)) = spec.split_once("...") {
        if left.contains("..") || right.contains("..") {
            return None;
        }
        return Some(RevisionRange::Symmetric {
            left: default_range_side(left).to_string(),
            right: default_range_side(right).to_string(),
        });
    }
    if let Some((left, right)) = spec.split_once("..") {
        if left.contains("..") || right.contains("..") {
            return None;
        }
        return Some(RevisionRange::Asymmetric {
            start: default_range_side(left).to_string(),
            end: default_range_side(right).to_string(),
        });
    }
    None
}

fn default_range_side(side: &str) -> &str {
    if side.is_empty() { "HEAD" } else { side }
}

/// A small builder for rev-list-style revision selection arguments.
///
/// Specs added through [`RevisionSelection::add_spec`] understand bare includes
/// (`B`), caret excludes (`^A`), asymmetric ranges (`A..B`), symmetric ranges
/// (`A...B`), and the `HEAD` defaults accepted by [`parse_revision_range`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RevisionSelection {
    items: Vec<RevisionSelectionItem>,
}

/// One item in a [`RevisionSelection`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RevisionSelectionItem {
    /// Include commits reachable from this revision.
    Include(String),
    /// Exclude commits reachable from this revision.
    Exclude(String),
    /// Include/exclude according to a parsed `A..B` or `A...B` range.
    Range(RevisionRange),
}

/// Resolved commit starts plus the full set of excluded commits.
///
/// `excluded` contains the complete ancestry closure of each exclude tip (and
/// symmetric-range merge base), so callers can walk from `starts` and filter any
/// commit whose oid is present here.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ResolvedRevisionSelection {
    pub starts: Vec<ObjectId>,
    pub excluded: HashSet<ObjectId>,
}

impl RevisionSelection {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_specs<I, S>(specs: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut selection = Self::new();
        for spec in specs {
            selection.add_spec(spec.as_ref())?;
        }
        Ok(selection)
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn items(&self) -> &[RevisionSelectionItem] {
        &self.items
    }

    pub fn add_spec(&mut self, spec: impl AsRef<str>) -> Result<&mut Self> {
        let spec = spec.as_ref();
        if spec.is_empty() {
            return Err(GitError::InvalidFormat("empty revision spec".into()));
        }
        if let Some(rev) = spec.strip_prefix('^') {
            if rev.is_empty() {
                return Err(GitError::InvalidFormat("empty exclude revision".into()));
            }
            return self.exclude(rev.to_string());
        }
        if let Some(range) = parse_revision_range(spec) {
            self.range(range);
            return Ok(self);
        }
        self.include(spec.to_string())
    }

    pub fn include(&mut self, rev: impl Into<String>) -> Result<&mut Self> {
        let rev = RevisionSpec::parse(rev)?.raw;
        self.items.push(RevisionSelectionItem::Include(rev));
        Ok(self)
    }

    pub fn exclude(&mut self, rev: impl Into<String>) -> Result<&mut Self> {
        let rev = RevisionSpec::parse(rev)?.raw;
        self.items.push(RevisionSelectionItem::Exclude(rev));
        Ok(self)
    }

    pub fn range(&mut self, range: RevisionRange) -> &mut Self {
        self.items.push(RevisionSelectionItem::Range(range));
        self
    }

    pub fn resolve<R: ObjectReader>(
        &self,
        git_dir: &Path,
        format: sley_core::ObjectFormat,
        reader: &R,
    ) -> Result<ResolvedRevisionSelection> {
        let mut resolved = ResolvedRevisionSelection::default();
        for item in &self.items {
            match item {
                RevisionSelectionItem::Include(rev) => {
                    resolved
                        .starts
                        .push(resolve_range_endpoint(git_dir, format, reader, rev)?);
                }
                RevisionSelectionItem::Exclude(rev) => {
                    let oid = resolve_range_endpoint(git_dir, format, reader, rev)?;
                    extend_excluded_ancestors(
                        git_dir,
                        format,
                        reader,
                        &mut resolved.excluded,
                        &oid,
                    )?;
                }
                RevisionSelectionItem::Range(range) => {
                    resolve_selection_range(git_dir, format, reader, range, &mut resolved)?;
                }
            }
        }
        Ok(resolved)
    }
}

impl ResolvedRevisionSelection {
    /// Walk from the resolved starts and return selected commit ids after
    /// applying the excluded set.
    pub fn selected_commit_oids<R: ObjectReader>(
        &self,
        git_dir: &Path,
        format: sley_core::ObjectFormat,
        reader: &R,
        first_parent: bool,
    ) -> Result<Vec<ObjectId>> {
        let mut graph = CommitGraphContext::load(git_dir, format);
        let mut seen = HashSet::new();
        let mut pending: VecDeque<ObjectId> = self.starts.clone().into();
        let mut out = Vec::new();
        while let Some(oid) = pending.pop_front() {
            if !seen.insert(oid.clone()) || self.excluded.contains(&oid) {
                continue;
            }
            let (parents, _) = match graph.metadata(&oid) {
                Some(metadata) => metadata,
                None => commit_metadata_from_object(reader, format, &oid)?,
            };
            if first_parent {
                pending.extend(parents.first().cloned());
            } else {
                pending.extend(parents);
            }
            out.push(oid);
        }
        Ok(out)
    }
}

/// Resolve a parsed range to the set of commit oids it selects.
///
/// `A..B` yields commits reachable from `B` but not `A`; `A...B` yields the
/// symmetric difference (reachable from `A` or `B` but not both). Endpoints are
/// resolved as revisions and peeled to commits before traversal. The returned
/// vector is unordered.
pub fn resolve_revision_range<R: ObjectReader>(
    git_dir: &Path,
    format: sley_core::ObjectFormat,
    reader: &R,
    range: &RevisionRange,
) -> Result<Vec<ObjectId>> {
    match range {
        RevisionRange::Asymmetric { start, end } => {
            let start_oid = resolve_range_endpoint(git_dir, format, reader, start)?;
            let end_oid = resolve_range_endpoint(git_dir, format, reader, end)?;
            let excluded = ancestor_set(git_dir, reader, format, &start_oid)?;
            let included = ancestor_set(git_dir, reader, format, &end_oid)?;
            Ok(included
                .into_iter()
                .filter(|oid| !excluded.contains(oid))
                .collect())
        }
        RevisionRange::Symmetric { left, right } => {
            let left_oid = resolve_range_endpoint(git_dir, format, reader, left)?;
            let right_oid = resolve_range_endpoint(git_dir, format, reader, right)?;
            let left_set = ancestor_set(git_dir, reader, format, &left_oid)?;
            let right_set = ancestor_set(git_dir, reader, format, &right_oid)?;
            let mut out = Vec::new();
            for oid in &left_set {
                if !right_set.contains(oid) {
                    out.push(oid.clone());
                }
            }
            for oid in &right_set {
                if !left_set.contains(oid) {
                    out.push(oid.clone());
                }
            }
            Ok(out)
        }
    }
}

fn resolve_selection_range<R: ObjectReader>(
    git_dir: &Path,
    format: sley_core::ObjectFormat,
    reader: &R,
    range: &RevisionRange,
    resolved: &mut ResolvedRevisionSelection,
) -> Result<()> {
    match range {
        RevisionRange::Asymmetric { start, end } => {
            let start_oid = resolve_range_endpoint(git_dir, format, reader, start)?;
            let end_oid = resolve_range_endpoint(git_dir, format, reader, end)?;
            extend_excluded_ancestors(git_dir, format, reader, &mut resolved.excluded, &start_oid)?;
            resolved.starts.push(end_oid);
        }
        RevisionRange::Symmetric { left, right } => {
            let left_oid = resolve_range_endpoint(git_dir, format, reader, left)?;
            let right_oid = resolve_range_endpoint(git_dir, format, reader, right)?;
            resolved.starts.push(left_oid.clone());
            resolved.starts.push(right_oid.clone());
            for base in merge_bases(git_dir, format, reader, &left_oid, &right_oid)? {
                extend_excluded_ancestors(git_dir, format, reader, &mut resolved.excluded, &base)?;
            }
        }
    }
    Ok(())
}

fn resolve_range_endpoint<R: ObjectReader>(
    git_dir: &Path,
    format: sley_core::ObjectFormat,
    reader: &R,
    rev: &str,
) -> Result<ObjectId> {
    let oid = resolve_revision_with_reader(git_dir, format, reader, rev)?;
    peel_to_commit(reader, format, &oid)
}

fn extend_excluded_ancestors<R: ObjectReader>(
    git_dir: &Path,
    format: sley_core::ObjectFormat,
    reader: &R,
    excluded: &mut HashSet<ObjectId>,
    start: &ObjectId,
) -> Result<()> {
    excluded.extend(ancestor_set(git_dir, reader, format, start)?);
    Ok(())
}

/// Compute the set of commits reachable from `start` (inclusive) following all
/// parent edges. Uses the commit-graph for parent lookups when available.
fn ancestor_set<R: ObjectReader>(
    git_dir: &Path,
    reader: &R,
    format: sley_core::ObjectFormat,
    start: &ObjectId,
) -> Result<HashSet<ObjectId>> {
    let mut graph = CommitGraphContext::load(git_dir, format);
    ancestor_set_with_graph(&mut graph, reader, start)
}

/// Reachability set of `start` (inclusive) over all parent edges, using a
/// pre-loaded graph context for parent lookups. A full reachability query
/// admits no generation-based pruning -- every reachable commit is part of the
/// answer -- so this is a plain BFS that simply reads parents from the graph
/// when available.
fn ancestor_set_with_graph<R: ObjectReader>(
    graph: &mut CommitGraphContext<'_>,
    reader: &R,
    start: &ObjectId,
) -> Result<HashSet<ObjectId>> {
    let mut seen = HashSet::new();
    let mut pending = VecDeque::from([start.clone()]);
    while let Some(oid) = pending.pop_front() {
        if !seen.insert(oid.clone()) {
            continue;
        }
        for parent in graph.commit_parents(reader, &oid)? {
            pending.push_back(parent);
        }
    }
    Ok(seen)
}

/// Determine whether `ancestor` is reachable from `descendant` via parent
/// edges (an ancestor check). A commit is considered its own ancestor.
pub fn is_ancestor<R: ObjectReader>(
    git_dir: &Path,
    format: sley_core::ObjectFormat,
    reader: &R,
    ancestor: &ObjectId,
    descendant: &ObjectId,
) -> Result<bool> {
    if ancestor == descendant {
        return Ok(true);
    }
    let mut graph = CommitGraphContext::load(git_dir, format);

    // Generation-based shortcut: a commit's generation is strictly greater than
    // any of its ancestors', so if `ancestor` sits at or above `descendant` in
    // the generation order it cannot be a (proper) ancestor of it. This only
    // fires when both generations are known; otherwise we fall through to the
    // walk. (`min_generation` doubles as the pruning floor below.)
    let min_generation = graph.generation(ancestor);
    if let (Some(anc_gen), Some(desc_gen)) = (min_generation, graph.generation(descendant))
        && anc_gen >= desc_gen
    {
        return Ok(false);
    }

    let mut seen = HashSet::new();
    let mut pending = VecDeque::from([descendant.clone()]);
    while let Some(oid) = pending.pop_front() {
        if !seen.insert(oid.clone()) {
            continue;
        }
        // Prune: if `oid`'s generation is below `ancestor`'s, then `oid` and all
        // of its own ancestors have a generation strictly smaller than
        // `ancestor`'s, so none of them can be `ancestor`. Stop descending here.
        // Only applies when both generations are known.
        if let (Some(floor), Some(here)) = (min_generation, graph.generation(&oid))
            && here < floor
        {
            continue;
        }
        for parent in graph.commit_parents(reader, &oid)? {
            if &parent == ancestor {
                return Ok(true);
            }
            pending.push_back(parent);
        }
    }
    Ok(false)
}

/// Compute the merge bases (best common ancestors) of two commits, mirroring
/// the generation-free history walk used elsewhere in the project. Self-contained
/// so callers do not need the CLI's merge-base machinery.
pub fn merge_bases<R: ObjectReader>(
    git_dir: &Path,
    format: sley_core::ObjectFormat,
    reader: &R,
    left: &ObjectId,
    right: &ObjectId,
) -> Result<Vec<ObjectId>> {
    // One graph context is shared by both ancestry walks so the commit-graph is
    // read and parsed at most once for the whole merge-base computation; parents
    // and commit dates come from the graph when present and fall back to object
    // reads otherwise. The depth-based lowest-common-ancestor reduction below is
    // left unchanged so the selected bases are bit-for-bit identical to the
    // pure object-reading walk.
    let mut graph = CommitGraphContext::load(git_dir, format);
    let left_depths = ancestor_depths_with_graph(&mut graph, reader, left)?;
    let right_depths = ancestor_depths_with_graph(&mut graph, reader, right)?;
    let candidates: Vec<ObjectId> = left_depths
        .keys()
        .filter(|oid| right_depths.contains_key(*oid))
        .cloned()
        .collect();
    // Keep only the lowest common ancestors: drop any candidate that has another
    // candidate strictly closer to *both* endpoints (i.e. a descendant common
    // ancestor).
    let mut bases: Vec<ObjectId> = candidates
        .iter()
        .filter(|candidate| {
            !candidates.iter().any(|other| {
                other != *candidate
                    && depth_lt(&left_depths, other, candidate)
                    && depth_lt(&right_depths, other, candidate)
            })
        })
        .cloned()
        .collect();
    bases.sort_by_key(|oid| oid.to_hex());
    Ok(bases)
}

fn depth_lt(depths: &HashMap<ObjectId, usize>, a: &ObjectId, b: &ObjectId) -> bool {
    match (depths.get(a), depths.get(b)) {
        (Some(a_depth), Some(b_depth)) => a_depth < b_depth,
        _ => false,
    }
}

/// BFS the ancestry of `start`, recording the shortest distance to each commit,
/// using a pre-loaded graph context for parent lookups so several walks can
/// share one parsed commit-graph. The traversal is an unpruned BFS by design:
/// the recorded depths feed the merge-base lowest-common-ancestor reduction,
/// which depends on every reachable commit's shortest distance, so dropping
/// nodes would change the result.
fn ancestor_depths_with_graph<R: ObjectReader>(
    graph: &mut CommitGraphContext<'_>,
    reader: &R,
    start: &ObjectId,
) -> Result<HashMap<ObjectId, usize>> {
    let mut depths = HashMap::new();
    let mut pending = VecDeque::from([(start.clone(), 0usize)]);
    while let Some((oid, depth)) = pending.pop_front() {
        if depths.get(&oid).is_some_and(|existing| *existing <= depth) {
            continue;
        }
        depths.insert(oid.clone(), depth);
        for parent in graph.commit_parents(reader, &oid)? {
            pending.push_back((parent, depth + 1));
        }
    }
    Ok(depths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sley_core::ObjectFormat;
    use sley_object::EncodedObject;
    use sley_odb::{ObjectDatabase, ObjectWriter};
    use sley_refs::{RefTarget, RefUpdate, ReflogEntry};
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn resolve_revision_reads_symbolic_head_and_tags() {
        let git_dir = temp_git_dir();
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .unwrap();
        fs::write(git_dir.join("HEAD"), b"ref: refs/heads/main\n").unwrap();
        let refs = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let mut tx = refs.transaction();
        tx.update(RefUpdate {
            name: "refs/heads/main".into(),
            expected: None,
            new: RefTarget::Direct(oid.clone()),
            reflog: None,
        });
        tx.update(RefUpdate {
            name: "refs/tags/v1.0".into(),
            expected: None,
            new: RefTarget::Direct(oid.clone()),
            reflog: None,
        });
        tx.commit().unwrap();
        assert_eq!(
            resolve_revision(&git_dir, ObjectFormat::Sha1, "HEAD").unwrap(),
            oid
        );
        assert_eq!(
            resolve_revision(&git_dir, ObjectFormat::Sha1, "v1.0").unwrap(),
            oid
        );
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn resolve_revision_supports_parent_suffixes() {
        let git_dir = temp_git_dir();
        let mut db = ObjectDatabase::new(ObjectFormat::Sha1);
        let tree = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "4b825dc642cb6eb9a060e54bf8d69288fbee4904",
        )
        .unwrap();
        let base = write_test_commit(&mut db, tree.clone(), Vec::new(), b"base\n");
        let first_parent = write_test_commit(&mut db, tree.clone(), vec![base.clone()], b"main\n");
        let second_parent = write_test_commit(&mut db, tree.clone(), vec![base.clone()], b"side\n");
        let merge = write_test_commit(
            &mut db,
            tree,
            vec![first_parent.clone(), second_parent.clone()],
            b"merge\n",
        );
        assert_eq!(
            resolve_revision_with_reader(&git_dir, ObjectFormat::Sha1, &db, &format!("{merge}^"))
                .unwrap(),
            first_parent
        );
        assert_eq!(
            resolve_revision_with_reader(&git_dir, ObjectFormat::Sha1, &db, &format!("{merge}^2"))
                .unwrap(),
            second_parent
        );
        assert_eq!(
            resolve_revision_with_reader(&git_dir, ObjectFormat::Sha1, &db, &format!("{merge}~2"))
                .unwrap(),
            base
        );
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn resolve_revision_supports_abbreviated_loose_object_ids() {
        let git_dir = temp_git_dir();
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let oid = db
            .write_object(EncodedObject::new(ObjectType::Blob, b"abbrev\n".to_vec()))
            .unwrap();

        assert_eq!(
            resolve_revision(&git_dir, ObjectFormat::Sha1, &oid.to_hex()[..8]).unwrap(),
            oid
        );
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn resolve_revision_prefers_ref_over_abbreviated_object_id() {
        let git_dir = temp_git_dir();
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let object = db
            .write_object(EncodedObject::new(
                ObjectType::Blob,
                b"abbrev conflict\n".to_vec(),
            ))
            .unwrap();
        let target = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .unwrap();
        let refs = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let mut tx = refs.transaction();
        tx.update(RefUpdate {
            name: format!("refs/heads/{}", &object.to_hex()[..4]),
            expected: None,
            new: RefTarget::Direct(target.clone()),
            reflog: None,
        });
        tx.commit().unwrap();

        assert_eq!(
            resolve_revision(&git_dir, ObjectFormat::Sha1, &object.to_hex()[..4]).unwrap(),
            target
        );
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn resolve_revision_uses_commit_graph_for_parent_suffixes() {
        let git_dir = temp_git_dir();
        fs::create_dir_all(git_dir.join("objects").join("info")).unwrap();
        let parent = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .unwrap();
        let child = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "2222222222222222222222222222222222222222",
        )
        .unwrap();
        fs::write(git_dir.join("HEAD"), format!("{child}\n")).unwrap();
        fs::write(
            git_dir.join("objects").join("info").join("commit-graph"),
            test_commit_graph(ObjectFormat::Sha1, &parent, &child),
        )
        .unwrap();

        struct MissingReader;
        impl ObjectReader for MissingReader {
            fn read_object(&self, oid: &ObjectId) -> Result<EncodedObject> {
                Err(GitError::NotFound(format!(
                    "object reader should not be used for {oid}"
                )))
            }
        }

        assert_eq!(
            resolve_revision_with_reader(&git_dir, ObjectFormat::Sha1, &MissingReader, "HEAD^",)
                .unwrap(),
            parent
        );
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn peel_to_tree_handles_commits_and_tags() {
        let mut db = ObjectDatabase::new(ObjectFormat::Sha1);
        let tree = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "4b825dc642cb6eb9a060e54bf8d69288fbee4904",
        )
        .unwrap();
        db.write_object(EncodedObject::new(ObjectType::Tree, Vec::new()))
            .unwrap();
        let commit = write_test_commit(&mut db, tree.clone(), Vec::new(), b"base\n");
        let tag = Tag {
            object: commit.clone(),
            object_type: ObjectType::Commit,
            name: b"v1.0".to_vec(),
            tagger: Some(b"Example User <example@example.invalid> 0 +0000".to_vec()),
            message: b"release\n".to_vec(),
        };
        let tag = db
            .write_object(EncodedObject::new(ObjectType::Tag, tag.write()))
            .unwrap();
        assert_eq!(
            peel_to_tree(&db, ObjectFormat::Sha1, &commit).unwrap(),
            tree
        );
        assert_eq!(peel_to_tree(&db, ObjectFormat::Sha1, &tag).unwrap(), tree);
    }

    #[test]
    fn peel_to_commit_handles_annotated_tags() {
        let mut db = ObjectDatabase::new(ObjectFormat::Sha1);
        let tree = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "4b825dc642cb6eb9a060e54bf8d69288fbee4904",
        )
        .unwrap();
        db.write_object(EncodedObject::new(ObjectType::Tree, Vec::new()))
            .unwrap();
        let commit = write_test_commit(&mut db, tree, Vec::new(), b"base\n");
        let tag = Tag {
            object: commit.clone(),
            object_type: ObjectType::Commit,
            name: b"v1.0".to_vec(),
            tagger: Some(b"Example User <example@example.invalid> 0 +0000".to_vec()),
            message: b"release\n".to_vec(),
        };
        let tag = db
            .write_object(EncodedObject::new(ObjectType::Tag, tag.write()))
            .unwrap();
        assert_eq!(
            peel_to_commit(&db, ObjectFormat::Sha1, &tag).unwrap(),
            commit
        );
    }

    #[test]
    fn resolve_revision_supports_peel_suffixes() {
        let git_dir = temp_git_dir();
        let mut db = ObjectDatabase::new(ObjectFormat::Sha1);
        let tree = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "4b825dc642cb6eb9a060e54bf8d69288fbee4904",
        )
        .unwrap();
        db.write_object(EncodedObject::new(ObjectType::Tree, Vec::new()))
            .unwrap();
        let commit = write_test_commit(&mut db, tree.clone(), Vec::new(), b"base\n");
        let tag = Tag {
            object: commit.clone(),
            object_type: ObjectType::Commit,
            name: b"v1.0".to_vec(),
            tagger: Some(b"Example User <example@example.invalid> 0 +0000".to_vec()),
            message: b"release\n".to_vec(),
        };
        let tag = db
            .write_object(EncodedObject::new(ObjectType::Tag, tag.write()))
            .unwrap();
        assert_eq!(
            resolve_revision_with_reader(&git_dir, ObjectFormat::Sha1, &db, &format!("{tag}^{{}}"))
                .unwrap(),
            commit
        );
        assert_eq!(
            resolve_revision_with_reader(
                &git_dir,
                ObjectFormat::Sha1,
                &db,
                &format!("{tag}^{{commit}}")
            )
            .unwrap(),
            commit
        );
        assert_eq!(
            resolve_revision_with_reader(
                &git_dir,
                ObjectFormat::Sha1,
                &db,
                &format!("{tag}^{{tree}}")
            )
            .unwrap(),
            tree
        );
        assert_eq!(
            resolve_revision_with_reader(
                &git_dir,
                ObjectFormat::Sha1,
                &db,
                &format!("{tag}^{{tag}}")
            )
            .unwrap(),
            tag
        );
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn pack_refs_with_auto_peel_writes_peeled_tag() {
        let git_dir = temp_git_dir();
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let tree = db
            .write_object(EncodedObject::new(ObjectType::Tree, Vec::new()))
            .unwrap();
        let commit = Commit {
            tree,
            parents: Vec::new(),
            author: b"Example User <example@example.invalid> 0 +0000".to_vec(),
            committer: b"Example User <example@example.invalid> 0 +0000".to_vec(),
            encoding: None,
            message: b"base\n".to_vec(),
        };
        let commit = db
            .write_object(EncodedObject::new(ObjectType::Commit, commit.write()))
            .unwrap();
        let tag = Tag {
            object: commit.clone(),
            object_type: ObjectType::Commit,
            name: b"v1.0".to_vec(),
            tagger: Some(b"Example User <example@example.invalid> 0 +0000".to_vec()),
            message: b"release\n".to_vec(),
        };
        let tag = db
            .write_object(EncodedObject::new(ObjectType::Tag, tag.write()))
            .unwrap();
        let refs = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let mut tx = refs.transaction();
        tx.update(RefUpdate {
            name: "refs/tags/v1.0".into(),
            expected: None,
            new: RefTarget::Direct(tag.clone()),
            reflog: None,
        });
        tx.commit().unwrap();

        let packed = pack_refs_with_auto_peel(&git_dir, ObjectFormat::Sha1, true).unwrap();
        let packed_tag = packed
            .iter()
            .find(|packed| packed.reference.name == "refs/tags/v1.0")
            .unwrap();
        assert_eq!(packed_tag.peeled, Some(commit.clone()));
        assert_eq!(
            refs.read_ref("refs/tags/v1.0").unwrap(),
            Some(RefTarget::Direct(tag))
        );
        assert!(!git_dir.join("refs").join("tags").join("v1.0").exists());
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn resolve_rev_path_finds_nested_blob_and_subtree() {
        let git_dir = temp_git_dir();
        let mut db = ObjectDatabase::new(ObjectFormat::Sha1);
        let blob = db
            .write_object(EncodedObject::new(ObjectType::Blob, b"hello\n".to_vec()))
            .unwrap();
        let sub = write_tree(&mut db, &[(0o100644, b"file.txt", &blob)]);
        let dir = write_tree(&mut db, &[(0o040000, b"sub", &sub)]);
        let root = write_tree(&mut db, &[(0o040000, b"dir", &dir)]);
        let commit = write_test_commit(&mut db, root.clone(), Vec::new(), b"init\n");

        // Nested blob via `<rev>:<path>`.
        assert_eq!(
            resolve_rev_path(
                &git_dir,
                ObjectFormat::Sha1,
                &db,
                &commit.to_hex(),
                "dir/sub/file.txt"
            )
            .unwrap(),
            blob
        );
        // Subtree path resolves to the subtree id.
        assert_eq!(
            resolve_rev_path(
                &git_dir,
                ObjectFormat::Sha1,
                &db,
                &commit.to_hex(),
                "dir/sub"
            )
            .unwrap(),
            sub
        );
        // Empty path resolves to the commit's tree.
        assert_eq!(
            resolve_rev_path(&git_dir, ObjectFormat::Sha1, &db, &commit.to_hex(), "").unwrap(),
            root
        );
        let entry = resolve_rev_path_entry(
            &git_dir,
            ObjectFormat::Sha1,
            &db,
            &commit.to_hex(),
            "dir/sub/file.txt",
        )
        .unwrap();
        assert_eq!(entry.oid, blob);
        assert_eq!(entry.mode, Some(0o100644));
        assert_eq!(entry.object_type, ObjectType::Blob);
        assert_eq!(entry.name, b"file.txt");
        let entry = resolve_rev_path_entry(&git_dir, ObjectFormat::Sha1, &db, &commit.to_hex(), "")
            .unwrap();
        assert_eq!(entry.oid, root);
        assert_eq!(entry.mode, None);
        assert_eq!(entry.object_type, ObjectType::Tree);
        assert!(entry.name.is_empty());
        // Resolvable through the unified string resolver too.
        assert_eq!(
            resolve_revision_with_reader(
                &git_dir,
                ObjectFormat::Sha1,
                &db,
                &format!("{commit}:dir/sub/file.txt"),
            )
            .unwrap(),
            blob
        );
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn resolve_rev_path_reports_missing_and_non_tree_paths() {
        let git_dir = temp_git_dir();
        let mut db = ObjectDatabase::new(ObjectFormat::Sha1);
        let blob = db
            .write_object(EncodedObject::new(ObjectType::Blob, b"root\n".to_vec()))
            .unwrap();
        let root = write_tree(&mut db, &[(0o100644, b"root.txt", &blob)]);
        let commit = write_test_commit(&mut db, root, Vec::new(), b"init\n");

        // Missing path.
        let missing = resolve_rev_path(
            &git_dir,
            ObjectFormat::Sha1,
            &db,
            &commit.to_hex(),
            "nope.txt",
        )
        .unwrap_err();
        assert!(
            matches!(&missing, GitError::NotFound(msg) if msg.contains("does not exist")),
            "unexpected error: {missing:?}"
        );

        // Descending through a blob is "not a tree" -> reported as not found.
        let not_tree = resolve_rev_path(
            &git_dir,
            ObjectFormat::Sha1,
            &db,
            &commit.to_hex(),
            "root.txt/x",
        )
        .unwrap_err();
        assert!(
            matches!(&not_tree, GitError::NotFound(msg) if msg.contains("does not exist")),
            "unexpected error: {not_tree:?}"
        );
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn resolve_index_path_reads_stage_entries() {
        let git_dir = temp_git_dir();
        let oid_zero = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .unwrap();
        let oid_two = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "2222222222222222222222222222222222222222",
        )
        .unwrap();
        let index = Index {
            version: 2,
            entries: vec![
                test_index_entry(b"file.txt", &oid_zero, 0),
                test_index_entry(b"conflict.txt", &oid_two, 2),
            ],
            extensions: Vec::new(),
            checksum: None,
        };
        fs::write(
            git_dir.join("index"),
            index.write(ObjectFormat::Sha1).unwrap(),
        )
        .unwrap();

        // `:path` defaults to stage 0.
        assert_eq!(
            resolve_revision_with_reader(
                &git_dir,
                ObjectFormat::Sha1,
                &ObjectDatabase::new(ObjectFormat::Sha1),
                ":file.txt",
            )
            .unwrap(),
            oid_zero
        );
        // `:N:path` selects a specific stage.
        assert_eq!(
            resolve_revision_with_reader(
                &git_dir,
                ObjectFormat::Sha1,
                &ObjectDatabase::new(ObjectFormat::Sha1),
                ":2:conflict.txt",
            )
            .unwrap(),
            oid_two
        );
        // Wrong stage reports a stage-specific error.
        let wrong_stage = resolve_revision_with_reader(
            &git_dir,
            ObjectFormat::Sha1,
            &ObjectDatabase::new(ObjectFormat::Sha1),
            ":1:conflict.txt",
        )
        .unwrap_err();
        assert!(
            matches!(&wrong_stage, GitError::NotFound(msg) if msg.contains("not at stage 1")),
            "unexpected error: {wrong_stage:?}"
        );
        // Unknown path reports "not in the index".
        let unknown = resolve_revision_with_reader(
            &git_dir,
            ObjectFormat::Sha1,
            &ObjectDatabase::new(ObjectFormat::Sha1),
            ":missing.txt",
        )
        .unwrap_err();
        assert!(
            matches!(&unknown, GitError::NotFound(msg) if msg.contains("not in the index")),
            "unexpected error: {unknown:?}"
        );
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn search_commit_message_all_finds_matching_commit() {
        let git_dir = temp_git_dir();
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let tree = db
            .write_object(EncodedObject::new(ObjectType::Tree, Vec::new()))
            .unwrap();
        let first = write_dated_commit(&mut db, tree.clone(), Vec::new(), b"add feature\n", 1000);
        let second = write_dated_commit(
            &mut db,
            tree.clone(),
            vec![first.clone()],
            b"fix the widget bug\n",
            2000,
        );
        let third = write_dated_commit(
            &mut db,
            tree,
            vec![second.clone()],
            b"unrelated change\n",
            3000,
        );
        let refs = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let mut tx = refs.transaction();
        tx.update(RefUpdate {
            name: "refs/heads/main".into(),
            expected: None,
            new: RefTarget::Direct(third.clone()),
            reflog: None,
        });
        tx.commit().unwrap();

        assert_eq!(
            resolve_revision_with_reader(&git_dir, ObjectFormat::Sha1, &db, ":/widget bug")
                .unwrap(),
            second
        );
        // `^{/regex}` over first-parent history finds the same commit from the tip.
        assert_eq!(
            resolve_revision_with_reader(
                &git_dir,
                ObjectFormat::Sha1,
                &db,
                &format!("{third}^{{/widget bug}}"),
            )
            .unwrap(),
            second
        );
        // No match is an error.
        let miss = resolve_revision_with_reader(&git_dir, ObjectFormat::Sha1, &db, ":/zzznomatch")
            .unwrap_err();
        assert!(
            matches!(miss, GitError::NotFound(_)),
            "unexpected: {miss:?}"
        );
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn parse_revision_range_recognizes_dot_forms() {
        assert_eq!(
            parse_revision_range("a..b"),
            Some(RevisionRange::Asymmetric {
                start: "a".into(),
                end: "b".into(),
            })
        );
        assert_eq!(
            parse_revision_range("a...b"),
            Some(RevisionRange::Symmetric {
                left: "a".into(),
                right: "b".into(),
            })
        );
        assert_eq!(
            parse_revision_range("..b"),
            Some(RevisionRange::Asymmetric {
                start: "HEAD".into(),
                end: "b".into(),
            })
        );
        assert_eq!(
            parse_revision_range("a.."),
            Some(RevisionRange::Asymmetric {
                start: "a".into(),
                end: "HEAD".into(),
            })
        );
        assert_eq!(parse_revision_range("plain"), None);
    }

    #[test]
    fn resolve_revision_range_excludes_ancestors_and_symmetric_difference() {
        let git_dir = temp_git_dir();
        let mut db = ObjectDatabase::new(ObjectFormat::Sha1);
        let tree = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "4b825dc642cb6eb9a060e54bf8d69288fbee4904",
        )
        .unwrap();
        // base -> a -> b   (left line)
        //   \--> c -> d    (right line)
        let base = write_test_commit(&mut db, tree.clone(), Vec::new(), b"base\n");
        let a = write_test_commit(&mut db, tree.clone(), vec![base.clone()], b"a\n");
        let b = write_test_commit(&mut db, tree.clone(), vec![a.clone()], b"b\n");
        let c = write_test_commit(&mut db, tree.clone(), vec![base.clone()], b"c\n");
        let d = write_test_commit(&mut db, tree, vec![c.clone()], b"d\n");

        // A..B: reachable from B (a..b line) but not from A (base only here) ->
        // {a, b}; base and earlier are excluded.
        let range = RevisionRange::Asymmetric {
            start: a.to_hex(),
            end: b.to_hex(),
        };
        let mut got = resolve_revision_range(&git_dir, ObjectFormat::Sha1, &db, &range).unwrap();
        got.sort_by(|x, y| x.to_hex().cmp(&y.to_hex()));
        assert_eq!(got, vec![b.clone()]);
        assert!(!got.contains(&a), "A itself is excluded");
        assert!(!got.contains(&base), "A's ancestors are excluded");

        // b...d: symmetric difference excludes the shared `base` while keeping
        // both unique sides {a, b} and {c, d}.
        let sym = RevisionRange::Symmetric {
            left: b.to_hex(),
            right: d.to_hex(),
        };
        let got_sym: HashSet<ObjectId> =
            resolve_revision_range(&git_dir, ObjectFormat::Sha1, &db, &sym)
                .unwrap()
                .into_iter()
                .collect();
        let expected: HashSet<ObjectId> = [a, b, c, d].into_iter().collect();
        assert_eq!(got_sym, expected);
        assert!(!got_sym.contains(&base), "shared base excluded from ...");
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn revision_selection_resolves_asymmetric_range() {
        let git_dir = temp_git_dir();
        let format = ObjectFormat::Sha1;
        let (db, all) = build_history(&git_dir, format);
        let root = all[0].clone();
        let a = all[1].clone();
        let c = all[3].clone();

        let selection = RevisionSelection::from_specs([format!("{a}..{c}")]).unwrap();
        let resolved = selection.resolve(&git_dir, format, &db).unwrap();

        assert_eq!(resolved.starts, vec![c.clone()]);
        assert_eq!(resolved.excluded, oid_set([root, a]));
        assert_oid_set(
            resolved
                .selected_commit_oids(&git_dir, format, &db, false)
                .unwrap(),
            [c],
        );
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn revision_selection_resolves_default_left_range() {
        let git_dir = temp_git_dir();
        let format = ObjectFormat::Sha1;
        let (db, all) = build_history(&git_dir, format);
        let root = all[0].clone();
        let a = all[1].clone();
        let c = all[3].clone();
        fs::write(git_dir.join("HEAD"), b"ref: refs/heads/main\n").unwrap();
        set_branch(&git_dir, "main", &a);

        let selection = RevisionSelection::from_specs([format!("..{c}")]).unwrap();
        let resolved = selection.resolve(&git_dir, format, &db).unwrap();

        assert_eq!(resolved.starts, vec![c.clone()]);
        assert_eq!(resolved.excluded, oid_set([root, a]));
        assert_oid_set(
            resolved
                .selected_commit_oids(&git_dir, format, &db, false)
                .unwrap(),
            [c],
        );
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn revision_selection_resolves_default_right_range() {
        let git_dir = temp_git_dir();
        let format = ObjectFormat::Sha1;
        let (db, all) = build_history(&git_dir, format);
        let root = all[0].clone();
        let a = all[1].clone();
        let c = all[3].clone();
        fs::write(git_dir.join("HEAD"), b"ref: refs/heads/main\n").unwrap();
        set_branch(&git_dir, "main", &c);

        let selection = RevisionSelection::from_specs([format!("{a}..")]).unwrap();
        let resolved = selection.resolve(&git_dir, format, &db).unwrap();

        assert_eq!(resolved.starts, vec![c.clone()]);
        assert_eq!(resolved.excluded, oid_set([root, a]));
        assert_oid_set(
            resolved
                .selected_commit_oids(&git_dir, format, &db, false)
                .unwrap(),
            [c],
        );
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn revision_selection_resolves_symmetric_range() {
        let git_dir = temp_git_dir();
        let format = ObjectFormat::Sha1;
        let (db, all) = build_history(&git_dir, format);
        let root = all[0].clone();
        let a = all[1].clone();
        let b = all[2].clone();

        let selection = RevisionSelection::from_specs([format!("{a}...{b}")]).unwrap();
        let resolved = selection.resolve(&git_dir, format, &db).unwrap();

        assert_eq!(resolved.starts, vec![a.clone(), b.clone()]);
        assert_eq!(resolved.excluded, oid_set([root]));
        assert_oid_set(
            resolved
                .selected_commit_oids(&git_dir, format, &db, false)
                .unwrap(),
            [a, b],
        );
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn revision_selection_resolves_caret_exclude() {
        let git_dir = temp_git_dir();
        let format = ObjectFormat::Sha1;
        let (db, all) = build_history(&git_dir, format);
        let root = all[0].clone();
        let a = all[1].clone();

        let selection = RevisionSelection::from_specs([format!("^{a}")]).unwrap();
        let resolved = selection.resolve(&git_dir, format, &db).unwrap();

        assert!(resolved.starts.is_empty());
        assert_eq!(resolved.excluded, oid_set([root, a]));
        assert!(
            resolved
                .selected_commit_oids(&git_dir, format, &db, false)
                .unwrap()
                .is_empty()
        );
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn revision_selection_resolves_bare_include() {
        let git_dir = temp_git_dir();
        let format = ObjectFormat::Sha1;
        let (db, all) = build_history(&git_dir, format);
        let root = all[0].clone();
        let a = all[1].clone();
        let c = all[3].clone();

        let selection = RevisionSelection::from_specs([c.to_hex()]).unwrap();
        let resolved = selection.resolve(&git_dir, format, &db).unwrap();

        assert_eq!(resolved.starts, vec![c.clone()]);
        assert!(resolved.excluded.is_empty());
        assert_oid_set(
            resolved
                .selected_commit_oids(&git_dir, format, &db, false)
                .unwrap(),
            [root, a, c],
        );
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn merge_bases_finds_common_ancestor() {
        let git_dir = temp_git_dir();
        let mut db = ObjectDatabase::new(ObjectFormat::Sha1);
        let tree = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "4b825dc642cb6eb9a060e54bf8d69288fbee4904",
        )
        .unwrap();
        let base = write_test_commit(&mut db, tree.clone(), Vec::new(), b"base\n");
        let left = write_test_commit(&mut db, tree.clone(), vec![base.clone()], b"left\n");
        let right = write_test_commit(&mut db, tree, vec![base.clone()], b"right\n");
        assert_eq!(
            merge_bases(&git_dir, ObjectFormat::Sha1, &db, &left, &right).unwrap(),
            vec![base]
        );
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn resolve_bare_at_is_head() {
        let git_dir = temp_git_dir();
        let oid = test_oid(0xaa);
        fs::write(git_dir.join("HEAD"), b"ref: refs/heads/main\n").unwrap();
        set_branch(&git_dir, "main", &oid);
        assert_eq!(
            resolve_revision(&git_dir, ObjectFormat::Sha1, "@").unwrap(),
            oid
        );
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn resolve_head_reflog_nth() {
        let git_dir = temp_git_dir();
        let c0 = test_oid(0x10);
        let c1 = test_oid(0x11);
        let c2 = test_oid(0x12);
        fs::write(git_dir.join("HEAD"), b"ref: refs/heads/main\n").unwrap();
        set_branch(&git_dir, "main", &c2);
        // Oldest-first reflog: c0 -> c1 -> c2 (c2 is the current value).
        write_head_reflog(
            &git_dir,
            &[
                (&zero_oid(), &c0, "commit (initial): c0"),
                (&c0, &c1, "commit: c1"),
                (&c1, &c2, "commit: c2"),
            ],
        );

        // `@{0}` is the current value, `@{1}`/`@{2}` walk back through the log.
        assert_eq!(
            resolve_revision(&git_dir, ObjectFormat::Sha1, "@{0}").unwrap(),
            c2
        );
        assert_eq!(
            resolve_revision(&git_dir, ObjectFormat::Sha1, "HEAD@{1}").unwrap(),
            c1
        );
        assert_eq!(
            resolve_revision(&git_dir, ObjectFormat::Sha1, "@{2}").unwrap(),
            c0
        );
        // Out-of-range reports a git-style "only has N entries" error.
        let err = resolve_revision(&git_dir, ObjectFormat::Sha1, "@{5}").unwrap_err();
        assert!(
            matches!(&err, GitError::NotFound(msg) if msg.contains("only has 3 entries")),
            "unexpected error: {err:?}"
        );
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn resolve_branch_reflog_nth() {
        let git_dir = temp_git_dir();
        let old = test_oid(0x20);
        let new = test_oid(0x21);
        set_branch(&git_dir, "topic", &new);
        write_branch_reflog(
            &git_dir,
            "topic",
            &[
                (&zero_oid(), &old, "branch: Created"),
                (&old, &new, "commit: work"),
            ],
        );
        assert_eq!(
            resolve_revision(&git_dir, ObjectFormat::Sha1, "topic@{0}").unwrap(),
            new
        );
        assert_eq!(
            resolve_revision(&git_dir, ObjectFormat::Sha1, "topic@{1}").unwrap(),
            old
        );
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn resolve_upstream_via_branch_config() {
        let git_dir = temp_git_dir();
        let tip = test_oid(0x30);
        fs::write(git_dir.join("HEAD"), b"ref: refs/heads/main\n").unwrap();
        set_branch(&git_dir, "main", &tip);
        set_ref(&git_dir, "refs/remotes/origin/main", &tip);
        fs::write(
            git_dir.join("config"),
            b"[branch \"main\"]\n\tremote = origin\n\tmerge = refs/heads/main\n",
        )
        .unwrap();

        for spec in ["@{u}", "@{upstream}", "main@{upstream}"] {
            assert_eq!(
                resolve_revision(&git_dir, ObjectFormat::Sha1, spec).unwrap(),
                tip,
                "spec {spec}"
            );
        }
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn resolve_push_falls_back_to_upstream_then_uses_push_remote() {
        let git_dir = temp_git_dir();
        let up = test_oid(0x40);
        let pushed = test_oid(0x41);
        fs::write(git_dir.join("HEAD"), b"ref: refs/heads/main\n").unwrap();
        set_branch(&git_dir, "main", &up);
        set_ref(&git_dir, "refs/remotes/origin/main", &up);

        // No push-specific config: `@{push}` mirrors `@{u}` (origin/main).
        fs::write(
            git_dir.join("config"),
            b"[branch \"main\"]\n\tremote = origin\n\tmerge = refs/heads/main\n",
        )
        .unwrap();
        assert_eq!(
            resolve_revision(&git_dir, ObjectFormat::Sha1, "@{push}").unwrap(),
            up
        );

        // With a pushRemote, `@{push}` follows refs/remotes/<pushRemote>/<short>.
        set_ref(&git_dir, "refs/remotes/fork/main", &pushed);
        fs::write(
            git_dir.join("config"),
            b"[branch \"main\"]\n\tremote = origin\n\tpushRemote = fork\n\tmerge = refs/heads/main\n",
        )
        .unwrap();
        assert_eq!(
            resolve_revision(&git_dir, ObjectFormat::Sha1, "@{push}").unwrap(),
            pushed
        );
        // `@{u}` still uses the upstream remote, not the push remote.
        assert_eq!(
            resolve_revision(&git_dir, ObjectFormat::Sha1, "@{u}").unwrap(),
            up
        );
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn resolve_previous_checkout_branch() {
        let git_dir = temp_git_dir();
        let main_tip = test_oid(0x50);
        let feature_tip = test_oid(0x51);
        fs::write(git_dir.join("HEAD"), b"ref: refs/heads/feature\n").unwrap();
        set_branch(&git_dir, "main", &main_tip);
        set_branch(&git_dir, "feature", &feature_tip);
        // Checkout history: ... -> feature -> main -> feature (newest last).
        write_head_reflog(
            &git_dir,
            &[
                (
                    &feature_tip,
                    &feature_tip,
                    "checkout: moving from main to feature",
                ),
                (
                    &feature_tip,
                    &main_tip,
                    "checkout: moving from feature to main",
                ),
                (
                    &main_tip,
                    &feature_tip,
                    "checkout: moving from main to feature",
                ),
            ],
        );
        // `@{-1}` = branch we left most recently (main) -> its current tip.
        assert_eq!(
            resolve_revision(&git_dir, ObjectFormat::Sha1, "@{-1}").unwrap(),
            main_tip
        );
        // `@{-2}` = the checkout before that (feature).
        assert_eq!(
            resolve_revision(&git_dir, ObjectFormat::Sha1, "@{-2}").unwrap(),
            feature_tip
        );
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn at_selector_composes_with_parent_suffix() {
        // `@{0}^` must resolve the reflog value first, then apply `^`: the
        // suffix splitter peels the `^` and recurses back into the `@{...}` base.
        let git_dir = temp_git_dir();
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let tree = db
            .write_object(EncodedObject::new(ObjectType::Tree, Vec::new()))
            .unwrap();
        let parent = write_dated_commit(&mut db, tree.clone(), Vec::new(), b"parent\n", 1000);
        let child = write_dated_commit(&mut db, tree, vec![parent.clone()], b"child\n", 2000);
        fs::write(git_dir.join("HEAD"), b"ref: refs/heads/main\n").unwrap();
        set_branch(&git_dir, "main", &child);
        write_head_reflog(
            &git_dir,
            &[
                (&zero_oid(), &parent, "commit (initial): parent"),
                (&parent, &child, "commit: child"),
            ],
        );
        assert_eq!(
            resolve_revision(&git_dir, ObjectFormat::Sha1, "@{0}").unwrap(),
            child
        );
        assert_eq!(
            resolve_revision(&git_dir, ObjectFormat::Sha1, "@{0}^").unwrap(),
            parent
        );
        assert_eq!(
            resolve_revision(&git_dir, ObjectFormat::Sha1, "HEAD@{0}~1").unwrap(),
            parent
        );
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn resolve_at_selector_rejects_unsupported_and_malformed() {
        let git_dir = temp_git_dir();
        fs::write(git_dir.join("HEAD"), b"ref: refs/heads/main\n").unwrap();
        set_branch(&git_dir, "main", &test_oid(0x60));
        // Date-based selectors are not implemented.
        let unsupported =
            resolve_revision(&git_dir, ObjectFormat::Sha1, "@{yesterday}").unwrap_err();
        assert!(
            matches!(&unsupported, GitError::Unsupported(_)),
            "unexpected error: {unsupported:?}"
        );
        // `@{-N}` only applies to a bare base.
        let bad_base = resolve_revision(&git_dir, ObjectFormat::Sha1, "main@{-1}").unwrap_err();
        assert!(
            matches!(&bad_base, GitError::InvalidFormat(_)),
            "unexpected error: {bad_base:?}"
        );
        fs::remove_dir_all(git_dir).unwrap();
    }

    fn test_oid(byte: u8) -> ObjectId {
        ObjectId::from_hex(ObjectFormat::Sha1, &format!("{byte:02x}").repeat(20)).unwrap()
    }

    fn zero_oid() -> ObjectId {
        ObjectId::from_hex(ObjectFormat::Sha1, &"0".repeat(40)).unwrap()
    }

    fn oid_set(oids: impl IntoIterator<Item = ObjectId>) -> HashSet<ObjectId> {
        oids.into_iter().collect()
    }

    fn assert_oid_set(
        actual: impl IntoIterator<Item = ObjectId>,
        expected: impl IntoIterator<Item = ObjectId>,
    ) {
        assert_eq!(oid_set(actual), oid_set(expected));
    }

    fn set_branch(git_dir: &Path, branch: &str, oid: &ObjectId) {
        set_ref(git_dir, &format!("refs/heads/{branch}"), oid);
    }

    fn set_ref(git_dir: &Path, name: &str, oid: &ObjectId) {
        let refs = FileRefStore::new(git_dir, ObjectFormat::Sha1);
        let mut tx = refs.transaction();
        tx.update(RefUpdate {
            name: name.to_string(),
            expected: None,
            new: RefTarget::Direct(oid.clone()),
            reflog: None,
        });
        tx.commit().unwrap();
    }

    fn write_head_reflog(git_dir: &Path, entries: &[(&ObjectId, &ObjectId, &str)]) {
        write_reflog_for(git_dir, "HEAD", entries);
    }

    fn write_branch_reflog(git_dir: &Path, branch: &str, entries: &[(&ObjectId, &ObjectId, &str)]) {
        write_reflog_for(git_dir, &format!("refs/heads/{branch}"), entries);
    }

    fn write_reflog_for(git_dir: &Path, name: &str, entries: &[(&ObjectId, &ObjectId, &str)]) {
        let refs = FileRefStore::new(git_dir, ObjectFormat::Sha1);
        let entries: Vec<ReflogEntry> = entries
            .iter()
            .map(|(old, new, message)| ReflogEntry {
                old_oid: (*old).clone(),
                new_oid: (*new).clone(),
                committer: b"Example User <example@example.invalid> 1000 +0000".to_vec(),
                message: message.as_bytes().to_vec(),
            })
            .collect();
        refs.write_reflog(name, &entries).unwrap();
    }

    fn write_test_commit(
        db: &mut ObjectDatabase,
        tree: ObjectId,
        parents: Vec<ObjectId>,
        message: &[u8],
    ) -> ObjectId {
        let commit = Commit {
            tree,
            parents,
            author: b"Example User <example@example.invalid> 0 +0000".to_vec(),
            committer: b"Example User <example@example.invalid> 0 +0000".to_vec(),
            encoding: None,
            message: message.to_vec(),
        };
        db.write_object(EncodedObject::new(ObjectType::Commit, commit.write()))
            .unwrap()
    }

    fn write_dated_commit<W: ObjectWriter>(
        db: &mut W,
        tree: ObjectId,
        parents: Vec<ObjectId>,
        message: &[u8],
        when: i64,
    ) -> ObjectId {
        let ident = format!("Example User <example@example.invalid> {when} +0000");
        let commit = Commit {
            tree,
            parents,
            author: ident.clone().into_bytes(),
            committer: ident.into_bytes(),
            encoding: None,
            message: message.to_vec(),
        };
        db.write_object(EncodedObject::new(ObjectType::Commit, commit.write()))
            .unwrap()
    }

    fn write_tree(db: &mut ObjectDatabase, entries: &[(u32, &[u8], &ObjectId)]) -> ObjectId {
        let tree = Tree {
            entries: entries
                .iter()
                .map(|(mode, name, oid)| sley_object::TreeEntry {
                    mode: *mode,
                    name: name.to_vec(),
                    oid: (*oid).clone(),
                })
                .collect(),
        };
        db.write_object(EncodedObject::new(ObjectType::Tree, tree.write()))
            .unwrap()
    }

    fn test_index_entry(path: &[u8], oid: &ObjectId, stage: u16) -> sley_index::IndexEntry {
        sley_index::IndexEntry {
            ctime_seconds: 0,
            ctime_nanoseconds: 0,
            mtime_seconds: 0,
            mtime_nanoseconds: 0,
            dev: 0,
            ino: 0,
            mode: 0o100644,
            uid: 0,
            gid: 0,
            size: 0,
            oid: oid.clone(),
            flags: (stage & 0x3) << 12,
            flags_extended: 0,
            path: path.to_vec(),
        }
    }

    fn temp_git_dir() -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "sley-rev-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    /// An object reader that refuses every read, used to prove a query was
    /// answered entirely from the commit-graph (parent/ancestry lookups never
    /// touched the odb).
    struct PanicReader;
    impl ObjectReader for PanicReader {
        fn read_object(&self, oid: &ObjectId) -> Result<EncodedObject> {
            Err(GitError::NotFound(format!(
                "object reader must not be used for {oid}; graph should cover it"
            )))
        }
    }

    /// Compute topological generation numbers for `parents` (a child -> parents
    /// map). A root commit has generation 1; every other commit is one greater
    /// than the maximum generation among its parents -- exactly git's definition.
    fn generation_numbers(parents: &HashMap<ObjectId, Vec<ObjectId>>) -> HashMap<ObjectId, u32> {
        let mut generations: HashMap<ObjectId, u32> = HashMap::new();
        // Repeatedly relax until a fixpoint; histories here are tiny so a simple
        // loop is plenty and avoids an explicit topological sort.
        loop {
            let mut changed = false;
            for (oid, oid_parents) in parents {
                let candidate = oid_parents
                    .iter()
                    .map(|parent| generations.get(parent).copied().unwrap_or(0))
                    .max()
                    .unwrap_or(0)
                    + 1;
                if generations.get(oid).copied() != Some(candidate) {
                    // Only advance upward so the fixpoint is monotone.
                    let current = generations.get(oid).copied().unwrap_or(0);
                    if candidate > current {
                        generations.insert(oid.clone(), candidate);
                        changed = true;
                    }
                }
            }
            if !changed {
                break;
            }
        }
        generations
    }

    /// Write a real commit-graph (via `sley_formats::CommitGraph::write`) covering
    /// `commits` into `<git_dir>/objects/info/commit-graph`, with correct
    /// topological generation numbers and committer dates pulled from each
    /// commit object.
    fn write_commit_graph_file(
        git_dir: &Path,
        format: ObjectFormat,
        reader: &impl ObjectReader,
        commits: &[ObjectId],
    ) {
        let mut parents_map: HashMap<ObjectId, Vec<ObjectId>> = HashMap::new();
        for oid in commits {
            parents_map.insert(oid.clone(), commit_parents(reader, format, oid).unwrap());
        }
        let generations = generation_numbers(&parents_map);
        let entries: Vec<sley_formats::CommitGraphWriteEntry> = commits
            .iter()
            .map(|oid| {
                let object = reader.read_object(oid).unwrap();
                let commit = Commit::parse_ref(format, &object.body).unwrap();
                let commit_time =
                    commit_committer_time(commit.committer).unwrap_or(0).max(0) as u64;
                sley_formats::CommitGraphWriteEntry {
                    oid: oid.clone(),
                    tree: commit.tree,
                    parents: commit.parents,
                    generation: generations.get(oid).copied().unwrap_or(1),
                    commit_time,
                }
            })
            .collect();
        let bytes = CommitGraph::write(format, &entries).unwrap();
        let info = git_dir.join("objects").join("info");
        fs::create_dir_all(&info).unwrap();
        fs::write(info.join("commit-graph"), bytes).unwrap();
    }

    fn remove_commit_graph(git_dir: &Path) {
        let path = git_dir.join("objects").join("info").join("commit-graph");
        if path.exists() {
            fs::remove_file(path).unwrap();
        }
    }

    /// Build a fixed multi-shape history and return the database plus the named
    /// commits. Shape (arrows point child -> parent):
    ///
    /// ```text
    ///   root
    ///   /  \
    ///  a    b
    ///  |    |\
    ///  c    d e
    ///   \  / \|
    ///    m1   f      m1 = merge(c, d)   (two-parent merge)
    ///     \   |
    ///      \  g
    ///       \ |
    ///        oct = merge(m1, g, f)      (octopus, three parents)
    /// ```
    ///
    /// plus a criss-cross pair `x1 = merge(a, b)` and `x2 = merge(b, a)` whose
    /// two merge bases are `a`'s and `b`'s shared ancestor structure (root).
    fn build_history(git_dir: &Path, format: ObjectFormat) -> (FileObjectDatabase, Vec<ObjectId>) {
        let mut db = FileObjectDatabase::from_git_dir(git_dir, format);
        let tree = db
            .write_object(EncodedObject::new(ObjectType::Tree, Vec::new()))
            .unwrap();
        let mut t = 1000i64;
        let mut commit = |db: &mut FileObjectDatabase, parents: Vec<ObjectId>, msg: &[u8]| {
            t += 1;
            write_dated_commit(db, tree.clone(), parents, msg, t)
        };
        let root = commit(&mut db, vec![], b"root\n");
        let a = commit(&mut db, vec![root.clone()], b"a\n");
        let b = commit(&mut db, vec![root.clone()], b"b\n");
        let c = commit(&mut db, vec![a.clone()], b"c\n");
        let d = commit(&mut db, vec![b.clone()], b"d\n");
        let e = commit(&mut db, vec![b.clone()], b"e\n");
        let m1 = commit(&mut db, vec![c.clone(), d.clone()], b"m1\n");
        let f = commit(&mut db, vec![d.clone(), e.clone()], b"f\n");
        let g = commit(&mut db, vec![f.clone()], b"g\n");
        let oct = commit(&mut db, vec![m1.clone(), g.clone(), f.clone()], b"oct\n");
        let x1 = commit(&mut db, vec![a.clone(), b.clone()], b"x1\n");
        let x2 = commit(&mut db, vec![b.clone(), a.clone()], b"x2\n");
        let all = vec![root, a, b, c, d, e, m1, f, g, oct, x1, x2];
        (db, all)
    }

    #[test]
    fn graph_backed_walks_match_object_only_walks() {
        let git_dir = temp_git_dir();
        let format = ObjectFormat::Sha1;
        let (db, all) = build_history(&git_dir, format);

        // Exercise every ordered pair of commits across is_ancestor, merge_bases
        // (both orders), and both range forms; capture the object-only baseline
        // (no graph file), then the graph-backed result, and require equality.
        remove_commit_graph(&git_dir);
        let baseline = collect_walk_results(&git_dir, format, &db, &all);

        write_commit_graph_file(&git_dir, format, &db, &all);
        let with_graph = collect_walk_results(&git_dir, format, &db, &all);

        assert_eq!(
            baseline, with_graph,
            "graph-backed walk diverged from object-only walk"
        );
        fs::remove_dir_all(git_dir).unwrap();
    }

    /// Run is_ancestor, merge_bases (both orders), and the `A..B`/`A...B` ranges
    /// over all pairs, returning a deterministic snapshot for comparison.
    fn collect_walk_results(
        git_dir: &Path,
        format: ObjectFormat,
        reader: &impl ObjectReader,
        all: &[ObjectId],
    ) -> Vec<(String, String, bool, Vec<String>, Vec<String>, Vec<String>)> {
        let mut out = Vec::new();
        for left in all {
            for right in all {
                let anc = is_ancestor(git_dir, format, reader, left, right).unwrap();
                let mut bases: Vec<String> = merge_bases(git_dir, format, reader, left, right)
                    .unwrap()
                    .iter()
                    .map(|oid| oid.to_hex())
                    .collect();
                bases.sort();
                let asym = RevisionRange::Asymmetric {
                    start: left.to_hex(),
                    end: right.to_hex(),
                };
                let mut asym_set: Vec<String> =
                    resolve_revision_range(git_dir, format, reader, &asym)
                        .unwrap()
                        .iter()
                        .map(|oid| oid.to_hex())
                        .collect();
                asym_set.sort();
                let sym = RevisionRange::Symmetric {
                    left: left.to_hex(),
                    right: right.to_hex(),
                };
                let mut sym_set: Vec<String> =
                    resolve_revision_range(git_dir, format, reader, &sym)
                        .unwrap()
                        .iter()
                        .map(|oid| oid.to_hex())
                        .collect();
                sym_set.sort();
                out.push((left.to_hex(), right.to_hex(), anc, bases, asym_set, sym_set));
            }
        }
        out
    }

    #[test]
    fn graph_backed_merge_base_handles_octopus_and_criss_cross() {
        let git_dir = temp_git_dir();
        let format = ObjectFormat::Sha1;
        let (db, all) = build_history(&git_dir, format);
        // Names in build order: [root,a,b,c,d,e,m1,f,g,oct,x1,x2].
        let (a, b) = (all[1].clone(), all[2].clone());
        let (m1, oct) = (all[6].clone(), all[9].clone());
        let (x1, x2) = (all[10].clone(), all[11].clone());

        write_commit_graph_file(&git_dir, format, &db, &all);

        // Criss-cross: x1 = merge(a,b), x2 = merge(b,a) -> two merge bases {a,b}.
        let mut xbases = merge_bases(&git_dir, format, &db, &x1, &x2).unwrap();
        xbases.sort_by_key(|oid| oid.to_hex());
        let mut expected = vec![a.clone(), b.clone()];
        expected.sort_by_key(|oid| oid.to_hex());
        assert_eq!(xbases, expected, "criss-cross must yield two merge bases");

        // Octopus child reaches m1 along its first parent edge.
        assert!(is_ancestor(&git_dir, format, &db, &m1, &oct).unwrap());
        // m1 is a merge base of itself and the octopus.
        assert_eq!(
            merge_bases(&git_dir, format, &db, &m1, &oct).unwrap(),
            vec![m1.clone()]
        );
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn graph_backed_queries_avoid_object_reads() {
        let git_dir = temp_git_dir();
        let format = ObjectFormat::Sha1;
        let (db, all) = build_history(&git_dir, format);
        write_commit_graph_file(&git_dir, format, &db, &all);
        let (root, a, oct, x1, x2) = (
            all[0].clone(),
            all[1].clone(),
            all[9].clone(),
            all[10].clone(),
            all[11].clone(),
        );

        // With a complete graph, ancestry/merge-base queries must be answerable
        // without ever reading a commit object: PanicReader errors on any read.
        assert!(is_ancestor(&git_dir, format, &PanicReader, &root, &oct).unwrap());
        assert!(!is_ancestor(&git_dir, format, &PanicReader, &oct, &root).unwrap());
        assert!(is_ancestor(&git_dir, format, &PanicReader, &a, &oct).unwrap());

        let bases = merge_bases(&git_dir, format, &PanicReader, &x1, &x2).unwrap();
        assert_eq!(bases.len(), 2, "criss-cross bases via graph only");

        // Range resolution peels its two endpoints from the odb (the graph does
        // not record object types), but the ancestry *walk* between them is
        // graph-backed. Verify the result matches the object-only walk.
        let range = RevisionRange::Asymmetric {
            start: a.to_hex(),
            end: oct.to_hex(),
        };
        let mut included: Vec<String> = resolve_revision_range(&git_dir, format, &db, &range)
            .unwrap()
            .iter()
            .map(|oid| oid.to_hex())
            .collect();
        included.sort();
        assert!(included.contains(&oct.to_hex()));
        assert!(
            !included.contains(&root.to_hex()),
            "root is an ancestor of A, excluded"
        );

        // Merge-base and range results via the graph still equal the object-only
        // walk for the same queries.
        remove_commit_graph(&git_dir);
        let object_bases = merge_bases(&git_dir, format, &db, &x1, &x2).unwrap();
        let mut object_range: Vec<String> = resolve_revision_range(&git_dir, format, &db, &range)
            .unwrap()
            .iter()
            .map(|oid| oid.to_hex())
            .collect();
        object_range.sort();
        write_commit_graph_file(&git_dir, format, &db, &all);
        let graph_bases = merge_bases(&git_dir, format, &PanicReader, &x1, &x2).unwrap();
        assert_eq!(object_bases, graph_bases);
        assert_eq!(object_range, included, "range walk diverged with graph");
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn graph_backed_parent_suffix_matches_object_walk() {
        let git_dir = temp_git_dir();
        let format = ObjectFormat::Sha1;
        let (db, all) = build_history(&git_dir, format);
        let oct = all[9].clone();
        let (m1, g, f) = (all[6].clone(), all[8].clone(), all[7].clone());

        // Object-only baseline for the octopus merge's parent navigation.
        remove_commit_graph(&git_dir);
        let base_p1 =
            resolve_revision_with_reader(&git_dir, format, &db, &format!("{oct}^1")).unwrap();
        let base_p2 =
            resolve_revision_with_reader(&git_dir, format, &db, &format!("{oct}^2")).unwrap();
        let base_p3 =
            resolve_revision_with_reader(&git_dir, format, &db, &format!("{oct}^3")).unwrap();
        let base_first =
            resolve_revision_with_reader(&git_dir, format, &db, &format!("{oct}~1")).unwrap();
        assert_eq!((&base_p1, &base_p2, &base_p3), (&m1, &g, &f));
        assert_eq!(base_first, m1);

        // With the graph present, the same suffixes resolve without object reads.
        write_commit_graph_file(&git_dir, format, &db, &all);
        assert_eq!(
            resolve_revision_with_reader(&git_dir, format, &PanicReader, &format!("{oct}^2"))
                .unwrap(),
            base_p2
        );
        assert_eq!(
            resolve_revision_with_reader(&git_dir, format, &PanicReader, &format!("{oct}~1"))
                .unwrap(),
            base_first
        );
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn missing_or_unparseable_graph_falls_back_to_objects() {
        let git_dir = temp_git_dir();
        let format = ObjectFormat::Sha1;
        let (db, all) = build_history(&git_dir, format);
        let (a, oct) = (all[1].clone(), all[9].clone());
        let object_answer = is_ancestor(&git_dir, format, &db, &a, &oct).unwrap();

        // A corrupt graph file must be ignored (not error), falling back to the
        // odb so the answer is unchanged.
        let info = git_dir.join("objects").join("info");
        fs::create_dir_all(&info).unwrap();
        fs::write(info.join("commit-graph"), b"not a real commit graph").unwrap();
        assert_eq!(
            is_ancestor(&git_dir, format, &db, &a, &oct).unwrap(),
            object_answer
        );
        // A graph that omits some commits must also fall back per-missing-commit.
        write_commit_graph_file(&git_dir, format, &db, &all[..3]);
        assert_eq!(
            is_ancestor(&git_dir, format, &db, &a, &oct).unwrap(),
            object_answer
        );
        assert_eq!(
            merge_bases(&git_dir, format, &db, &all[10], &all[11]).unwrap(),
            {
                remove_commit_graph(&git_dir);
                merge_bases(&git_dir, format, &db, &all[10], &all[11]).unwrap()
            }
        );
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn commit_graph_chain_is_consulted() {
        let git_dir = temp_git_dir();
        let format = ObjectFormat::Sha1;
        // A short linear history whose single chain layer is self-contained
        // (no cross-layer parent edges), so the chain reader can resolve it.
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, format);
        let tree = db
            .write_object(EncodedObject::new(ObjectType::Tree, Vec::new()))
            .unwrap();
        let root = write_dated_commit(&mut db, tree.clone(), vec![], b"root\n", 1000);
        let mid = write_dated_commit(&mut db, tree.clone(), vec![root.clone()], b"mid\n", 1001);
        let tip = write_dated_commit(&mut db, tree.clone(), vec![mid.clone()], b"tip\n", 1002);
        let commits = vec![root.clone(), mid.clone(), tip.clone()];

        let parents_map: HashMap<ObjectId, Vec<ObjectId>> = commits
            .iter()
            .map(|oid| (oid.clone(), commit_parents(&db, format, oid).unwrap()))
            .collect();
        let generations = generation_numbers(&parents_map);
        let entries: Vec<sley_formats::CommitGraphWriteEntry> = commits
            .iter()
            .map(|oid| sley_formats::CommitGraphWriteEntry {
                oid: oid.clone(),
                tree: tree.clone(),
                parents: parents_map[oid].clone(),
                generation: generations[oid],
                commit_time: 0,
            })
            .collect();
        let bytes = CommitGraph::write(format, &entries).unwrap();

        // Lay the bytes out as a one-layer chain.
        let graphs = git_dir.join("objects").join("info").join("commit-graphs");
        fs::create_dir_all(&graphs).unwrap();
        let hash = sley_core::digest_bytes(format, &bytes).unwrap().to_hex();
        fs::write(graphs.join(format!("graph-{hash}.graph")), &bytes).unwrap();
        fs::write(graphs.join("commit-graph-chain"), format!("{hash}\n")).unwrap();

        // No monolithic commit-graph present, only the chain: queries must be
        // answerable from the chain without reading objects.
        assert!(
            !git_dir
                .join("objects")
                .join("info")
                .join("commit-graph")
                .exists()
        );
        assert!(is_ancestor(&git_dir, format, &PanicReader, &root, &tip).unwrap());
        assert_eq!(
            merge_bases(&git_dir, format, &PanicReader, &mid, &tip).unwrap(),
            vec![mid.clone()]
        );
        fs::remove_dir_all(git_dir).unwrap();
    }

    fn test_commit_graph(format: ObjectFormat, parent: &ObjectId, child: &ObjectId) -> Vec<u8> {
        let tree = ObjectId::from_hex(format, "4b825dc642cb6eb9a060e54bf8d69288fbee4904").unwrap();
        let mut oidf = vec![0u8; 256 * 4];
        let parent_first = parent.as_bytes()[0] as usize;
        let child_first = child.as_bytes()[0] as usize;
        for idx in 0..256 {
            let count = u32::from(idx >= parent_first) + u32::from(idx >= child_first);
            oidf[idx * 4..idx * 4 + 4].copy_from_slice(&count.to_be_bytes());
        }
        let mut oidl = Vec::new();
        oidl.extend_from_slice(parent.as_bytes());
        oidl.extend_from_slice(child.as_bytes());
        let mut cdat = Vec::new();
        cdat.extend_from_slice(&commit_graph_cdat_entry(
            &tree,
            0x7000_0000,
            0x7000_0000,
            1,
            1,
        ));
        cdat.extend_from_slice(&commit_graph_cdat_entry(&tree, 0, 0x7000_0000, 2, 2));
        commit_graph_file(
            format,
            &[(*b"OIDF", oidf), (*b"OIDL", oidl), (*b"CDAT", cdat)],
        )
    }

    fn commit_graph_cdat_entry(
        tree: &ObjectId,
        parent_one: u32,
        parent_two: u32,
        generation: u32,
        commit_time: u64,
    ) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(tree.as_bytes());
        out.extend_from_slice(&parent_one.to_be_bytes());
        out.extend_from_slice(&parent_two.to_be_bytes());
        let high = (generation << 2) | ((commit_time >> 32) as u32 & 0x3);
        out.extend_from_slice(&high.to_be_bytes());
        out.extend_from_slice(&(commit_time as u32).to_be_bytes());
        out
    }

    fn commit_graph_file(format: ObjectFormat, chunks: &[([u8; 4], Vec<u8>)]) -> Vec<u8> {
        let lookup_len = (chunks.len() + 1) * 12;
        let mut out = Vec::new();
        out.extend_from_slice(b"CGPH");
        out.push(1);
        out.push(match format {
            ObjectFormat::Sha1 => 1,
            ObjectFormat::Sha256 => 2,
        });
        out.push(chunks.len() as u8);
        out.push(0);
        let mut offset = (8 + lookup_len) as u64;
        for (id, data) in chunks {
            out.extend_from_slice(id);
            out.extend_from_slice(&offset.to_be_bytes());
            offset += data.len() as u64;
        }
        out.extend_from_slice(&[0, 0, 0, 0]);
        out.extend_from_slice(&offset.to_be_bytes());
        for (_id, data) in chunks {
            out.extend_from_slice(data);
        }
        let checksum = sley_core::digest_bytes(format, &out).unwrap();
        out.extend_from_slice(checksum.as_bytes());
        out
    }
}
