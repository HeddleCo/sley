pub mod graph;

use sley_config::GitConfig;
use sley_core::{GitError, ObjectFormat, ObjectId, Result};

pub use sley_core::BString;
use sley_formats::CommitGraph;
use sley_index::Index;
use sley_object::{Commit, EncodedObject, ObjectType, Tag, TreeEntries};
use sley_odb::{FileObjectDatabase, ObjectPrefixResolution, ObjectReader};
use sley_refs::{FileRefStore, PackedRef, RefTarget, resolve_ref_peeled, validate_symref_name};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

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
    resolve_revision_inner(git_dir, format, reader, rev, None)
}

/// Like [`resolve_revision_with_reader`], but resolves `@{upstream}` / `@{push}`
/// against a caller-supplied effective config instead of re-reading
/// `<git_dir>/config` blindly.
///
/// Callers that have already resolved the repository config — including
/// `include`/`includeIf` directives and command-line `-c` / `GIT_CONFIG_*`
/// overrides — pass it here so upstream resolution honours the same
/// `branch.<name>.{remote,merge}` the rest of the command sees. When `config`
/// is `None` (or the upstream path is reached without one), this falls back to an
/// include-aware read of `<git_dir>/config`.
pub fn resolve_revision_with_config<R: ObjectReader>(
    git_dir: &Path,
    format: ObjectFormat,
    reader: &R,
    rev: &str,
    config: &GitConfig,
) -> Result<ObjectId> {
    resolve_revision_inner(git_dir, format, reader, rev, Some(config))
}

fn resolve_revision_inner<R: ObjectReader>(
    git_dir: &Path,
    format: ObjectFormat,
    reader: &R,
    rev: &str,
    config: Option<&GitConfig>,
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
    if let Some(oid) = resolve_at_selector(git_dir, format, rev, config)? {
        return Ok(oid);
    }
    if let Some((base, suffix)) = split_revision_suffix(rev)? {
        if base.is_empty() {
            return Err(GitError::InvalidFormat(format!(
                "revision {rev} has empty base"
            )));
        }
        let base_oid = resolve_revision_inner(git_dir, format, reader, base, config)?;
        return apply_revision_suffix(git_dir, reader, format, &base_oid, suffix, rev);
    }
    resolve_revision_name(git_dir, format, rev)
}

pub struct RevisionResolver<'a, R> {
    git_dir: &'a Path,
    format: ObjectFormat,
    reader: &'a R,
    config: Option<&'a GitConfig>,
}

impl<'a, R: ObjectReader> RevisionResolver<'a, R> {
    pub fn new(git_dir: &'a Path, format: ObjectFormat, reader: &'a R) -> Self {
        Self {
            git_dir,
            format,
            reader,
            config: None,
        }
    }

    /// Attach a caller-resolved effective config so `@{upstream}` / `@{push}`
    /// honour `include`/`includeIf` and `-c` / `GIT_CONFIG_*` overrides. See
    /// [`resolve_revision_with_config`].
    pub fn with_config(mut self, config: &'a GitConfig) -> Self {
        self.config = Some(config);
        self
    }

    pub fn resolve(&self, rev: &str) -> Result<ObjectId> {
        resolve_revision_inner(self.git_dir, self.format, self.reader, rev, self.config)
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

    /// `<rev>:<path>` resolution that follows in-tree symlinks, as
    /// `git cat-file --follow-symlinks` does. See
    /// [`resolve_rev_path_follow_symlinks`].
    pub fn resolve_path_follow_symlinks(&self, rev: &str, path: &str) -> SymlinkedTreePath {
        resolve_rev_path_follow_symlinks(self.git_dir, self.format, self.reader, rev, path)
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
    Err(GitError::not_found(format!("revision {rev}")))
}

fn resolve_revision_ref(refs: &FileRefStore, rev: &str) -> Result<Option<ObjectId>> {
    let initial = if rev == "HEAD" {
        "HEAD".to_string()
    } else if rev.starts_with("refs/") {
        rev.to_string()
    } else if refs.read_ref(&format!("refs/heads/{rev}"))?.is_some() {
        format!("refs/heads/{rev}")
    } else if refs.read_ref(&format!("refs/tags/{rev}"))?.is_some() {
        format!("refs/tags/{rev}")
    } else if rev.contains('/') && refs.read_ref(&format!("refs/{rev}"))?.is_some() {
        // git's lookup rule #2 ("refs/%s") — e.g. `bisect/bad`, `notes/commits`.
        format!("refs/{rev}")
    } else if refs.read_ref(&format!("refs/remotes/{rev}"))?.is_some() {
        format!("refs/remotes/{rev}")
    } else if refs.read_ref(&format!("refs/remotes/{rev}/HEAD"))?.is_some() {
        format!("refs/remotes/{rev}/HEAD")
    } else if validate_symref_name(rev).is_ok() {
        rev.to_string()
    } else {
        return Ok(None);
    };
    resolve_ref_peeled(refs, &initial)
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
    config: Option<&GitConfig>,
) -> Result<Option<ObjectId>> {
    // Bare `@` is an alias for HEAD.
    if rev == "@" {
        let refs = FileRefStore::new(git_dir.to_path_buf(), format);
        return match resolve_revision_ref(&refs, "HEAD")? {
            Some(oid) => Ok(Some(oid)),
            None => Err(GitError::not_found("revision @")),
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
        return Ok(Some(resolve_upstream(
            git_dir, format, base, false, rev, config,
        )?));
    }
    if inner == "push" {
        return Ok(Some(resolve_upstream(
            git_dir, format, base, true, rev, config,
        )?));
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
        return Err(GitError::not_found(format!(
            "no reflog for '{}' to resolve {rev}",
            reflog_display_name(base)
        )));
    }
    // `@{N}` counts back from the newest entry; index `len - 1 - n`.
    let len = entries.len();
    if n >= len {
        return Err(GitError::not_found(format!(
            "log for '{}' only has {len} entries",
            reflog_display_name(base)
        )));
    }
    Ok(entries[len - 1 - n].new_oid)
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
                GitError::not_found(format!(
                    "could not resolve previous branch '{from}' for {rev}"
                ))
            });
        }
    }
    Err(GitError::not_found(format!(
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
    config: Option<&GitConfig>,
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

    // Prefer the caller-resolved effective config (includes + `-c` overrides);
    // fall back to an include-aware read of `<git_dir>/config` when none was
    // threaded in.
    let owned_config;
    let config = match config {
        Some(config) => config,
        None => {
            owned_config = read_repo_config(git_dir)?;
            &owned_config
        }
    };
    let merge = config
        .get("branch", Some(&branch), "merge")
        .ok_or_else(|| {
            GitError::not_found(format!("no upstream configured for branch '{branch}'"))
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
    .ok_or_else(|| GitError::not_found(format!("no upstream remote for branch '{branch}'")))?;

    let tracking = format!("refs/remotes/{remote}/{short}");
    match resolve_revision_ref(&refs, &tracking)? {
        Some(oid) => Ok(oid),
        None => Err(GitError::not_found(format!(
            "upstream tracking ref '{tracking}' for {rev} is missing"
        ))),
    }
}

/// Read the repository config (`<git_dir>/config`), resolving `include`/`includeIf`
/// directives and layering inherited `GIT_CONFIG_*` overrides.
///
/// This is the fallback used when a caller did not thread its already-resolved
/// effective config in via [`resolve_revision_with_config`]; it shares
/// [`sley_config::read_repo_config`] so a missing file is treated as empty and
/// includes are honoured. (Command-line `-c` overrides the CLI holds in-process
/// are only visible when the caller passes the resolved config explicitly.)
fn read_repo_config(git_dir: &Path) -> Result<GitConfig> {
    sley_config::read_repo_config(git_dir, None)
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
                // `<rev>^0` is not "the 0th parent" — git defines it as "peel to
                // a commit": dereference tags/etc. down to the commit object the
                // revision names. For an annotated tag this follows the tag to
                // its commit; for a commit it is the commit itself.
                let _ = raw_rev;
                return peel_revision(reader, format, base, PeelKind::Commit);
            }
            let mut graph = CommitGraphContext::load(git_dir, format);
            graph
                .commit_parents(reader, base)?
                .get(parent - 1)
                .cloned()
                .ok_or_else(|| GitError::not_found(format!("parent {parent} of {base}")))
        }
        RevisionSuffix::FirstParent(count) => {
            let mut graph = CommitGraphContext::load(git_dir, format);
            let mut current = *base;
            for _ in 0..count {
                current = graph
                    .commit_first_parent(reader, &current)?
                    .ok_or_else(|| GitError::not_found(format!("first parent of {current}")))?;
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

#[derive(Debug, Clone)]
struct GraphBloomCommit {
    parents: Vec<ObjectId>,
    filter: Vec<u8>,
    settings: sley_formats::CommitGraphBloomSettings,
}

#[derive(Debug, Clone, Copy, Default)]
struct GraphBloomStats {
    filter_not_present: usize,
    maybe: usize,
    definitely_not: usize,
    false_positive: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GraphBloomConsult {
    DefinitelyNot,
    Maybe,
    NotPresent,
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

    /// First parent of `oid` from the graph. The outer `None` means the commit is
    /// not present in the graph; the inner `None` means the commit is present but
    /// root/unborn with no parents.
    fn first_parent(&mut self, oid: &ObjectId) -> Option<Option<ObjectId>> {
        self.lookup(oid)
            .map(|commit| commit.parents.first().cloned())
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
        // Graft seam: history is cut at shallow boundary commits, so walks
        // must see them as parentless regardless of graph/object contents.
        if reader.is_shallow_graft(oid) {
            return Ok(Vec::new());
        }
        if let Some(parents) = self.parents(oid) {
            return Ok(parents);
        }
        commit_parents(reader, self.format, oid)
    }

    /// First parent of `oid`: from the graph when present, otherwise read+parsed
    /// from the commit object via `reader`.
    fn commit_first_parent<R: ObjectReader>(
        &mut self,
        reader: &R,
        oid: &ObjectId,
    ) -> Result<Option<ObjectId>> {
        if reader.is_shallow_graft(oid) {
            return Ok(None);
        }
        if let Some(parent) = self.first_parent(oid) {
            return Ok(parent);
        }
        Ok(commit_parents(reader, self.format, oid)?.into_iter().next())
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
            parents.push(parent_entry.oid);
        }
        map.insert(
            entry.oid,
            GraphCommit {
                parents,
                generation: entry.generation,
                commit_time: entry.commit_time,
            },
        );
    }
    Ok(map)
}

fn load_commit_graph_bloom_map(
    objects_dir: &Path,
    format: sley_core::ObjectFormat,
) -> HashMap<ObjectId, GraphBloomCommit> {
    let graph_path = objects_dir.join("info").join("commit-graph");
    if !graph_path.exists() {
        return HashMap::new();
    }
    fs::read(&graph_path)
        .map_err(|err| GitError::Io(err.to_string()))
        .and_then(|bytes| CommitGraph::parse(&bytes, format))
        .and_then(|graph| graph_to_bloom_map(&graph))
        .unwrap_or_default()
}

fn graph_to_bloom_map(graph: &CommitGraph) -> Result<HashMap<ObjectId, GraphBloomCommit>> {
    let Some(filters) = &graph.bloom_filters else {
        return Ok(HashMap::new());
    };
    let settings = sley_formats::CommitGraphBloomSettings {
        hash_version: filters.hash_version,
        hash_count: filters.hash_count,
        bits_per_entry: filters.bits_per_entry,
        max_changed_paths: sley_formats::DEFAULT_COMMIT_GRAPH_BLOOM_SETTINGS.max_changed_paths,
    };
    let mut map = HashMap::with_capacity(graph.commits.len());
    for (idx, entry) in graph.commits.iter().enumerate() {
        let mut parents = Vec::with_capacity(entry.parents.len());
        for parent in &entry.parents {
            let parent = usize::try_from(*parent).map_err(|_| {
                GitError::InvalidFormat("commit-graph parent index overflow".into())
            })?;
            let parent_entry = graph.commits.get(parent).ok_or_else(|| {
                GitError::InvalidFormat("commit-graph parent points past commit table".into())
            })?;
            parents.push(parent_entry.oid);
        }
        if let Some(filter) = filters.filter_for_commit(idx) {
            map.insert(
                entry.oid,
                GraphBloomCommit {
                    parents,
                    filter: filter.to_vec(),
                    settings,
                },
            );
        }
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
    Ok(sley_odb::grafted_parents(
        reader,
        oid,
        Commit::parse_ref(format, &object.body)?.parents,
    ))
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
            Ok(*oid)
        }
        PeelKind::Commit => peel_to_commit(reader, format, oid),
        PeelKind::Tree => peel_to_tree(reader, format, oid),
        PeelKind::Tag => {
            let object = reader.read_object(oid)?;
            if object.object_type == ObjectType::Tag {
                Ok(*oid)
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
        return Ok(*oid);
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
        ObjectType::Tree => Ok(*oid),
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
        ObjectType::Commit => Ok(*oid),
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

// ===========================================================================
// RevWalk — the unified commit-graph traversal iterator (STAGE-A).
// ===========================================================================
//
// `RevWalk` is the single configurable seam every commit traversal in
// rev-list/log should flow through. It subsumes the previously special-cased
// `walk_commit_metadata` (plain BFS over every ancestor) and
// `walk_commit_metadata_date_ordered_limited` (commit-date priority queue with
// early stop) variants: both are now thin wrappers that build a `RevWalk` and
// collect it.
//
// STAGE-A delivers the ordering + limiting foundations:
//
//   * ordering — a priority queue keyed by the configured [`RevWalkOrder`]
//     (commit-date default, author-date, or topo). Commit-date order is
//     byte-identical to the previous `..._date_ordered_limited` heap, so the
//     existing passing rev-list/log ordering cells are preserved exactly.
//   * limiting — `--max-count`/`-n`, `--skip`, `--since`/`--max-age` (lower
//     committer-time bound) and `--until`/`--min-age` (upper bound), and
//     `--first-parent`.
//   * a [`Pathspec`](sley_pathspec::Pathspec) slot, wired in but NOT yet used
//     to prune (TREESAME / history simplification is STAGE-B). It is carried so
//     the seam is in place and a pathspec round-trips through the builder.
//
// What is deliberately NOT here (reported as remaining):
//   * TREESAME / pathspec-limited history simplification (`--simplify-merges`,
//     `--full-history`, default parent-rewriting) — STAGE-B.
//   * `--graph` ASCII topology rendering — STAGE-C.

pub use sley_pathspec::{Pathspec, PathspecMatchMagic};

/// Commit ordering for a [`RevWalk`].
///
/// `CommitDate` (the default) reproduces git's default newest-committer-date
/// priority-queue order; `AuthorDate` keys on the author timestamp; `Topo`
/// yields a strict topological order (no parent emitted before all its
/// children). For STAGE-A, `Topo`'s final linearization is applied by the
/// caller's existing topo post-sort; the walk itself collects the reachable
/// set the post-sort consumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RevWalkOrder {
    /// Newest committer date first (git default). Byte-identical to the old
    /// `walk_commit_metadata_date_ordered_limited` heap.
    #[default]
    CommitDate,
    /// Newest author date first.
    AuthorDate,
    /// Topological order (children before parents).
    Topo,
}

/// Inclusive committer-time window for `--since`/`--until`/`--max-age`/
/// `--min-age` limiting. `None` bounds are open.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RevWalkDateWindow {
    /// Lower bound (`--since` / `--max-age`): commits older than this are
    /// dropped, and the walk stops descending past them.
    pub min_time: Option<i64>,
    /// Upper bound (`--until` / `--min-age`): commits newer than this are
    /// dropped from output (but the walk continues past them to reach older
    /// commits within the window).
    pub max_time: Option<i64>,
}

impl RevWalkDateWindow {
    fn is_open(&self) -> bool {
        self.min_time.is_none() && self.max_time.is_none()
    }
}

/// Configurable commit-graph traversal — the unified rev-walk seam.
///
/// Build with [`RevWalk::new`], tune with the chained setters, then drive it as
/// an iterator (it yields [`CommitMetadata`]). Construction loads nothing; the
/// commit-graph is read lazily on the first `next()` and reused for the walk.
pub struct RevWalk<'a, R: ObjectReader> {
    graph: CommitGraphContext<'a>,
    reader: &'a R,
    format: ObjectFormat,
    starts: Vec<ObjectId>,
    order: RevWalkOrder,
    first_parent: bool,
    max_count: Option<usize>,
    skip: usize,
    window: RevWalkDateWindow,
    pathspec: Pathspec,

    // Traversal state, initialized on the first `next()`.
    started: bool,
    seen: HashSet<ObjectId>,
    heap: std::collections::BinaryHeap<RevWalkHeapEntry>,
    records: HashMap<ObjectId, CommitMetadata>,
    emitted: usize,
    skipped: usize,
}

/// Heap entry ordered so `BinaryHeap::pop` returns the commit the configured
/// order wants emitted next. For date orders the key is `(time, Reverse(oid))`
/// — newest first, ties broken by *smaller* oid (matching the old heap's
/// `(commit_time, Reverse(oid))`).
struct RevWalkHeapEntry {
    key: i64,
    oid: ObjectId,
}

impl PartialEq for RevWalkHeapEntry {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key && self.oid == other.oid
    }
}
impl Eq for RevWalkHeapEntry {}
impl Ord for RevWalkHeapEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Max-heap pops the greatest. We want newest time first; for equal
        // times, the SMALLER oid first — so reverse the oid comparison.
        self.key
            .cmp(&other.key)
            .then_with(|| other.oid.cmp(&self.oid))
    }
}
impl PartialOrd for RevWalkHeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<'a, R: ObjectReader> RevWalk<'a, R> {
    /// Start a walk from `starts` over the commit-graph at `git_dir`.
    pub fn new(
        git_dir: &'a Path,
        format: ObjectFormat,
        reader: &'a R,
        starts: impl IntoIterator<Item = ObjectId>,
    ) -> Self {
        Self {
            graph: CommitGraphContext::load(git_dir, format),
            reader,
            format,
            starts: starts.into_iter().collect(),
            order: RevWalkOrder::default(),
            first_parent: false,
            max_count: None,
            skip: 0,
            window: RevWalkDateWindow::default(),
            pathspec: Pathspec::default(),
            started: false,
            seen: HashSet::new(),
            heap: std::collections::BinaryHeap::new(),
            records: HashMap::new(),
            emitted: 0,
            skipped: 0,
        }
    }

    /// Set the commit ordering.
    pub fn order(mut self, order: RevWalkOrder) -> Self {
        self.order = order;
        self
    }

    /// Follow only the first parent of each commit (`--first-parent`).
    pub fn first_parent(mut self, first_parent: bool) -> Self {
        self.first_parent = first_parent;
        self
    }

    /// Stop after emitting `max_count` commits (`--max-count`/`-n`). Combined
    /// with [`skip`](Self::skip): `skip` commits are dropped first, then up to
    /// `max_count` are yielded.
    pub fn max_count(mut self, max_count: Option<usize>) -> Self {
        self.max_count = max_count;
        self
    }

    /// Drop the first `skip` commits before yielding (`--skip`).
    pub fn skip(mut self, skip: usize) -> Self {
        self.skip = skip;
        self
    }

    /// Limit to a committer-time window (`--since`/`--until`/`--max-age`/
    /// `--min-age`).
    pub fn date_window(mut self, window: RevWalkDateWindow) -> Self {
        self.window = window;
        self
    }

    /// Attach a pathspec. STAGE-A carries it for the seam; it does not yet
    /// prune the walk (TREESAME simplification is STAGE-B).
    pub fn pathspec(mut self, pathspec: Pathspec) -> Self {
        self.pathspec = pathspec;
        self
    }

    /// The pathspec attached to this walk (empty if none).
    pub fn pathspec_ref(&self) -> &Pathspec {
        &self.pathspec
    }

    /// Priority-queue key for `metadata` under the active order.
    ///
    /// `CommitMetadata` carries only committer time (the value the commit-graph
    /// records), so every order keys on it in STAGE-A. `AuthorDate` is wired as
    /// a distinct order so callers can request it; until the metadata fast-path
    /// records author time it degrades to committer time, and a caller needing
    /// strict author-date ordering linearizes full [`CommitRecord`]s instead.
    /// `Topo` likewise uses committer time as the heap key — the strict
    /// topological linearization is applied by the caller's topo post-sort over
    /// the collected set (STAGE-A keeps that post-sort as the proven path).
    fn order_key(&self, metadata: &CommitMetadata) -> i64 {
        let _ = self.order;
        metadata.commit_time
    }

    fn push(&mut self, metadata: CommitMetadata) {
        let key = self.order_key(&metadata);
        let oid = metadata.oid;
        self.records.insert(oid, metadata);
        self.heap.push(RevWalkHeapEntry { key, oid });
    }

    fn init(&mut self) -> Result<()> {
        let starts = std::mem::take(&mut self.starts);
        for start in starts {
            if !self.seen.insert(start) {
                continue;
            }
            let metadata =
                commit_metadata_lookup(&mut self.graph, self.reader, self.format, &start)?;
            self.push(metadata);
        }
        self.started = true;
        Ok(())
    }

    fn enqueue_parents(&mut self, metadata: &CommitMetadata) -> Result<()> {
        let parents: Vec<ObjectId> = if self.first_parent {
            metadata.parents.first().cloned().into_iter().collect()
        } else {
            metadata.parents.clone()
        };
        for parent in parents {
            if !self.seen.insert(parent) {
                continue;
            }
            let parent_metadata =
                commit_metadata_lookup(&mut self.graph, self.reader, self.format, &parent)?;
            self.push(parent_metadata);
        }
        Ok(())
    }

    /// Advance the walk by one commit, returning the next [`CommitMetadata`] in
    /// the configured order (after skip/limit/date-window filtering), or `None`
    /// when the walk is exhausted.
    pub fn try_next(&mut self) -> Result<Option<CommitMetadata>> {
        if !self.started {
            self.init()?;
        }
        loop {
            if let Some(max) = self.max_count
                && self.emitted >= max
            {
                return Ok(None);
            }
            let Some(entry) = self.heap.pop() else {
                return Ok(None);
            };
            let Some(metadata) = self.records.get(&entry.oid).cloned() else {
                continue;
            };
            // Descend regardless of the date window's upper bound: a commit
            // newer than `--until` is dropped from output but its ancestors
            // may still fall in-window. The lower bound, however, prunes the
            // descent — nothing older than `--since` can have in-window
            // ancestors (committer time is non-increasing along ancestry only
            // approximately, but git applies the same descent cutoff).
            let within_lower = self
                .window
                .min_time
                .is_none_or(|min| metadata.commit_time >= min);
            if within_lower {
                self.enqueue_parents(&metadata)?;
            }
            // Output filtering: both window bounds gate emission.
            let emit = self.window.is_open()
                || (self
                    .window
                    .min_time
                    .is_none_or(|min| metadata.commit_time >= min)
                    && self
                        .window
                        .max_time
                        .is_none_or(|max| metadata.commit_time <= max));
            if !emit {
                continue;
            }
            if self.skipped < self.skip {
                self.skipped += 1;
                continue;
            }
            self.emitted += 1;
            return Ok(Some(metadata));
        }
    }

    /// Collect the full walk into a `Vec`, honoring all configured limits.
    pub fn collect_all(mut self) -> Result<Vec<CommitMetadata>> {
        let mut out = Vec::new();
        while let Some(metadata) = self.try_next()? {
            out.push(metadata);
        }
        Ok(out)
    }
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
        if !seen.insert(oid) {
            continue;
        }
        let metadata = commit_metadata_lookup(&mut graph, reader, format, &oid)?;
        // `--first-parent` follows only the first parent of each commit; otherwise
        // every parent is enqueued (matching `walk_commits`).
        if first_parent {
            pending.extend(metadata.parents.first().cloned());
        } else {
            pending.extend(metadata.parents.iter().cloned());
        }
        out.push(metadata);
    }
    Ok(out)
}

/// Walk history in committer-date order, stopping after `limit` commits. This is
/// the early-stop counterpart of walking every ancestor and then sorting for
/// `rev-list`/`log -n`.
///
/// Now a thin wrapper over [`RevWalk`] in [`RevWalkOrder::CommitDate`]: the
/// `(commit_time, Reverse(oid))` priority order is reproduced byte-identically
/// by the unified iterator, so the existing rev-list/log `-n` ordering cells
/// are preserved.
pub fn walk_commit_metadata_date_ordered_limited<R: ObjectReader>(
    git_dir: &Path,
    format: sley_core::ObjectFormat,
    reader: &R,
    starts: impl IntoIterator<Item = ObjectId>,
    first_parent: bool,
    limit: usize,
) -> Result<Vec<CommitMetadata>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    RevWalk::new(git_dir, format, reader, starts)
        .order(RevWalkOrder::CommitDate)
        .first_parent(first_parent)
        .max_count(Some(limit))
        .collect_all()
}

fn commit_metadata_lookup<R: ObjectReader>(
    graph: &mut CommitGraphContext,
    reader: &R,
    format: sley_core::ObjectFormat,
    oid: &ObjectId,
) -> Result<CommitMetadata> {
    let (parents, commit_time) = match graph.metadata(oid) {
        Some(metadata) => metadata,
        None => commit_metadata_from_object(reader, format, oid)?,
    };
    Ok(CommitMetadata {
        oid: *oid,
        parents: sley_odb::grafted_parents(reader, oid, parents),
        commit_time,
    })
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
    Ok((sley_odb::grafted_parents(reader, oid, commit.parents), commit_time))
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
        if !seen.insert(oid) {
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
        let parents = sley_odb::grafted_parents(reader, &oid, commit.parents.clone());
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
// TREESAME / pathspec-limited history simplification (STAGE-B)
//
// Faithful port of the subset of git's revision.c history-simplification needed
// for pathspec-limited `log`/`rev-list`: per-commit TREESAME classification
// (`try_to_simplify_commit`/`rev_compare_tree`), the default simplification that
// follows only the TREESAME parent and drops unchanged commits, `--full-history`
// (keep every commit that touches the paths plus the merges that join them), and
// parent rewriting (`rewrite_parents`/`rewrite_one`).
// ---------------------------------------------------------------------------

/// Flags controlling history simplification, mirroring the relevant `rev_info`
/// fields. `--simplify-merges`, `--show-pulls`, and `--ancestry-path` are
/// STAGE-C; this struct carries the STAGE-B subset.
#[derive(Debug, Clone, Copy, Default)]
pub struct SimplifyOptions {
    /// `--full-history`: keep every commit whose limited tree-diff is non-empty
    /// against *any* parent (and the merges that join those lines), rather than
    /// the default which follows a single TREESAME parent.
    pub full_history: bool,
    /// `--first-parent`: TREESAME is computed only against the first parent, and
    /// rewriting follows only the first parent.
    pub first_parent: bool,
}

/// Per-commit simplification flags computed during the TREESAME pass.
#[derive(Debug, Clone, Default)]
struct CommitSimplify {
    /// git's `TREESAME` object flag: the commit does not change any pathspec-
    /// matched path relative to its relevant parent(s).
    treesame: bool,
    /// The parent list after default-mode diversion. In `try_to_simplify_commit`,
    /// when a merge is REV_TREE_SAME to one of its parents (and we are doing
    /// dense, non-`--full-history` simplification), git truncates the parent list
    /// to *just that parent* and diverts the whole walk down it — the other merge
    /// sides are discarded. `None` means "use the commit's real parents" (no
    /// diversion happened); `Some(list)` is the diverted (single-parent) list.
    simplified_parents: Option<Vec<ObjectId>>,
}

/// Resolve a commit's tree oid, preferring the already-parsed record.
fn commit_tree_oid(record: &CommitRecord) -> ObjectId {
    record.commit.tree
}

/// git's `rev_compare_tree` reduced to the SAME/!SAME decision the default and
/// `--full-history` simplifications need: are `parent_tree` and `commit_tree`
/// identical across every path the pathspec matches?
///
/// Mirrors `diff_tree_oid` limited by the pathspec: we diff the two trees
/// (rename-blind, exactly as git's pruning diff is) and report SAME iff no
/// changed path is matched by the pathspec. An empty pathspec matches every
/// path, so it reduces to "are the trees equal".
fn tree_same_for_pathspec(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    parent_tree: &ObjectId,
    commit_tree: &ObjectId,
    pathspec: &Pathspec,
) -> Result<bool> {
    if parent_tree == commit_tree {
        return Ok(true);
    }
    // Rename-blind name-status diff — git's pruning diff never detects renames.
    let options = sley_diff_merge::DiffNameStatusOptions {
        detect_renames: false,
        detect_copies: false,
        find_copies_harder: false,
        rename_empty: false,
    };
    let changes = sley_diff_merge::diff_name_status_trees_with_options(
        db,
        format,
        parent_tree,
        commit_tree,
        options,
    )?;
    for entry in &changes {
        if pathspec.is_empty() || pathspec.matches(entry.path.as_bytes()) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// git's `rev_same_tree_as_empty` for the pathspec subset: is `commit_tree`
/// empty of every pathspec-matched path (i.e. a root commit adds nothing the
/// pathspec cares about)?
fn tree_same_as_empty_for_pathspec(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    commit_tree: &ObjectId,
    pathspec: &Pathspec,
) -> Result<bool> {
    let options = sley_diff_merge::DiffNameStatusOptions {
        detect_renames: false,
        detect_copies: false,
        find_copies_harder: false,
        rename_empty: false,
    };
    let changes = sley_diff_merge::diff_name_status_empty_tree_with_options(
        db,
        format,
        commit_tree,
        options,
    )?;
    for entry in &changes {
        if pathspec.is_empty() || pathspec.matches(entry.path.as_bytes()) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn commit_graph_bloom_paths_for_pathspec(pathspec: &Pathspec) -> Option<Vec<Vec<u8>>> {
    if pathspec.is_empty() {
        return None;
    }
    let mut paths = Vec::new();
    for element in pathspec.elements() {
        let mut pattern = element.pattern();
        if element.is_exclude()
            || element.is_icase()
            || element.is_glob()
            || !element.attrs().is_empty()
            || pattern.is_empty()
            || pattern.iter().any(|byte| matches!(*byte, b'*' | b'?' | b'[' | b'\\'))
        {
            return None;
        }
        while pattern.ends_with(b"/") {
            pattern = &pattern[..pattern.len() - 1];
        }
        if pattern.is_empty() {
            return None;
        }
        paths.push(pattern.to_vec());
    }
    (!paths.is_empty()).then_some(paths)
}

fn commit_graph_bloom_read_changed_paths_enabled(objects_dir: &Path) -> bool {
    let Some(git_dir) = objects_dir.parent() else {
        return true;
    };
    sley_config::read_repo_config(git_dir, None)
        .ok()
        .and_then(|config| config.get_bool("commitGraph", None, "readChangedPaths"))
        .unwrap_or(true)
}

fn commit_graph_bloom_consult(
    blooms: &HashMap<ObjectId, GraphBloomCommit>,
    commit: &ObjectId,
    parent: Option<&ObjectId>,
    paths: &[Vec<u8>],
) -> GraphBloomConsult {
    let Some(bloom) = blooms.get(commit) else {
        return GraphBloomConsult::NotPresent;
    };
    match parent {
        Some(parent) => {
            if bloom.parents.first() != Some(parent) {
                return GraphBloomConsult::NotPresent;
            }
        }
        None => {
            if !bloom.parents.is_empty() {
                return GraphBloomConsult::NotPresent;
            }
        }
    }
    let maybe_changed = paths.iter().any(|path| {
        sley_formats::commit_graph_bloom_filter_contains(&bloom.filter, path, bloom.settings)
    });
    if maybe_changed {
        GraphBloomConsult::Maybe
    } else {
        GraphBloomConsult::DefinitelyNot
    }
}

/// Compute the `TREESAME` flag for every commit in `records`, limited by
/// `pathspec`. `reachable` is the set of oids in `records` so we can tell a
/// "relevant" (on-graph) parent from a boundary one — git's `relevant_commit`.
///
/// Faithful to `try_to_simplify_commit`'s dense-mode logic: a root commit is
/// TREESAME iff it adds no pathspec-matched path; a single-parent commit is
/// TREESAME iff its tree-diff against the parent is empty for the pathspec; a
/// merge is TREESAME iff it is SAME to its relevant parent(s) (irrelevant —
/// off-graph — parents cannot make it !TREESAME when any relevant parent
/// exists).
fn compute_treesame(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    records: &[CommitRecord],
    reachable: &HashSet<ObjectId>,
    pathspec: &Pathspec,
    first_parent: bool,
    full_history: bool,
) -> Result<HashMap<ObjectId, CommitSimplify>> {
    // O(1) tree lookup for on-graph commits.
    let tree_by_oid: HashMap<ObjectId, ObjectId> =
        records.iter().map(|r| (r.oid, r.commit.tree)).collect();
    let parent_tree = |oid: &ObjectId| -> Option<ObjectId> {
        if let Some(tree) = tree_by_oid.get(oid) {
            Some(*tree)
        } else {
            read_commit_tree(db, format, oid).ok()
        }
    };
    let bloom_paths = commit_graph_bloom_paths_for_pathspec(pathspec)
        .filter(|_| commit_graph_bloom_read_changed_paths_enabled(db.objects_dir()));
    let bloom_map = bloom_paths
        .as_ref()
        .map(|_| load_commit_graph_bloom_map(db.objects_dir(), format))
        .unwrap_or_default();
    let mut bloom_stats = GraphBloomStats::default();

    let mut out = HashMap::with_capacity(records.len());
    for record in records {
        let commit_tree = commit_tree_oid(record);
        let mut simplify = CommitSimplify::default();
        if record.parents.is_empty() {
            simplify.treesame = if let Some(paths) = bloom_paths.as_ref() {
                match commit_graph_bloom_consult(&bloom_map, &record.oid, None, paths) {
                    GraphBloomConsult::DefinitelyNot => {
                        bloom_stats.definitely_not += 1;
                        true
                    }
                    GraphBloomConsult::Maybe => {
                        bloom_stats.maybe += 1;
                        let same = tree_same_as_empty_for_pathspec(db, format, &commit_tree, pathspec)?;
                        if same {
                            bloom_stats.false_positive += 1;
                        }
                        same
                    }
                    GraphBloomConsult::NotPresent => {
                        bloom_stats.filter_not_present += 1;
                        tree_same_as_empty_for_pathspec(db, format, &commit_tree, pathspec)?
                    }
                }
            } else {
                tree_same_as_empty_for_pathspec(db, format, &commit_tree, pathspec)?
            };
            out.insert(record.oid, simplify);
            continue;
        }
        // Non-merge in default (non-dense) mode is always a change. We always run
        // dense here (the pathspec / --full-history path), so fall through.
        let mut relevant_parents = 0usize;
        let mut relevant_change = false;
        let mut irrelevant_change = false;
        let mut diverted = false;
        for (nth, parent) in record.parents.iter().enumerate() {
            // `--first-parent`: do not compare against later parents (git breaks
            // out of the loop at nth_parent == 1).
            if first_parent && nth >= 1 {
                break;
            }
            let relevant = reachable.contains(parent);
            if relevant {
                relevant_parents += 1;
            }
            let Some(pt) = parent_tree(parent) else {
                // Missing parent tree → REV_TREE_NEW (a difference).
                if relevant {
                    relevant_change = true;
                } else {
                    irrelevant_change = true;
                }
                continue;
            };
            let same = if nth == 0
                && let Some(paths) = bloom_paths.as_ref()
            {
                match commit_graph_bloom_consult(&bloom_map, &record.oid, Some(parent), paths) {
                    GraphBloomConsult::DefinitelyNot => {
                        bloom_stats.definitely_not += 1;
                        true
                    }
                    GraphBloomConsult::Maybe => {
                        bloom_stats.maybe += 1;
                        let same = tree_same_for_pathspec(db, format, &pt, &commit_tree, pathspec)?;
                        if same {
                            bloom_stats.false_positive += 1;
                        }
                        same
                    }
                    GraphBloomConsult::NotPresent => {
                        bloom_stats.filter_not_present += 1;
                        tree_same_for_pathspec(db, format, &pt, &commit_tree, pathspec)?
                    }
                }
            } else {
                tree_same_for_pathspec(db, format, &pt, &commit_tree, pathspec)?
            };
            if same {
                // try_to_simplify_commit: REV_TREE_SAME. In dense, non-full-
                // history mode, if this parent is relevant (or we keep
                // simplify_history on), git truncates the parent list to this
                // single parent, marks TREESAME, and diverts. We only divert in
                // the default (non-full-history) mode.
                if !full_history && relevant {
                    simplify.simplified_parents = Some(vec![*parent]);
                    simplify.treesame = true;
                    diverted = true;
                    break;
                }
                // full-history (or irrelevant): keep going, do not divert.
                continue;
            }
            if relevant {
                relevant_change = true;
            } else {
                irrelevant_change = true;
            }
        }
        if !diverted {
            // git: if we have any relevant parents, TREESAME considers only them;
            // otherwise it falls back to the irrelevant ones.
            simplify.treesame = if relevant_parents > 0 {
                !relevant_change
            } else {
                !irrelevant_change
            };
        }
        out.insert(record.oid, simplify);
    }
    if bloom_paths.is_some()
        && (bloom_stats.filter_not_present > 0
            || bloom_stats.maybe > 0
            || bloom_stats.definitely_not > 0
            || bloom_stats.false_positive > 0)
    {
        sley_core::trace2::bloom_statistics(
            bloom_stats.filter_not_present,
            bloom_stats.maybe,
            bloom_stats.definitely_not,
            bloom_stats.false_positive,
        );
    }
    Ok(out)
}

/// Read a commit's tree oid directly from the object store (for off-graph
/// parents not present as a `CommitRecord`).
fn read_commit_tree(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    oid: &ObjectId,
) -> Result<ObjectId> {
    let object = db.read_object(oid)?;
    if object.object_type != ObjectType::Commit {
        return Err(GitError::InvalidObject(format!(
            "expected commit {oid}, found {}",
            object.object_type.as_str()
        )));
    }
    Ok(Commit::parse_ref(format, &object.body)?.tree)
}

/// git's `one_relevant_parent`: pick the single parent a TREESAME commit can be
/// simplified onto, or `None` if there is no unique relevant parent.
fn one_relevant_parent<'a>(
    parents: &'a [ObjectId],
    reachable: &HashSet<ObjectId>,
    record_oids: &HashSet<ObjectId>,
    first_parent: bool,
) -> Option<&'a ObjectId> {
    if parents.is_empty() {
        return None;
    }
    if first_parent || parents.len() == 1 {
        return parents.first();
    }
    let mut relevant: Option<&ObjectId> = None;
    for parent in parents {
        let is_relevant = reachable.contains(parent) || record_oids.contains(parent);
        if is_relevant {
            if relevant.is_some() {
                return None;
            }
            relevant = Some(parent);
        }
    }
    relevant
}

/// git's `rewrite_one`: follow a chain of TREESAME commits to the first ancestor
/// that is either !TREESAME (a real change), a root with no parents, or a commit
/// without a unique relevant parent. Returns that rewritten parent oid, or
/// `None` when the chain dead-ends at a root (the parent edge is dropped).
fn rewrite_one(
    start: &ObjectId,
    simplify: &HashMap<ObjectId, CommitSimplify>,
    parents_of: &HashMap<ObjectId, Vec<ObjectId>>,
    reachable: &HashSet<ObjectId>,
    record_oids: &HashSet<ObjectId>,
    first_parent: bool,
) -> Option<ObjectId> {
    let mut current = *start;
    loop {
        let ts = simplify.get(&current).map(|s| s.treesame).unwrap_or(false);
        if !ts {
            return Some(current);
        }
        let Some(parents) = parents_of.get(&current) else {
            // Off-graph; treat as a real boundary (keep it).
            return Some(current);
        };
        if parents.is_empty() {
            // rewrite_one_noparents: the edge is dropped.
            return None;
        }
        match one_relevant_parent(parents, reachable, record_oids, first_parent) {
            Some(parent) => current = *parent,
            None => return Some(current),
        }
    }
}

/// Apply pathspec-limited default / `--full-history` simplification to an ordered
/// reachable commit set, returning the records to display with their parents
/// rewritten past simplified-away commits.
///
/// `records` must already be in the desired output order (date or topo). The
/// returned records preserve that order, filtered and parent-rewritten.
pub fn simplify_history(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    records: Vec<CommitRecord>,
    pathspec: &Pathspec,
    options: SimplifyOptions,
) -> Result<Vec<CommitRecord>> {
    if pathspec.is_empty() {
        // Without a pathspec there is nothing to prune: every commit "changes"
        // the (whole) tree, so TREESAME is never set and no simplification
        // applies. `--full-history` only differs from the default *in the
        // presence of a pathspec* (it keeps the merges that join the matching
        // lines); with no pathspec it is a no-op. git's `prune` flag is off when
        // `prune_data` is empty, so it never runs `try_to_simplify_commit`.
        return Ok(records);
    }
    let reachable: HashSet<ObjectId> = records.iter().map(|r| r.oid).collect();
    let record_oids = reachable.clone();
    let simplify = compute_treesame(
        db,
        format,
        &records,
        &reachable,
        pathspec,
        options.first_parent,
        options.full_history,
    )?;

    // Effective parent list for each commit: the diverted single parent when
    // default-mode simplification truncated a merge, else the real parents
    // (first-parent-limited when requested).
    let effective_parents = |oid: &ObjectId, real: &[ObjectId]| -> Vec<ObjectId> {
        if let Some(div) = simplify
            .get(oid)
            .and_then(|s| s.simplified_parents.as_ref())
        {
            return div.clone();
        }
        if options.first_parent {
            real.iter().take(1).cloned().collect()
        } else {
            real.to_vec()
        }
    };
    let parents_of: HashMap<ObjectId, Vec<ObjectId>> = records
        .iter()
        .map(|r| (r.oid, effective_parents(&r.oid, &r.parents)))
        .collect();

    // Re-derive reachability following the *effective* (diverted) parent edges,
    // starting from the tips — commits in the set that are not an effective
    // parent of any other commit. In default mode this is what drops the
    // pruned-away merge sides: a side branch only reachable through a diverted
    // merge edge is never visited.
    let is_effective_parent: HashSet<ObjectId> = parents_of
        .values()
        .flat_map(|ps| ps.iter().copied())
        .collect();
    let tips: Vec<ObjectId> = records
        .iter()
        .map(|r| r.oid)
        .filter(|oid| !is_effective_parent.contains(oid))
        .collect();
    let mut live: HashSet<ObjectId> = HashSet::new();
    let mut stack = tips;
    while let Some(oid) = stack.pop() {
        if !live.insert(oid) {
            continue;
        }
        if let Some(ps) = parents_of.get(&oid) {
            for p in ps {
                if record_oids.contains(p) && !live.contains(p) {
                    stack.push(*p);
                }
            }
        }
    }

    let mut out = Vec::with_capacity(records.len());
    for record in records {
        // Only commits still reachable after diversion are candidates.
        if !live.contains(&record.oid) {
            continue;
        }
        let ts = simplify
            .get(&record.oid)
            .map(|s| s.treesame)
            .unwrap_or(false);
        let effective = parents_of
            .get(&record.oid)
            .cloned()
            .unwrap_or_else(|| record.parents.clone());
        let is_merge = effective.len() > 1;

        // Default simplification: show a commit iff it is !TREESAME. With
        // --full-history every non-TREESAME commit is shown AND merges are kept
        // even when TREESAME (so the joined lines stay connected).
        let show = if options.full_history {
            !ts || is_merge
        } else {
            !ts
        };
        if !show {
            continue;
        }

        // Rewrite parents past simplified-away (TREESAME) commits.
        let mut new_parents: Vec<ObjectId> = Vec::with_capacity(effective.len());
        let mut seen_parent: HashSet<ObjectId> = HashSet::new();
        for parent in &effective {
            if let Some(rewritten) = rewrite_one(
                parent,
                &simplify,
                &parents_of,
                &reachable,
                &record_oids,
                options.first_parent,
            ) {
                // Drop duplicate parents introduced by rewriting (git's
                // remove_duplicate_parents collapses these).
                if seen_parent.insert(rewritten) {
                    new_parents.push(rewritten);
                }
            }
        }
        out.push(CommitRecord {
            oid: record.oid,
            parents: new_parents,
            commit: record.commit,
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
    pub name: BString,
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
        .ok_or_else(|| GitError::not_found(format!("path '{path}' does not exist in '{rev}'")))
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
    let mut current = *tree_oid;
    // Split on '/', skipping empty components so leading/trailing/duplicate
    // separators ("a//b", "/a", "dir/") behave the way git's pathspec does.
    let components: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    if components.is_empty() {
        return Some(ResolvedTreePath {
            oid: current,
            mode: None,
            object_type: ObjectType::Tree,
            name: BString::default(),
        });
    }
    let last = components.len() - 1;
    for (idx, component) in components.iter().enumerate() {
        let object = reader.read_object(&current).ok()?;
        if object.object_type != ObjectType::Tree {
            // Cannot descend through a blob (or anything non-tree).
            return None;
        }
        let mut found = None;
        for entry in TreeEntries::new(format, &object.body) {
            let entry = entry.ok()?;
            if found.is_none() && entry.name == component.as_bytes() {
                found = Some((entry.mode, entry.oid, entry.name.into()));
            }
        }
        let (mode, oid, name) = found?;
        let object_type = sley_object::tree_entry_object_type(mode);
        if idx == last {
            return Some(ResolvedTreePath {
                oid,
                mode: Some(mode),
                object_type,
                name,
            });
        }
        // Intermediate component must itself be a tree to keep descending.
        if object_type != ObjectType::Tree {
            return None;
        }
        current = oid;
    }
    None
}

/// Outcome of a `--follow-symlinks` tree-path walk (upstream's
/// `get_tree_entry_follow_symlinks`, tree-walk.c).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SymlinkedTreePath {
    /// The walk ended on an in-repo object (the symlink chain — if any —
    /// resolved to a blob or tree inside the repository).
    Found(ObjectId),
    /// The symlink chain leaves the repository: an absolute link target, or
    /// `..` escaping past the root. The unresolvable remainder of the link
    /// path is reported verbatim (upstream sets `*mode = 0` and fills
    /// `result_path`).
    OutOfRepo(Vec<u8>),
    /// A path component was not found before any symlink had been followed
    /// (upstream `MISSING_OBJECT`).
    Missing,
    /// A path component was not found after at least one symlink had been
    /// followed (upstream `DANGLING_SYMLINK`).
    Dangling,
    /// More than [`FOLLOW_SYMLINKS_MAX_LINKS`] symlinks were followed
    /// (upstream `SYMLINK_LOOP`).
    Loop,
    /// A non-final path component resolved to a regular file (upstream
    /// `NOT_DIR`).
    NotDir,
}

/// Linux's built-in cap on the number of symlinks to follow; upstream adopts
/// the same value (`GET_TREE_ENTRY_FOLLOW_SYMLINKS_MAX_LINKS`).
pub const FOLLOW_SYMLINKS_MAX_LINKS: u32 = 40;

/// Resolve `<rev>:<path>` like [`resolve_rev_path_entry`], but follow in-tree
/// symlinks the way `git cat-file --follow-symlinks` does (upstream
/// `get_tree_entry_follow_symlinks`). Failures on the `<rev>` side (unknown
/// revision, non-treeish) report [`SymlinkedTreePath::Missing`], matching
/// upstream where a failed treeish lookup yields `MISSING_OBJECT`.
pub fn resolve_rev_path_follow_symlinks<R: ObjectReader>(
    git_dir: &Path,
    format: ObjectFormat,
    reader: &R,
    rev: &str,
    path: &str,
) -> SymlinkedTreePath {
    let Ok(rev_oid) = resolve_revision_with_reader(git_dir, format, reader, rev) else {
        return SymlinkedTreePath::Missing;
    };
    resolve_tree_path_follow_symlinks(reader, format, &rev_oid, path)
}

/// Walk `path` within the tree of `treeish` (peeled as needed), following
/// symlink entries. This mirrors upstream's `get_tree_entry_follow_symlinks`
/// loop: path components are consumed left to right against a stack of parent
/// trees rooted at the repository root; a symlink entry splices its target in
/// front of the remaining path; `..` pops a parent (escaping the root reports
/// the remainder as out-of-repo, like an absolute link target does).
pub fn resolve_tree_path_follow_symlinks<R: ObjectReader>(
    reader: &R,
    format: ObjectFormat,
    treeish: &ObjectId,
    path: &str,
) -> SymlinkedTreePath {
    // Stack of (tree oid, tree object) from the root down to the directory
    // currently being walked. Lookups always run against the top entry.
    let mut parents: Vec<(ObjectId, Arc<EncodedObject>)> = Vec::new();
    let mut namebuf: Vec<u8> = path.as_bytes().to_vec();
    let mut current_oid = *treeish;
    let mut follows_remaining = FOLLOW_SYMLINKS_MAX_LINKS;
    // Once a symlink has been followed, a failed lookup is a dangling link
    // rather than a missing path (upstream flips `retval` the same way).
    let mut followed_symlink = false;
    let mut need_load = true;

    loop {
        let fail = if followed_symlink {
            SymlinkedTreePath::Dangling
        } else {
            SymlinkedTreePath::Missing
        };

        if need_load {
            let Ok(tree_oid) = peel_to_tree(reader, format, &current_oid) else {
                return fail;
            };
            let Ok(object) = reader.read_object(&tree_oid) else {
                return fail;
            };
            if object.object_type != ObjectType::Tree {
                return fail;
            }
            parents.push((tree_oid, object));
            if namebuf.is_empty() {
                // `<rev>:` (or a symlink chain that consumed the whole path)
                // names the tree just loaded.
                return SymlinkedTreePath::Found(tree_oid);
            }
            if parents.last().is_some_and(|(_, object)| object.body.is_empty()) {
                return fail;
            }
            need_load = false;
        }

        // Handle symlinks to e.g. `a//b` by removing leading slashes.
        while namebuf.first() == Some(&b'/') {
            namebuf.remove(0);
        }

        // Split namebuf into a first component and an optional remainder.
        let slash = namebuf.iter().position(|&byte| byte == b'/');
        let (component_len, has_remainder) = match slash {
            Some(index) => (index, true),
            None => (namebuf.len(), false),
        };

        // `..` can appear in namebuf when a symlink target contains it.
        if &namebuf[..component_len] == b".." {
            if parents.len() == 1 {
                // `..` at the repository root: the rest of the path (the
                // `..` included) escapes the repository.
                return SymlinkedTreePath::OutOfRepo(namebuf);
            }
            parents.pop();
            namebuf.drain(..if has_remainder { 3 } else { 2 });
            continue;
        }

        // A symlink to `dir/..` leaves an empty path: the current tree.
        if component_len == 0 {
            let Some((tree_oid, _)) = parents.last() else {
                return fail;
            };
            return SymlinkedTreePath::Found(*tree_oid);
        }

        // Look up the first (or only) path component in the current tree.
        let mut found = None;
        if let Some((_, object)) = parents.last() {
            for entry in TreeEntries::new(format, &object.body) {
                let Ok(entry) = entry else {
                    return fail;
                };
                if entry.name == &namebuf[..component_len] {
                    found = Some((entry.mode, entry.oid));
                    break;
                }
            }
        }
        let Some((mode, oid)) = found else {
            return fail;
        };

        match mode & 0o170000 {
            0o040000 => {
                // Directory: done if it is the last component, else descend.
                if !has_remainder {
                    return SymlinkedTreePath::Found(oid);
                }
                current_oid = oid;
                need_load = true;
                namebuf.drain(..component_len + 1);
            }
            0o100000 => {
                // Regular file: done if last component, otherwise the path
                // tries to descend through a non-directory.
                if !has_remainder {
                    return SymlinkedTreePath::Found(oid);
                }
                return SymlinkedTreePath::NotDir;
            }
            0o120000 => {
                // Follow a symlink.
                if follows_remaining == 0 {
                    return SymlinkedTreePath::Loop;
                }
                follows_remaining -= 1;
                followed_symlink = true;
                let Ok(link) = reader.read_object(&oid) else {
                    return SymlinkedTreePath::Dangling;
                };
                let target = link.body.clone();
                if target.first() == Some(&b'/') {
                    // An absolute link target leaves the repository; any
                    // remainder is dropped, exactly like upstream.
                    return SymlinkedTreePath::OutOfRepo(target);
                }
                // Splice the target in front of the remainder and re-walk
                // from the current directory (top of the parent stack).
                let mut spliced = target;
                if has_remainder {
                    spliced.push(b'/');
                    spliced.extend_from_slice(&namebuf[component_len + 1..]);
                }
                namebuf = spliced;
            }
            _ => {
                // Gitlink (or unknown mode): upstream's loop falls through and
                // re-scans its already-consumed tree descriptor, failing to
                // find the entry again — the walk ends missing/dangling.
                return fail;
            }
        }
    }
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
            return Err(GitError::not_found(format!(
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
            return Ok(entry.oid);
        }
    }
    if path_exists {
        Err(GitError::not_found(format!(
            "path '{path}' is in the index, but not at stage {stage}"
        )))
    } else {
        Err(GitError::not_found(format!(
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
        if !seen.insert(oid) {
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
        .ok_or_else(|| GitError::not_found(format!("no commit matching ':/{text}'")))
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
        if !seen.insert(oid) {
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
        current = if reader.is_shallow_graft(&oid) {
            None
        } else {
            match graph.first_parent(&oid) {
                Some(parent) => parent,
                None => commit.parents.into_iter().next(),
            }
        };
    }
    Err(GitError::not_found(format!(
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
        if seen.insert(commit) {
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
            if !seen.insert(oid) || self.excluded.contains(&oid) {
                continue;
            }
            if first_parent {
                pending.extend(graph.commit_first_parent(reader, &oid)?);
                out.push(oid);
                continue;
            }
            let (parents, _) = match graph.metadata(&oid) {
                Some(metadata) => metadata,
                None => commit_metadata_from_object(reader, format, &oid)?,
            };
            pending.extend(sley_odb::grafted_parents(reader, &oid, parents));
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
                    out.push(*oid);
                }
            }
            for oid in &right_set {
                if !left_set.contains(oid) {
                    out.push(*oid);
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
            resolved.starts.push(left_oid);
            resolved.starts.push(right_oid);
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
    let mut pending = VecDeque::from([*start]);
    while let Some(oid) = pending.pop_front() {
        if !seen.insert(oid) {
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
    let mut pending = VecDeque::from([*descendant]);
    while let Some(oid) = pending.pop_front() {
        if !seen.insert(oid) {
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
    let mut pending = VecDeque::from([(*start, 0usize)]);
    while let Some((oid, depth)) = pending.pop_front() {
        if depths.get(&oid).is_some_and(|existing| *existing <= depth) {
            continue;
        }
        depths.insert(oid, depth);
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
        .expect("test operation should succeed");
        fs::write(git_dir.join("HEAD"), b"ref: refs/heads/main\n")
            .expect("test operation should succeed");
        let refs = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let mut tx = refs.transaction();
        tx.update(RefUpdate {
            name: "refs/heads/main".into(),
            expected: None,
            new: RefTarget::Direct(oid),
            reflog: None,
        });
        tx.update(RefUpdate {
            name: "refs/tags/v1.0".into(),
            expected: None,
            new: RefTarget::Direct(oid),
            reflog: None,
        });
        tx.commit().expect("test operation should succeed");
        assert_eq!(
            resolve_revision(&git_dir, ObjectFormat::Sha1, "HEAD")
                .expect("test operation should succeed"),
            oid
        );
        assert_eq!(
            resolve_revision(&git_dir, ObjectFormat::Sha1, "v1.0")
                .expect("test operation should succeed"),
            oid
        );
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn resolve_revision_supports_parent_suffixes() {
        let git_dir = temp_git_dir();
        let mut db = ObjectDatabase::new(ObjectFormat::Sha1);
        let tree = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "4b825dc642cb6eb9a060e54bf8d69288fbee4904",
        )
        .expect("test operation should succeed");
        let base = write_test_commit(&mut db, tree, Vec::new(), b"base\n");
        let first_parent = write_test_commit(&mut db, tree, vec![base], b"main\n");
        let second_parent = write_test_commit(&mut db, tree, vec![base], b"side\n");
        let merge = write_test_commit(&mut db, tree, vec![first_parent, second_parent], b"merge\n");
        assert_eq!(
            resolve_revision_with_reader(&git_dir, ObjectFormat::Sha1, &db, &format!("{merge}^"))
                .expect("test operation should succeed"),
            first_parent
        );
        assert_eq!(
            resolve_revision_with_reader(&git_dir, ObjectFormat::Sha1, &db, &format!("{merge}^2"))
                .expect("test operation should succeed"),
            second_parent
        );
        assert_eq!(
            resolve_revision_with_reader(&git_dir, ObjectFormat::Sha1, &db, &format!("{merge}~2"))
                .expect("test operation should succeed"),
            base
        );
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn resolve_revision_supports_abbreviated_loose_object_ids() {
        let git_dir = temp_git_dir();
        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let oid = db
            .write_object(EncodedObject::new(ObjectType::Blob, b"abbrev\n".to_vec()))
            .expect("test operation should succeed");

        assert_eq!(
            resolve_revision(&git_dir, ObjectFormat::Sha1, &oid.to_hex()[..8])
                .expect("test operation should succeed"),
            oid
        );
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn resolve_revision_prefers_ref_over_abbreviated_object_id() {
        let git_dir = temp_git_dir();
        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let object = db
            .write_object(EncodedObject::new(
                ObjectType::Blob,
                b"abbrev conflict\n".to_vec(),
            ))
            .expect("test operation should succeed");
        let target = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed");
        let refs = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let mut tx = refs.transaction();
        tx.update(RefUpdate {
            name: format!("refs/heads/{}", &object.to_hex()[..4]),
            expected: None,
            new: RefTarget::Direct(target),
            reflog: None,
        });
        tx.commit().expect("test operation should succeed");

        assert_eq!(
            resolve_revision(&git_dir, ObjectFormat::Sha1, &object.to_hex()[..4])
                .expect("test operation should succeed"),
            target
        );
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn resolve_revision_uses_commit_graph_for_parent_suffixes() {
        let git_dir = temp_git_dir();
        fs::create_dir_all(git_dir.join("objects").join("info"))
            .expect("test operation should succeed");
        let parent = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed");
        let child = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "2222222222222222222222222222222222222222",
        )
        .expect("test operation should succeed");
        fs::write(git_dir.join("HEAD"), format!("{child}\n"))
            .expect("test operation should succeed");
        fs::write(
            git_dir.join("objects").join("info").join("commit-graph"),
            test_commit_graph(ObjectFormat::Sha1, &parent, &child),
        )
        .expect("test operation should succeed");

        struct MissingReader;
        impl ObjectReader for MissingReader {
            fn read_object(&self, oid: &ObjectId) -> Result<std::sync::Arc<EncodedObject>> {
                Err(GitError::not_found(format!(
                    "object reader should not be used for {oid}"
                )))
            }
        }

        assert_eq!(
            resolve_revision_with_reader(&git_dir, ObjectFormat::Sha1, &MissingReader, "HEAD^",)
                .expect("test operation should succeed"),
            parent
        );
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn peel_to_tree_handles_commits_and_tags() {
        let mut db = ObjectDatabase::new(ObjectFormat::Sha1);
        let tree = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "4b825dc642cb6eb9a060e54bf8d69288fbee4904",
        )
        .expect("test operation should succeed");
        db.write_object(EncodedObject::new(ObjectType::Tree, Vec::new()))
            .expect("test operation should succeed");
        let commit = write_test_commit(&mut db, tree, Vec::new(), b"base\n");
        let tag = Tag {
            object: commit,
            object_type: ObjectType::Commit,
            name: b"v1.0".to_vec(),
            tagger: Some(b"Example User <example@example.invalid> 0 +0000".to_vec()),
            message: b"release\n".to_vec(),
        };
        let tag = db
            .write_object(EncodedObject::new(ObjectType::Tag, tag.write()))
            .expect("test operation should succeed");
        assert_eq!(
            peel_to_tree(&db, ObjectFormat::Sha1, &commit).expect("test operation should succeed"),
            tree
        );
        assert_eq!(
            peel_to_tree(&db, ObjectFormat::Sha1, &tag).expect("test operation should succeed"),
            tree
        );
    }

    #[test]
    fn peel_to_commit_handles_annotated_tags() {
        let mut db = ObjectDatabase::new(ObjectFormat::Sha1);
        let tree = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "4b825dc642cb6eb9a060e54bf8d69288fbee4904",
        )
        .expect("test operation should succeed");
        db.write_object(EncodedObject::new(ObjectType::Tree, Vec::new()))
            .expect("test operation should succeed");
        let commit = write_test_commit(&mut db, tree, Vec::new(), b"base\n");
        let tag = Tag {
            object: commit,
            object_type: ObjectType::Commit,
            name: b"v1.0".to_vec(),
            tagger: Some(b"Example User <example@example.invalid> 0 +0000".to_vec()),
            message: b"release\n".to_vec(),
        };
        let tag = db
            .write_object(EncodedObject::new(ObjectType::Tag, tag.write()))
            .expect("test operation should succeed");
        assert_eq!(
            peel_to_commit(&db, ObjectFormat::Sha1, &tag).expect("test operation should succeed"),
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
        .expect("test operation should succeed");
        db.write_object(EncodedObject::new(ObjectType::Tree, Vec::new()))
            .expect("test operation should succeed");
        let commit = write_test_commit(&mut db, tree, Vec::new(), b"base\n");
        let tag = Tag {
            object: commit,
            object_type: ObjectType::Commit,
            name: b"v1.0".to_vec(),
            tagger: Some(b"Example User <example@example.invalid> 0 +0000".to_vec()),
            message: b"release\n".to_vec(),
        };
        let tag = db
            .write_object(EncodedObject::new(ObjectType::Tag, tag.write()))
            .expect("test operation should succeed");
        assert_eq!(
            resolve_revision_with_reader(&git_dir, ObjectFormat::Sha1, &db, &format!("{tag}^{{}}"))
                .expect("test operation should succeed"),
            commit
        );
        assert_eq!(
            resolve_revision_with_reader(
                &git_dir,
                ObjectFormat::Sha1,
                &db,
                &format!("{tag}^{{commit}}")
            )
            .expect("test operation should succeed"),
            commit
        );
        assert_eq!(
            resolve_revision_with_reader(
                &git_dir,
                ObjectFormat::Sha1,
                &db,
                &format!("{tag}^{{tree}}")
            )
            .expect("test operation should succeed"),
            tree
        );
        assert_eq!(
            resolve_revision_with_reader(
                &git_dir,
                ObjectFormat::Sha1,
                &db,
                &format!("{tag}^{{tag}}")
            )
            .expect("test operation should succeed"),
            tag
        );
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn pack_refs_with_auto_peel_writes_peeled_tag() {
        let git_dir = temp_git_dir();
        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let tree = db
            .write_object(EncodedObject::new(ObjectType::Tree, Vec::new()))
            .expect("test operation should succeed");
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
            .expect("test operation should succeed");
        let tag = Tag {
            object: commit,
            object_type: ObjectType::Commit,
            name: b"v1.0".to_vec(),
            tagger: Some(b"Example User <example@example.invalid> 0 +0000".to_vec()),
            message: b"release\n".to_vec(),
        };
        let tag = db
            .write_object(EncodedObject::new(ObjectType::Tag, tag.write()))
            .expect("test operation should succeed");
        let refs = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let mut tx = refs.transaction();
        tx.update(RefUpdate {
            name: "refs/tags/v1.0".into(),
            expected: None,
            new: RefTarget::Direct(tag),
            reflog: None,
        });
        tx.commit().expect("test operation should succeed");

        let packed = pack_refs_with_auto_peel(&git_dir, ObjectFormat::Sha1, true)
            .expect("test operation should succeed");
        let packed_tag = packed
            .iter()
            .find(|packed| packed.reference.name == "refs/tags/v1.0")
            .expect("test operation should succeed");
        assert_eq!(packed_tag.peeled, Some(commit));
        assert_eq!(
            refs.read_ref("refs/tags/v1.0")
                .expect("test operation should succeed"),
            Some(RefTarget::Direct(tag))
        );
        assert!(!git_dir.join("refs").join("tags").join("v1.0").exists());
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn resolve_rev_path_finds_nested_blob_and_subtree() {
        let git_dir = temp_git_dir();
        let mut db = ObjectDatabase::new(ObjectFormat::Sha1);
        let blob = db
            .write_object(EncodedObject::new(ObjectType::Blob, b"hello\n".to_vec()))
            .expect("test operation should succeed");
        let sub = write_tree(&mut db, &[(0o100644, b"file.txt", &blob)]);
        let dir = write_tree(&mut db, &[(0o040000, b"sub", &sub)]);
        let root = write_tree(&mut db, &[(0o040000, b"dir", &dir)]);
        let commit = write_test_commit(&mut db, root, Vec::new(), b"init\n");

        // Nested blob via `<rev>:<path>`.
        assert_eq!(
            resolve_rev_path(
                &git_dir,
                ObjectFormat::Sha1,
                &db,
                &commit.to_hex(),
                "dir/sub/file.txt"
            )
            .expect("test operation should succeed"),
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
            .expect("test operation should succeed"),
            sub
        );
        // Empty path resolves to the commit's tree.
        assert_eq!(
            resolve_rev_path(&git_dir, ObjectFormat::Sha1, &db, &commit.to_hex(), "")
                .expect("test operation should succeed"),
            root
        );
        let entry = resolve_rev_path_entry(
            &git_dir,
            ObjectFormat::Sha1,
            &db,
            &commit.to_hex(),
            "dir/sub/file.txt",
        )
        .expect("test operation should succeed");
        assert_eq!(entry.oid, blob);
        assert_eq!(entry.mode, Some(0o100644));
        assert_eq!(entry.object_type, ObjectType::Blob);
        assert_eq!(entry.name, b"file.txt");
        let entry = resolve_rev_path_entry(&git_dir, ObjectFormat::Sha1, &db, &commit.to_hex(), "")
            .expect("test operation should succeed");
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
            .expect("test operation should succeed"),
            blob
        );
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn resolve_rev_path_reports_missing_and_non_tree_paths() {
        let git_dir = temp_git_dir();
        let mut db = ObjectDatabase::new(ObjectFormat::Sha1);
        let blob = db
            .write_object(EncodedObject::new(ObjectType::Blob, b"root\n".to_vec()))
            .expect("test operation should succeed");
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
        .expect_err("test operation should fail");
        assert!(
            matches!(&missing, GitError::NotFound(kind) if kind.to_string().contains("does not exist")),
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
        .expect_err("test operation should fail");
        assert!(
            matches!(&not_tree, GitError::NotFound(kind) if kind.to_string().contains("does not exist")),
            "unexpected error: {not_tree:?}"
        );
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn resolve_index_path_reads_stage_entries() {
        let git_dir = temp_git_dir();
        let oid_zero = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed");
        let oid_two = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "2222222222222222222222222222222222222222",
        )
        .expect("test operation should succeed");
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
            index
                .write(ObjectFormat::Sha1)
                .expect("test operation should succeed"),
        )
        .expect("test operation should succeed");

        // `:path` defaults to stage 0.
        assert_eq!(
            resolve_revision_with_reader(
                &git_dir,
                ObjectFormat::Sha1,
                &ObjectDatabase::new(ObjectFormat::Sha1),
                ":file.txt",
            )
            .expect("test operation should succeed"),
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
            .expect("test operation should succeed"),
            oid_two
        );
        // Wrong stage reports a stage-specific error.
        let wrong_stage = resolve_revision_with_reader(
            &git_dir,
            ObjectFormat::Sha1,
            &ObjectDatabase::new(ObjectFormat::Sha1),
            ":1:conflict.txt",
        )
        .expect_err("test operation should fail");
        assert!(
            matches!(&wrong_stage, GitError::NotFound(kind) if kind.to_string().contains("not at stage 1")),
            "unexpected error: {wrong_stage:?}"
        );
        // Unknown path reports "not in the index".
        let unknown = resolve_revision_with_reader(
            &git_dir,
            ObjectFormat::Sha1,
            &ObjectDatabase::new(ObjectFormat::Sha1),
            ":missing.txt",
        )
        .expect_err("test operation should fail");
        assert!(
            matches!(&unknown, GitError::NotFound(kind) if kind.to_string().contains("not in the index")),
            "unexpected error: {unknown:?}"
        );
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn search_commit_message_all_finds_matching_commit() {
        let git_dir = temp_git_dir();
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let tree = db
            .write_object(EncodedObject::new(ObjectType::Tree, Vec::new()))
            .expect("test operation should succeed");
        let first = write_dated_commit(&mut db, tree, Vec::new(), b"add feature\n", 1000);
        let second = write_dated_commit(&mut db, tree, vec![first], b"fix the widget bug\n", 2000);
        let third = write_dated_commit(&mut db, tree, vec![second], b"unrelated change\n", 3000);
        let refs = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let mut tx = refs.transaction();
        tx.update(RefUpdate {
            name: "refs/heads/main".into(),
            expected: None,
            new: RefTarget::Direct(third),
            reflog: None,
        });
        tx.commit().expect("test operation should succeed");

        assert_eq!(
            resolve_revision_with_reader(&git_dir, ObjectFormat::Sha1, &db, ":/widget bug")
                .expect("test operation should succeed"),
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
            .expect("test operation should succeed"),
            second
        );
        // No match is an error.
        let miss = resolve_revision_with_reader(&git_dir, ObjectFormat::Sha1, &db, ":/zzznomatch")
            .expect_err("test operation should fail");
        assert!(
            matches!(miss, GitError::NotFound(_)),
            "unexpected: {miss:?}"
        );
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
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
        .expect("test operation should succeed");
        // base -> a -> b   (left line)
        //   \--> c -> d    (right line)
        let base = write_test_commit(&mut db, tree, Vec::new(), b"base\n");
        let a = write_test_commit(&mut db, tree, vec![base], b"a\n");
        let b = write_test_commit(&mut db, tree, vec![a], b"b\n");
        let c = write_test_commit(&mut db, tree, vec![base], b"c\n");
        let d = write_test_commit(&mut db, tree, vec![c.clone()], b"d\n");

        // A..B: reachable from B (a..b line) but not from A (base only here) ->
        // {a, b}; base and earlier are excluded.
        let range = RevisionRange::Asymmetric {
            start: a.to_hex(),
            end: b.to_hex(),
        };
        let mut got = resolve_revision_range(&git_dir, ObjectFormat::Sha1, &db, &range)
            .expect("test operation should succeed");
        got.sort_by_key(|x| x.to_hex());
        assert_eq!(got, vec![b]);
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
                .expect("test operation should succeed")
                .into_iter()
                .collect();
        let expected: HashSet<ObjectId> = [a, b, c, d].into_iter().collect();
        assert_eq!(got_sym, expected);
        assert!(!got_sym.contains(&base), "shared base excluded from ...");
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn revision_selection_resolves_asymmetric_range() {
        let git_dir = temp_git_dir();
        let format = ObjectFormat::Sha1;
        let (db, all) = build_history(&git_dir, format);
        let root = all[0].clone();
        let a = all[1].clone();
        let c = all[3].clone();

        let selection = RevisionSelection::from_specs([format!("{a}..{c}")])
            .expect("test operation should succeed");
        let resolved = selection
            .resolve(&git_dir, format, &db)
            .expect("test operation should succeed");

        assert_eq!(resolved.starts, vec![c.clone()]);
        assert_eq!(resolved.excluded, oid_set([root, a]));
        assert_oid_set(
            resolved
                .selected_commit_oids(&git_dir, format, &db, false)
                .expect("test operation should succeed"),
            [c],
        );
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn revision_selection_resolves_default_left_range() {
        let git_dir = temp_git_dir();
        let format = ObjectFormat::Sha1;
        let (db, all) = build_history(&git_dir, format);
        let root = all[0].clone();
        let a = all[1].clone();
        let c = all[3].clone();
        fs::write(git_dir.join("HEAD"), b"ref: refs/heads/main\n")
            .expect("test operation should succeed");
        set_branch(&git_dir, "main", &a);

        let selection = RevisionSelection::from_specs([format!("..{c}")])
            .expect("test operation should succeed");
        let resolved = selection
            .resolve(&git_dir, format, &db)
            .expect("test operation should succeed");

        assert_eq!(resolved.starts, vec![c.clone()]);
        assert_eq!(resolved.excluded, oid_set([root, a]));
        assert_oid_set(
            resolved
                .selected_commit_oids(&git_dir, format, &db, false)
                .expect("test operation should succeed"),
            [c],
        );
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn revision_selection_resolves_default_right_range() {
        let git_dir = temp_git_dir();
        let format = ObjectFormat::Sha1;
        let (db, all) = build_history(&git_dir, format);
        let root = all[0].clone();
        let a = all[1].clone();
        let c = all[3].clone();
        fs::write(git_dir.join("HEAD"), b"ref: refs/heads/main\n")
            .expect("test operation should succeed");
        set_branch(&git_dir, "main", &c);

        let selection = RevisionSelection::from_specs([format!("{a}..")])
            .expect("test operation should succeed");
        let resolved = selection
            .resolve(&git_dir, format, &db)
            .expect("test operation should succeed");

        assert_eq!(resolved.starts, vec![c.clone()]);
        assert_eq!(resolved.excluded, oid_set([root, a]));
        assert_oid_set(
            resolved
                .selected_commit_oids(&git_dir, format, &db, false)
                .expect("test operation should succeed"),
            [c],
        );
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn revision_selection_resolves_symmetric_range() {
        let git_dir = temp_git_dir();
        let format = ObjectFormat::Sha1;
        let (db, all) = build_history(&git_dir, format);
        let root = all[0].clone();
        let a = all[1].clone();
        let b = all[2].clone();

        let selection = RevisionSelection::from_specs([format!("{a}...{b}")])
            .expect("test operation should succeed");
        let resolved = selection
            .resolve(&git_dir, format, &db)
            .expect("test operation should succeed");

        assert_eq!(resolved.starts, vec![a, b]);
        assert_eq!(resolved.excluded, oid_set([root]));
        assert_oid_set(
            resolved
                .selected_commit_oids(&git_dir, format, &db, false)
                .expect("test operation should succeed"),
            [a, b],
        );
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn revision_selection_resolves_caret_exclude() {
        let git_dir = temp_git_dir();
        let format = ObjectFormat::Sha1;
        let (db, all) = build_history(&git_dir, format);
        let root = all[0].clone();
        let a = all[1].clone();

        let selection = RevisionSelection::from_specs([format!("^{a}")])
            .expect("test operation should succeed");
        let resolved = selection
            .resolve(&git_dir, format, &db)
            .expect("test operation should succeed");

        assert!(resolved.starts.is_empty());
        assert_eq!(resolved.excluded, oid_set([root, a]));
        assert!(
            resolved
                .selected_commit_oids(&git_dir, format, &db, false)
                .expect("test operation should succeed")
                .is_empty()
        );
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn revision_selection_resolves_bare_include() {
        let git_dir = temp_git_dir();
        let format = ObjectFormat::Sha1;
        let (db, all) = build_history(&git_dir, format);
        let root = all[0].clone();
        let a = all[1].clone();
        let c = all[3].clone();

        let selection =
            RevisionSelection::from_specs([c.to_hex()]).expect("test operation should succeed");
        let resolved = selection
            .resolve(&git_dir, format, &db)
            .expect("test operation should succeed");

        assert_eq!(resolved.starts, vec![c.clone()]);
        assert!(resolved.excluded.is_empty());
        assert_oid_set(
            resolved
                .selected_commit_oids(&git_dir, format, &db, false)
                .expect("test operation should succeed"),
            [root, a, c],
        );
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn merge_bases_finds_common_ancestor() {
        let git_dir = temp_git_dir();
        let mut db = ObjectDatabase::new(ObjectFormat::Sha1);
        let tree = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "4b825dc642cb6eb9a060e54bf8d69288fbee4904",
        )
        .expect("test operation should succeed");
        let base = write_test_commit(&mut db, tree, Vec::new(), b"base\n");
        let left = write_test_commit(&mut db, tree, vec![base], b"left\n");
        let right = write_test_commit(&mut db, tree, vec![base], b"right\n");
        assert_eq!(
            merge_bases(&git_dir, ObjectFormat::Sha1, &db, &left, &right)
                .expect("test operation should succeed"),
            vec![base]
        );
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn resolve_bare_at_is_head() {
        let git_dir = temp_git_dir();
        let oid = test_oid(0xaa);
        fs::write(git_dir.join("HEAD"), b"ref: refs/heads/main\n")
            .expect("test operation should succeed");
        set_branch(&git_dir, "main", &oid);
        assert_eq!(
            resolve_revision(&git_dir, ObjectFormat::Sha1, "@")
                .expect("test operation should succeed"),
            oid
        );
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn resolve_head_reflog_nth() {
        let git_dir = temp_git_dir();
        let c0 = test_oid(0x10);
        let c1 = test_oid(0x11);
        let c2 = test_oid(0x12);
        fs::write(git_dir.join("HEAD"), b"ref: refs/heads/main\n")
            .expect("test operation should succeed");
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
            resolve_revision(&git_dir, ObjectFormat::Sha1, "@{0}")
                .expect("test operation should succeed"),
            c2
        );
        assert_eq!(
            resolve_revision(&git_dir, ObjectFormat::Sha1, "HEAD@{1}")
                .expect("test operation should succeed"),
            c1
        );
        assert_eq!(
            resolve_revision(&git_dir, ObjectFormat::Sha1, "@{2}")
                .expect("test operation should succeed"),
            c0
        );
        // Out-of-range reports a git-style "only has N entries" error.
        let err = resolve_revision(&git_dir, ObjectFormat::Sha1, "@{5}")
            .expect_err("test operation should fail");
        assert!(
            matches!(&err, GitError::NotFound(kind) if kind.to_string().contains("only has 3 entries")),
            "unexpected error: {err:?}"
        );
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
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
            resolve_revision(&git_dir, ObjectFormat::Sha1, "topic@{0}")
                .expect("test operation should succeed"),
            new
        );
        assert_eq!(
            resolve_revision(&git_dir, ObjectFormat::Sha1, "topic@{1}")
                .expect("test operation should succeed"),
            old
        );
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn resolve_upstream_via_branch_config() {
        let git_dir = temp_git_dir();
        let tip = test_oid(0x30);
        fs::write(git_dir.join("HEAD"), b"ref: refs/heads/main\n")
            .expect("test operation should succeed");
        set_branch(&git_dir, "main", &tip);
        set_ref(&git_dir, "refs/remotes/origin/main", &tip);
        fs::write(
            git_dir.join("config"),
            b"[branch \"main\"]\n\tremote = origin\n\tmerge = refs/heads/main\n",
        )
        .expect("test operation should succeed");

        for spec in ["@{u}", "@{upstream}", "main@{upstream}"] {
            assert_eq!(
                resolve_revision(&git_dir, ObjectFormat::Sha1, spec)
                    .expect("test operation should succeed"),
                tip,
                "spec {spec}"
            );
        }
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn resolve_push_falls_back_to_upstream_then_uses_push_remote() {
        let git_dir = temp_git_dir();
        let up = test_oid(0x40);
        let pushed = test_oid(0x41);
        fs::write(git_dir.join("HEAD"), b"ref: refs/heads/main\n")
            .expect("test operation should succeed");
        set_branch(&git_dir, "main", &up);
        set_ref(&git_dir, "refs/remotes/origin/main", &up);

        // No push-specific config: `@{push}` mirrors `@{u}` (origin/main).
        fs::write(
            git_dir.join("config"),
            b"[branch \"main\"]\n\tremote = origin\n\tmerge = refs/heads/main\n",
        )
        .expect("test operation should succeed");
        assert_eq!(
            resolve_revision(&git_dir, ObjectFormat::Sha1, "@{push}")
                .expect("test operation should succeed"),
            up
        );

        // With a pushRemote, `@{push}` follows refs/remotes/<pushRemote>/<short>.
        set_ref(&git_dir, "refs/remotes/fork/main", &pushed);
        fs::write(
            git_dir.join("config"),
            b"[branch \"main\"]\n\tremote = origin\n\tpushRemote = fork\n\tmerge = refs/heads/main\n",
        )
        .expect("test operation should succeed");
        assert_eq!(
            resolve_revision(&git_dir, ObjectFormat::Sha1, "@{push}")
                .expect("test operation should succeed"),
            pushed
        );
        // `@{u}` still uses the upstream remote, not the push remote.
        assert_eq!(
            resolve_revision(&git_dir, ObjectFormat::Sha1, "@{u}")
                .expect("test operation should succeed"),
            up
        );
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn resolve_previous_checkout_branch() {
        let git_dir = temp_git_dir();
        let main_tip = test_oid(0x50);
        let feature_tip = test_oid(0x51);
        fs::write(git_dir.join("HEAD"), b"ref: refs/heads/feature\n")
            .expect("test operation should succeed");
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
            resolve_revision(&git_dir, ObjectFormat::Sha1, "@{-1}")
                .expect("test operation should succeed"),
            main_tip
        );
        // `@{-2}` = the checkout before that (feature).
        assert_eq!(
            resolve_revision(&git_dir, ObjectFormat::Sha1, "@{-2}")
                .expect("test operation should succeed"),
            feature_tip
        );
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn at_selector_composes_with_parent_suffix() {
        // `@{0}^` must resolve the reflog value first, then apply `^`: the
        // suffix splitter peels the `^` and recurses back into the `@{...}` base.
        let git_dir = temp_git_dir();
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let tree = db
            .write_object(EncodedObject::new(ObjectType::Tree, Vec::new()))
            .expect("test operation should succeed");
        let parent = write_dated_commit(&mut db, tree, Vec::new(), b"parent\n", 1000);
        let child = write_dated_commit(&mut db, tree, vec![parent], b"child\n", 2000);
        fs::write(git_dir.join("HEAD"), b"ref: refs/heads/main\n")
            .expect("test operation should succeed");
        set_branch(&git_dir, "main", &child);
        write_head_reflog(
            &git_dir,
            &[
                (&zero_oid(), &parent, "commit (initial): parent"),
                (&parent, &child, "commit: child"),
            ],
        );
        assert_eq!(
            resolve_revision(&git_dir, ObjectFormat::Sha1, "@{0}")
                .expect("test operation should succeed"),
            child
        );
        assert_eq!(
            resolve_revision(&git_dir, ObjectFormat::Sha1, "@{0}^")
                .expect("test operation should succeed"),
            parent
        );
        assert_eq!(
            resolve_revision(&git_dir, ObjectFormat::Sha1, "HEAD@{0}~1")
                .expect("test operation should succeed"),
            parent
        );
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn resolve_at_selector_rejects_unsupported_and_malformed() {
        let git_dir = temp_git_dir();
        fs::write(git_dir.join("HEAD"), b"ref: refs/heads/main\n")
            .expect("test operation should succeed");
        set_branch(&git_dir, "main", &test_oid(0x60));
        // Date-based selectors are not implemented.
        let unsupported = resolve_revision(&git_dir, ObjectFormat::Sha1, "@{yesterday}")
            .expect_err("test operation should fail");
        assert!(
            matches!(&unsupported, GitError::Unsupported(_)),
            "unexpected error: {unsupported:?}"
        );
        // `@{-N}` only applies to a bare base.
        let bad_base = resolve_revision(&git_dir, ObjectFormat::Sha1, "main@{-1}")
            .expect_err("test operation should fail");
        assert!(
            matches!(&bad_base, GitError::InvalidFormat(_)),
            "unexpected error: {bad_base:?}"
        );
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    fn test_oid(byte: u8) -> ObjectId {
        ObjectId::from_hex(ObjectFormat::Sha1, &format!("{byte:02x}").repeat(20))
            .expect("test operation should succeed")
    }

    fn zero_oid() -> ObjectId {
        ObjectId::from_hex(ObjectFormat::Sha1, &"0".repeat(40))
            .expect("test operation should succeed")
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
            new: RefTarget::Direct(*oid),
            reflog: None,
        });
        tx.commit().expect("test operation should succeed");
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
        refs.write_reflog(name, &entries)
            .expect("test operation should succeed");
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
            .expect("test operation should succeed")
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
            .expect("test operation should succeed")
    }

    fn write_tree(db: &mut ObjectDatabase, entries: &[(u32, &[u8], &ObjectId)]) -> ObjectId {
        let tree = sley_object::Tree {
            entries: entries
                .iter()
                .map(|(mode, name, oid)| sley_object::TreeEntry {
                    mode: *mode,
                    name: BString::from(*name),
                    oid: (*oid).clone(),
                })
                .collect(),
        };
        db.write_object(EncodedObject::new(ObjectType::Tree, tree.write()))
            .expect("test operation should succeed")
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
            oid: *oid,
            flags: (stage & 0x3) << 12,
            flags_extended: 0,
            path: BString::from(path),
        }
    }

    fn temp_git_dir() -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "sley-rev-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("test operation should succeed");
        path
    }

    /// An object reader that refuses every read, used to prove a query was
    /// answered entirely from the commit-graph (parent/ancestry lookups never
    /// touched the odb).
    struct PanicReader;
    impl ObjectReader for PanicReader {
        fn read_object(&self, oid: &ObjectId) -> Result<std::sync::Arc<EncodedObject>> {
            Err(GitError::not_found(format!(
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
                        generations.insert(*oid, candidate);
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
            parents_map.insert(
                *oid,
                commit_parents(reader, format, oid).expect("test operation should succeed"),
            );
        }
        let generations = generation_numbers(&parents_map);
        let entries: Vec<sley_formats::CommitGraphWriteEntry> = commits
            .iter()
            .map(|oid| {
                let object = reader
                    .read_object(oid)
                    .expect("test operation should succeed");
                let commit =
                    Commit::parse_ref(format, &object.body).expect("test operation should succeed");
                let commit_time =
                    commit_committer_time(commit.committer).unwrap_or(0).max(0) as u64;
                sley_formats::CommitGraphWriteEntry {
                    oid: *oid,
                    tree: commit.tree,
                    parents: commit.parents,
                    generation: generations.get(oid).copied().unwrap_or(1),
                    commit_time,
                    bloom_filter: None,
                }
            })
            .collect();
        let bytes = CommitGraph::write(format, &entries).expect("test operation should succeed");
        let info = git_dir.join("objects").join("info");
        fs::create_dir_all(&info).expect("test operation should succeed");
        fs::write(info.join("commit-graph"), bytes).expect("test operation should succeed");
    }

    fn remove_commit_graph(git_dir: &Path) {
        let path = git_dir.join("objects").join("info").join("commit-graph");
        if path.exists() {
            fs::remove_file(path).expect("test operation should succeed");
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
            .expect("test operation should succeed");
        let mut t = 1000i64;
        let mut commit = |db: &mut FileObjectDatabase, parents: Vec<ObjectId>, msg: &[u8]| {
            t += 1;
            write_dated_commit(db, tree, parents, msg, t)
        };
        let root = commit(&mut db, vec![], b"root\n");
        let a = commit(&mut db, vec![root], b"a\n");
        let b = commit(&mut db, vec![root], b"b\n");
        let c = commit(&mut db, vec![a], b"c\n");
        let d = commit(&mut db, vec![b], b"d\n");
        let e = commit(&mut db, vec![b], b"e\n");
        let m1 = commit(&mut db, vec![c.clone(), d.clone()], b"m1\n");
        let f = commit(&mut db, vec![d.clone(), e.clone()], b"f\n");
        let g = commit(&mut db, vec![f.clone()], b"g\n");
        let oct = commit(&mut db, vec![m1.clone(), g.clone(), f.clone()], b"oct\n");
        let x1 = commit(&mut db, vec![a, b], b"x1\n");
        let x2 = commit(&mut db, vec![b, a], b"x2\n");
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
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    type WalkResult = (String, String, bool, Vec<String>, Vec<String>, Vec<String>);

    /// Run is_ancestor, merge_bases (both orders), and the `A..B`/`A...B` ranges
    /// over all pairs, returning a deterministic snapshot for comparison.
    fn collect_walk_results(
        git_dir: &Path,
        format: ObjectFormat,
        reader: &impl ObjectReader,
        all: &[ObjectId],
    ) -> Vec<WalkResult> {
        let mut out = Vec::new();
        for left in all {
            for right in all {
                let anc = is_ancestor(git_dir, format, reader, left, right)
                    .expect("test operation should succeed");
                let mut bases: Vec<String> = merge_bases(git_dir, format, reader, left, right)
                    .expect("test operation should succeed")
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
                        .expect("test operation should succeed")
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
                        .expect("test operation should succeed")
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
        let mut xbases =
            merge_bases(&git_dir, format, &db, &x1, &x2).expect("test operation should succeed");
        xbases.sort_by_key(|oid| oid.to_hex());
        let mut expected = vec![a, b];
        expected.sort_by_key(|oid| oid.to_hex());
        assert_eq!(xbases, expected, "criss-cross must yield two merge bases");

        // Octopus child reaches m1 along its first parent edge.
        assert!(
            is_ancestor(&git_dir, format, &db, &m1, &oct).expect("test operation should succeed")
        );
        // m1 is a merge base of itself and the octopus.
        assert_eq!(
            merge_bases(&git_dir, format, &db, &m1, &oct).expect("test operation should succeed"),
            vec![m1.clone()]
        );
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
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
        assert!(
            is_ancestor(&git_dir, format, &PanicReader, &root, &oct)
                .expect("test operation should succeed")
        );
        assert!(
            !is_ancestor(&git_dir, format, &PanicReader, &oct, &root)
                .expect("test operation should succeed")
        );
        assert!(
            is_ancestor(&git_dir, format, &PanicReader, &a, &oct)
                .expect("test operation should succeed")
        );

        let bases = merge_bases(&git_dir, format, &PanicReader, &x1, &x2)
            .expect("test operation should succeed");
        assert_eq!(bases.len(), 2, "criss-cross bases via graph only");

        // Range resolution peels its two endpoints from the odb (the graph does
        // not record object types), but the ancestry *walk* between them is
        // graph-backed. Verify the result matches the object-only walk.
        let range = RevisionRange::Asymmetric {
            start: a.to_hex(),
            end: oct.to_hex(),
        };
        let mut included: Vec<String> = resolve_revision_range(&git_dir, format, &db, &range)
            .expect("test operation should succeed")
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
        let object_bases =
            merge_bases(&git_dir, format, &db, &x1, &x2).expect("test operation should succeed");
        let mut object_range: Vec<String> = resolve_revision_range(&git_dir, format, &db, &range)
            .expect("test operation should succeed")
            .iter()
            .map(|oid| oid.to_hex())
            .collect();
        object_range.sort();
        write_commit_graph_file(&git_dir, format, &db, &all);
        let graph_bases = merge_bases(&git_dir, format, &PanicReader, &x1, &x2)
            .expect("test operation should succeed");
        assert_eq!(object_bases, graph_bases);
        assert_eq!(object_range, included, "range walk diverged with graph");
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
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
        let base_p1 = resolve_revision_with_reader(&git_dir, format, &db, &format!("{oct}^1"))
            .expect("test operation should succeed");
        let base_p2 = resolve_revision_with_reader(&git_dir, format, &db, &format!("{oct}^2"))
            .expect("test operation should succeed");
        let base_p3 = resolve_revision_with_reader(&git_dir, format, &db, &format!("{oct}^3"))
            .expect("test operation should succeed");
        let base_first = resolve_revision_with_reader(&git_dir, format, &db, &format!("{oct}~1"))
            .expect("test operation should succeed");
        assert_eq!((&base_p1, &base_p2, &base_p3), (&m1, &g, &f));
        assert_eq!(base_first, m1);

        // With the graph present, the same suffixes resolve without object reads.
        write_commit_graph_file(&git_dir, format, &db, &all);
        assert_eq!(
            resolve_revision_with_reader(&git_dir, format, &PanicReader, &format!("{oct}^2"))
                .expect("test operation should succeed"),
            base_p2
        );
        assert_eq!(
            resolve_revision_with_reader(&git_dir, format, &PanicReader, &format!("{oct}~1"))
                .expect("test operation should succeed"),
            base_first
        );
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn missing_or_unparseable_graph_falls_back_to_objects() {
        let git_dir = temp_git_dir();
        let format = ObjectFormat::Sha1;
        let (db, all) = build_history(&git_dir, format);
        let (a, oct) = (all[1].clone(), all[9].clone());
        let object_answer =
            is_ancestor(&git_dir, format, &db, &a, &oct).expect("test operation should succeed");

        // A corrupt graph file must be ignored (not error), falling back to the
        // odb so the answer is unchanged.
        let info = git_dir.join("objects").join("info");
        fs::create_dir_all(&info).expect("test operation should succeed");
        fs::write(info.join("commit-graph"), b"not a real commit graph")
            .expect("test operation should succeed");
        assert_eq!(
            is_ancestor(&git_dir, format, &db, &a, &oct).expect("test operation should succeed"),
            object_answer
        );
        // A graph that omits some commits must also fall back per-missing-commit.
        write_commit_graph_file(&git_dir, format, &db, &all[..3]);
        assert_eq!(
            is_ancestor(&git_dir, format, &db, &a, &oct).expect("test operation should succeed"),
            object_answer
        );
        assert_eq!(
            merge_bases(&git_dir, format, &db, &all[10], &all[11])
                .expect("test operation should succeed"),
            {
                remove_commit_graph(&git_dir);
                merge_bases(&git_dir, format, &db, &all[10], &all[11])
                    .expect("test operation should succeed")
            }
        );
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
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
            .expect("test operation should succeed");
        let root = write_dated_commit(&mut db, tree, vec![], b"root\n", 1000);
        let mid = write_dated_commit(&mut db, tree, vec![root], b"mid\n", 1001);
        let tip = write_dated_commit(&mut db, tree, vec![mid.clone()], b"tip\n", 1002);
        let commits = [root, mid.clone(), tip.clone()];

        let parents_map: HashMap<ObjectId, Vec<ObjectId>> = commits
            .iter()
            .map(|oid| {
                (
                    *oid,
                    commit_parents(&db, format, oid).expect("test operation should succeed"),
                )
            })
            .collect();
        let generations = generation_numbers(&parents_map);
        let entries: Vec<sley_formats::CommitGraphWriteEntry> = commits
            .iter()
            .map(|oid| sley_formats::CommitGraphWriteEntry {
                oid: *oid,
                tree,
                parents: parents_map[oid].clone(),
                generation: generations[oid],
                commit_time: 0,
                bloom_filter: None,
            })
            .collect();
        let bytes = CommitGraph::write(format, &entries).expect("test operation should succeed");

        // Lay the bytes out as a one-layer chain.
        let graphs = git_dir.join("objects").join("info").join("commit-graphs");
        fs::create_dir_all(&graphs).expect("test operation should succeed");
        let hash = sley_core::digest_bytes(format, &bytes)
            .expect("test operation should succeed")
            .to_hex();
        fs::write(graphs.join(format!("graph-{hash}.graph")), &bytes)
            .expect("test operation should succeed");
        fs::write(graphs.join("commit-graph-chain"), format!("{hash}\n"))
            .expect("test operation should succeed");

        // No monolithic commit-graph present, only the chain: queries must be
        // answerable from the chain without reading objects.
        assert!(
            !git_dir
                .join("objects")
                .join("info")
                .join("commit-graph")
                .exists()
        );
        assert!(
            is_ancestor(&git_dir, format, &PanicReader, &root, &tip)
                .expect("test operation should succeed")
        );
        assert_eq!(
            merge_bases(&git_dir, format, &PanicReader, &mid, &tip)
                .expect("test operation should succeed"),
            vec![mid.clone()]
        );
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    fn test_commit_graph(format: ObjectFormat, parent: &ObjectId, child: &ObjectId) -> Vec<u8> {
        let tree = ObjectId::from_hex(format, "4b825dc642cb6eb9a060e54bf8d69288fbee4904")
            .expect("test operation should succeed");
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
        let checksum =
            sley_core::digest_bytes(format, &out).expect("test operation should succeed");
        out.extend_from_slice(checksum.as_bytes());
        out
    }

    // --- RevWalk skeleton (STAGE-A) -------------------------------------

    /// Build a linear chain c0 <- c1 <- ... with strictly increasing committer
    /// times, returning the oids oldest-first. The empty tree is reused.
    fn build_linear_history(git_dir: &std::path::Path, n: usize) -> Vec<ObjectId> {
        let mut db = FileObjectDatabase::from_git_dir(git_dir, ObjectFormat::Sha1);
        let tree = db
            .write_object(EncodedObject::new(ObjectType::Tree, Vec::new()))
            .expect("write empty tree");
        let mut oids = Vec::new();
        let mut parents = Vec::new();
        for i in 0..n {
            let oid = write_dated_commit(
                &mut db,
                tree,
                parents.clone(),
                format!("c{i}\n").as_bytes(),
                100 + i as i64,
            );
            parents = vec![oid];
            oids.push(oid);
        }
        oids
    }

    fn walk_oids<R: ObjectReader>(walk: RevWalk<'_, R>) -> Vec<ObjectId> {
        walk.collect_all()
            .expect("walk succeeds")
            .into_iter()
            .map(|m| m.oid)
            .collect()
    }

    #[test]
    fn revwalk_commit_date_order_newest_first() {
        let git_dir = temp_git_dir();
        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let oids = build_linear_history(&git_dir, 4); // oldest..newest
        let tip = *oids.last().expect("tip");
        let got = walk_oids(RevWalk::new(&git_dir, ObjectFormat::Sha1, &db, [tip]));
        let mut expected = oids.clone();
        expected.reverse(); // newest committer-date first
        assert_eq!(got, expected);
        fs::remove_dir_all(git_dir).expect("cleanup");
    }

    #[test]
    fn revwalk_max_count_limits_output() {
        let git_dir = temp_git_dir();
        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let oids = build_linear_history(&git_dir, 5);
        let tip = *oids.last().expect("tip");
        let got = walk_oids(
            RevWalk::new(&git_dir, ObjectFormat::Sha1, &db, [tip]).max_count(Some(2)),
        );
        assert_eq!(got, vec![oids[4], oids[3]]);
        fs::remove_dir_all(git_dir).expect("cleanup");
    }

    #[test]
    fn revwalk_skip_then_limit() {
        let git_dir = temp_git_dir();
        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let oids = build_linear_history(&git_dir, 5);
        let tip = *oids.last().expect("tip");
        let got = walk_oids(
            RevWalk::new(&git_dir, ObjectFormat::Sha1, &db, [tip])
                .skip(1)
                .max_count(Some(2)),
        );
        // newest..oldest is c4,c3,c2,c1,c0; skip 1 -> c3,c2.
        assert_eq!(got, vec![oids[3], oids[2]]);
        fs::remove_dir_all(git_dir).expect("cleanup");
    }

    #[test]
    fn revwalk_delegates_match_old_limited_walk() {
        // The thin-wrapper invariant: walk_commit_metadata_date_ordered_limited
        // (now RevWalk-backed) is byte-identical to a direct RevWalk in
        // CommitDate order with the same limit.
        let git_dir = temp_git_dir();
        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let oids = build_linear_history(&git_dir, 6);
        let tip = *oids.last().expect("tip");
        let via_fn = walk_commit_metadata_date_ordered_limited(
            &git_dir,
            ObjectFormat::Sha1,
            &db,
            [tip],
            false,
            3,
        )
        .expect("limited walk")
        .into_iter()
        .map(|m| m.oid)
        .collect::<Vec<_>>();
        let via_walk = walk_oids(
            RevWalk::new(&git_dir, ObjectFormat::Sha1, &db, [tip])
                .order(RevWalkOrder::CommitDate)
                .max_count(Some(3)),
        );
        assert_eq!(via_fn, via_walk);
        fs::remove_dir_all(git_dir).expect("cleanup");
    }

    #[test]
    fn revwalk_first_parent_follows_one_line() {
        let git_dir = temp_git_dir();
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let tree = db
            .write_object(EncodedObject::new(ObjectType::Tree, Vec::new()))
            .expect("tree");
        let base = write_dated_commit(&mut db, tree, vec![], b"base\n", 100);
        let side = write_dated_commit(&mut db, tree, vec![base], b"side\n", 110);
        let main = write_dated_commit(&mut db, tree, vec![base], b"main\n", 120);
        let merge = write_dated_commit(&mut db, tree, vec![main, side], b"merge\n", 130);
        let first_parent = walk_oids(
            RevWalk::new(&git_dir, ObjectFormat::Sha1, &db, [merge]).first_parent(true),
        );
        // first-parent line: merge -> main -> base; `side` is skipped.
        assert_eq!(first_parent, vec![merge, main, base]);
        assert!(!first_parent.contains(&side));
        fs::remove_dir_all(git_dir).expect("cleanup");
    }

    #[test]
    fn revwalk_date_window_filters_and_prunes() {
        let git_dir = temp_git_dir();
        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let oids = build_linear_history(&git_dir, 5); // times 100..104
        let tip = *oids.last().expect("tip");
        // since=102 (>=102), until=103 (<=103) -> times 102,103 -> oids[3],oids[2].
        let got = walk_oids(
            RevWalk::new(&git_dir, ObjectFormat::Sha1, &db, [tip]).date_window(
                RevWalkDateWindow {
                    min_time: Some(102),
                    max_time: Some(103),
                },
            ),
        );
        assert_eq!(got, vec![oids[3], oids[2]]);
        fs::remove_dir_all(git_dir).expect("cleanup");
    }

    #[test]
    fn revwalk_pathspec_is_carried_but_not_pruning() {
        // STAGE-A: a pathspec is attached and round-trips, but does not yet
        // prune (TREESAME simplification is STAGE-B). The full history is still
        // returned.
        let git_dir = temp_git_dir();
        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let oids = build_linear_history(&git_dir, 3);
        let tip = *oids.last().expect("tip");
        let spec = Pathspec::parse([b"does/not/exist".as_slice()], PathspecMatchMagic::default())
            .expect("pathspec");
        let walk =
            RevWalk::new(&git_dir, ObjectFormat::Sha1, &db, [tip]).pathspec(spec.clone());
        assert_eq!(walk.pathspec_ref(), &spec);
        let got = walk_oids(walk);
        assert_eq!(got.len(), 3, "pathspec must not prune in STAGE-A");
        fs::remove_dir_all(git_dir).expect("cleanup");
    }
}
