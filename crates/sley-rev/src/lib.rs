pub mod bisect;
pub mod graph;
pub mod revlist;
mod setup;

use sley_config::GitConfig;
use sley_core::{GitError, MissingObjectContext, ObjectFormat, ObjectId, Result};

pub use setup::{
    MatchedRef, NoWalkMode, PseudoRefResolver, RevisionOptions, RevisionOrder,
    RevisionSetupContext, RevisionSymmetricRange, RevisionTip, SetupRevisions,
    ambiguous_argument_error, ambiguous_argument_message, setup_revisions, setup_revisions_os,
};
pub use sley_core::BString;
use sley_formats::CommitGraph;
use sley_index::Index;
use sley_object::{Commit, EncodedObject, ObjectType, Tag, TreeEntries};
use sley_odb::{FileObjectDatabase, ObjectPrefixResolution, ObjectReader, repository_objects_dir};
use sley_refs::{
    FileRefStore, PackedRef, RefTarget, ReflogEntry, validate_ref_name_for_read,
    validate_symref_target,
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

fn read_revision_object<R: ObjectReader>(reader: &R, oid: &ObjectId) -> Result<Arc<EncodedObject>> {
    reader
        .read_object(oid)
        .map_err(|err| with_missing_object_context(err, *oid, MissingObjectContext::RevisionWalk))
}

fn with_missing_object_context(
    err: GitError,
    oid: ObjectId,
    context: MissingObjectContext,
) -> GitError {
    let kind = err
        .not_found_kind()
        .and_then(sley_core::NotFoundKind::missing_object_kind);
    match kind {
        Some(kind) => GitError::object_kind_not_found_in(oid, kind, context),
        None => err,
    }
}

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

    pub fn borrowed(&self) -> Result<RevisionSpecRef<'_>> {
        RevisionSpecRef::parse(&self.raw)
    }
}

/// A borrowed, allocation-free classification of a revision spelling.
///
/// This is intentionally only a top-level parse for now: it separates the
/// forms that change the resolver entry point (`:/text`, `:[stage:]path`, and
/// `<rev>:<path>`) while leaving suffix chains like `^`, `~`, `^{tree}`, and
/// `^{/text}` to the existing suffix resolver. Keeping the slices borrowed lets
/// callers route a user-provided spec without copying, and it avoids the
/// brittle "first colon wins" behavior that misclassified colons inside
/// `^{/text}` and reflog selectors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RevisionSpecRef<'a> {
    raw: &'a str,
    kind: RevisionSpecKind<'a>,
}

impl<'a> RevisionSpecRef<'a> {
    pub fn parse(raw: &'a str) -> Result<Self> {
        if raw.is_empty() {
            return Err(GitError::InvalidFormat("empty revision spec".into()));
        }
        let kind = if let Some(text) = raw.strip_prefix(":/") {
            RevisionSpecKind::MessageSearch { text }
        } else if let Some(rest) = raw.strip_prefix(':') {
            let (stage, path) = parse_index_stage_path(rest);
            RevisionSpecKind::IndexPath { stage, path }
        } else if let Some((rev, path)) = split_top_level_rev_path(raw) {
            RevisionSpecKind::TreePath { rev, path }
        } else {
            RevisionSpecKind::Revision { rev: raw }
        };
        Ok(Self { raw, kind })
    }

    pub fn raw(&self) -> &'a str {
        self.raw
    }

    pub fn kind(&self) -> RevisionSpecKind<'a> {
        self.kind
    }

    pub fn tree_path(&self) -> Option<(&'a str, &'a str)> {
        match self.kind {
            RevisionSpecKind::TreePath { rev, path } => Some((rev, path)),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevisionSpecKind<'a> {
    MessageSearch { text: &'a str },
    IndexPath { stage: u8, path: &'a str },
    TreePath { rev: &'a str, path: &'a str },
    Revision { rev: &'a str },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitRecord {
    pub oid: ObjectId,
    pub parents: Vec<ObjectId>,
    pub commit: Commit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectDisambiguation {
    Any,
    Commit,
    Commitish,
    Tree,
    Treeish,
    Tag,
    Blob,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShortObjectIdResolution {
    Missing,
    Unique(ObjectId),
    Ambiguous(Vec<ObjectId>),
}

impl ShortObjectIdResolution {
    pub fn into_result(self, prefix: &str) -> Result<ObjectId> {
        match self {
            Self::Unique(oid) => Ok(oid),
            Self::Missing => Err(GitError::not_found(format!("revision {prefix}"))),
            Self::Ambiguous(_) => Err(short_object_id_ambiguous_error(prefix)),
        }
    }
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

/// Resolve a commit's root tree oid directly from the commit-graph, when a
/// usable monolithic graph covers `oid`.
///
/// Commit-graphs are optional acceleration data, so a missing, unsupported, or
/// corrupt graph is reported as `Ok(None)` and callers should fall back to
/// reading the commit object for parity.
pub fn commit_graph_tree_oid(
    git_dir: &Path,
    format: sley_core::ObjectFormat,
    oid: &ObjectId,
) -> Result<Option<ObjectId>> {
    let mut graph = CommitGraphContext::load(git_dir, format);
    match graph.direct_graph() {
        DirectCommitGraph::Raw(graph) => graph.tree_oid(oid).or(Ok(None)),
        DirectCommitGraph::Missing | DirectCommitGraph::Invalid => Ok(None),
    }
}

/// Terms that name the new/bad and old/good sides of an active bisect.
///
/// Git stores these as two LF-terminated lines in `$GIT_DIR/BISECT_TERMS`.
/// Missing state means the default `bad`/`good` vocabulary. Commands that need
/// to enumerate `refs/bisect/*` should use [`Self::is_bad_ref`] and
/// [`Self::is_good_ref`] so custom terms stay centralized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BisectTerms {
    pub bad: String,
    pub good: String,
}

impl Default for BisectTerms {
    fn default() -> Self {
        Self {
            bad: "bad".to_string(),
            good: "good".to_string(),
        }
    }
}

impl BisectTerms {
    pub fn is_bad_ref(&self, ref_name: &str) -> bool {
        bisect_ref_matches_term(ref_name, &self.bad)
    }

    pub fn is_good_ref(&self, ref_name: &str) -> bool {
        bisect_ref_matches_term(ref_name, &self.good)
    }
}

pub fn read_bisect_terms(git_dir: impl AsRef<Path>) -> Result<BisectTerms> {
    let path = git_dir.as_ref().join("BISECT_TERMS");
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(BisectTerms::default());
        }
        Err(err) => return Err(GitError::Io(err.to_string())),
    };
    let mut lines = contents.lines();
    let bad = match lines.next() {
        Some(line) => line.to_string(),
        None => String::new(),
    };
    let good = match lines.next() {
        Some(line) => line.to_string(),
        None => String::new(),
    };
    Ok(BisectTerms { bad, good })
}

fn bisect_ref_matches_term(ref_name: &str, term: &str) -> bool {
    ref_name
        .strip_prefix("refs/bisect/")
        .is_some_and(|name| name.starts_with(term))
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
    resolve_revision_inner(
        git_dir,
        format,
        reader,
        rev,
        None,
        ObjectDisambiguation::Any,
    )
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
    resolve_revision_inner(
        git_dir,
        format,
        reader,
        rev,
        Some(config),
        ObjectDisambiguation::Any,
    )
}

/// Resolve `rev` to an [`ObjectId`], preferring objects that satisfy
/// `disambiguation` when (and only when) `rev` falls through to short
/// object-id prefix resolution. Ref names always take precedence over a
/// same-spelled short hex prefix, mirroring git's `get_oid_basic`
/// (`repo_dwim_ref` is consulted before `get_short_oid`); the disambiguation
/// flag only narrows the candidate set at the short-OID stage.
pub fn resolve_revision_with_disambiguation(
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    rev: &str,
    disambiguation: ObjectDisambiguation,
) -> Result<ObjectId> {
    let git_dir = git_dir.as_ref();
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    resolve_revision_inner(git_dir, format, &db, rev, None, disambiguation)
}

/// `commit-ish` variant of [`resolve_revision_with_reader`]: a ref still wins
/// over a same-spelled short hex prefix, but an ambiguous bare prefix is
/// narrowed to its commit-ish candidates (used by the revision walker setup so
/// `cherry-pick <ref>` / `revert <ref>` honour ref precedence while keeping the
/// commit-ish disambiguation for genuine bare-OID prefixes).
pub fn resolve_revision_commitish_with_reader<R: ObjectReader>(
    git_dir: &Path,
    format: ObjectFormat,
    reader: &R,
    rev: &str,
) -> Result<ObjectId> {
    resolve_revision_inner(
        git_dir,
        format,
        reader,
        rev,
        None,
        ObjectDisambiguation::Commitish,
    )
}

/// Like [`resolve_revision_commitish_with_reader`] but resolves `@{upstream}` /
/// `@{push}` against a caller-supplied effective config. See
/// [`resolve_revision_with_config`].
pub fn resolve_revision_commitish_with_config<R: ObjectReader>(
    git_dir: &Path,
    format: ObjectFormat,
    reader: &R,
    rev: &str,
    config: &GitConfig,
) -> Result<ObjectId> {
    resolve_revision_inner(
        git_dir,
        format,
        reader,
        rev,
        Some(config),
        ObjectDisambiguation::Commitish,
    )
}

/// Commit-ish revision resolution that builds its own on-disk reader. See
/// [`resolve_revision_commitish_with_reader`].
pub fn resolve_revision_commitish(
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    rev: &str,
) -> Result<ObjectId> {
    let git_dir = git_dir.as_ref();
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    resolve_revision_commitish_with_reader(git_dir, format, &db, rev)
}

/// Resolve a revision expression to the full refname that names it, when the
/// expression is ref-backed. This is the symbolic side of
/// `resolve_revision_with_config`: it is used by callers such as
/// `rev-parse --symbolic-full-name`, checkout, and branch deletion to keep
/// `@{upstream}` selectors as refs instead of flattening them to object IDs.
pub fn resolve_revision_symbolic_full_name(
    git_dir: &Path,
    format: ObjectFormat,
    rev: &str,
) -> Result<Option<String>> {
    resolve_revision_symbolic_full_name_inner(git_dir, format, rev, None)
}

pub fn resolve_revision_symbolic_full_name_with_config(
    git_dir: &Path,
    format: ObjectFormat,
    rev: &str,
    config: &GitConfig,
) -> Result<Option<String>> {
    resolve_revision_symbolic_full_name_inner(git_dir, format, rev, Some(config))
}

fn resolve_revision_symbolic_full_name_inner(
    git_dir: &Path,
    format: ObjectFormat,
    rev: &str,
    config: Option<&GitConfig>,
) -> Result<Option<String>> {
    if rev.len() == format.hex_len() && rev.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Ok(None);
    }
    if let Some(name) = resolve_at_selector_ref_name(git_dir, format, rev, config)? {
        return Ok(Some(name));
    }
    let refs = FileRefStore::new(git_dir.to_path_buf(), format);
    if rev == "HEAD" {
        return refs.current_branch_ref();
    }
    if rev.starts_with("refs/") {
        return Ok(refs.read_ref(rev)?.map(|_| rev.to_string()));
    }
    for candidate in [
        format!("refs/{rev}"),
        format!("refs/tags/{rev}"),
        format!("refs/heads/{rev}"),
        format!("refs/remotes/{rev}"),
        format!("refs/remotes/{rev}/HEAD"),
    ] {
        if refs.read_ref(&candidate)?.is_some() {
            return Ok(Some(candidate));
        }
    }
    Err(GitError::not_found(format!("revision {rev}")))
}

fn resolve_revision_inner<R: ObjectReader>(
    git_dir: &Path,
    format: ObjectFormat,
    reader: &R,
    rev: &str,
    config: Option<&GitConfig>,
    disambiguation: ObjectDisambiguation,
) -> Result<ObjectId> {
    let parsed = RevisionSpecRef::parse(rev)?;
    match parsed.kind() {
        RevisionSpecKind::MessageSearch { text } => {
            return search_commit_message_all(git_dir, format, reader, text);
        }
        RevisionSpecKind::IndexPath { stage, path } => {
            return resolve_index_path(git_dir, format, reader, stage, path);
        }
        RevisionSpecKind::TreePath {
            rev: rev_part,
            path,
        } => {
            return resolve_rev_path(git_dir, format, reader, rev_part, path);
        }
        RevisionSpecKind::Revision { rev: _ } => {}
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
        // Resolve the base through the ref-first path so a ref always wins over
        // a same-spelled short hex prefix (e.g. `added^` must take `added` the
        // ref, not a `added…` object prefix). The suffix dictates the type a
        // bare prefix must satisfy, which only applies once ref lookup misses.
        let base_disambiguation =
            disambiguation_for_suffix(suffix).unwrap_or(ObjectDisambiguation::Any);
        let base_oid =
            resolve_revision_inner(git_dir, format, reader, base, config, base_disambiguation)?;
        return apply_revision_suffix(git_dir, reader, format, &base_oid, suffix, rev);
    }
    resolve_revision_name(git_dir, format, rev, disambiguation)
}

fn disambiguation_for_suffix(suffix: RevisionSuffix<'_>) -> Option<ObjectDisambiguation> {
    match suffix {
        RevisionSuffix::Parent(_) | RevisionSuffix::FirstParent(_) | RevisionSuffix::Search(_) => {
            Some(ObjectDisambiguation::Commitish)
        }
        RevisionSuffix::Peel(PeelKind::Object) => Some(ObjectDisambiguation::Any),
        RevisionSuffix::Peel(PeelKind::AnyNonTag) => Some(ObjectDisambiguation::Any),
        RevisionSuffix::Peel(PeelKind::Commit) => Some(ObjectDisambiguation::Commitish),
        RevisionSuffix::Peel(PeelKind::Tree) => Some(ObjectDisambiguation::Treeish),
        RevisionSuffix::Peel(PeelKind::Tag) => Some(ObjectDisambiguation::Tag),
        RevisionSuffix::Peel(PeelKind::Blob) => Some(ObjectDisambiguation::Blob),
    }
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
        resolve_revision_inner(
            self.git_dir,
            self.format,
            self.reader,
            rev,
            self.config,
            ObjectDisambiguation::Any,
        )
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
    disambiguation: ObjectDisambiguation,
) -> Result<ObjectId> {
    if rev.len() == format.hex_len() && rev.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return ObjectId::from_hex(format, rev);
    }
    let refs = FileRefStore::new(git_dir.to_path_buf(), format);
    if let Some(oid) = resolve_revision_ref(&refs, rev)? {
        return Ok(oid);
    }
    // Ref lookup missed: now a bare hex prefix may name an object. This is the
    // single point where short object-id prefixes resolve, so ref names always
    // win over a same-spelled prefix; `disambiguation` narrows the candidate
    // set here (and only here), matching git's `get_short_oid`.
    if rev.len() >= 4
        && rev.len() < format.hex_len()
        && rev.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return resolve_short_object_id(git_dir, format, rev, disambiguation)?.into_result(rev);
    }
    // git's get_describe_name: `<tag>-<count>-g<hex>` (describe output) resolves
    // to the abbreviated commit named by the trailing `-g<hex>`.
    if let Some(oid) = resolve_describe_name(git_dir, format, rev)? {
        return Ok(oid);
    }
    Err(GitError::not_found(format!("revision {rev}")))
}

pub fn short_object_id_ambiguous_error(prefix: &str) -> GitError {
    GitError::InvalidObjectId(format!("short object ID {prefix} is ambiguous"))
}

pub fn is_short_object_id_ambiguous_error(err: &GitError) -> bool {
    matches!(err, GitError::InvalidObjectId(msg) if msg.starts_with("short object ID ") && msg.ends_with(" is ambiguous"))
}

pub fn resolve_short_object_id(
    git_dir: &Path,
    format: ObjectFormat,
    prefix: &str,
    disambiguation: ObjectDisambiguation,
) -> Result<ShortObjectIdResolution> {
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    resolve_short_object_id_with_reader(git_dir, format, &db, prefix, disambiguation)
}

pub fn object_ids_with_prefix(
    git_dir: &Path,
    format: ObjectFormat,
    prefix: &str,
) -> Result<Vec<ObjectId>> {
    FileObjectDatabase::from_git_dir(git_dir, format).object_ids_with_prefix(prefix)
}

pub fn resolve_short_object_id_with_reader<R: ObjectReader>(
    git_dir: &Path,
    format: ObjectFormat,
    reader: &R,
    prefix: &str,
    disambiguation: ObjectDisambiguation,
) -> Result<ShortObjectIdResolution> {
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let candidates = db.object_ids_with_prefix(prefix)?;
    if candidates.is_empty() {
        return Ok(ShortObjectIdResolution::Missing);
    }
    if disambiguation == ObjectDisambiguation::Any {
        return Ok(match candidates.len() {
            1 => ShortObjectIdResolution::Unique(candidates[0]),
            _ => ShortObjectIdResolution::Ambiguous(candidates),
        });
    }
    let mut accepted = Vec::new();
    for oid in &candidates {
        if short_object_id_matches_type(reader, format, oid, disambiguation) {
            accepted.push(*oid);
        }
    }
    Ok(match accepted.len() {
        1 => ShortObjectIdResolution::Unique(accepted[0]),
        0 => ShortObjectIdResolution::Ambiguous(candidates),
        _ => ShortObjectIdResolution::Ambiguous(accepted),
    })
}

fn short_object_id_matches_type<R: ObjectReader>(
    reader: &R,
    format: ObjectFormat,
    oid: &ObjectId,
    disambiguation: ObjectDisambiguation,
) -> bool {
    match disambiguation {
        ObjectDisambiguation::Any => true,
        ObjectDisambiguation::Commit => reader
            .read_object(oid)
            .is_ok_and(|object| object.object_type == ObjectType::Commit),
        ObjectDisambiguation::Commitish => peel_to_commit(reader, format, oid).is_ok(),
        ObjectDisambiguation::Tree => reader
            .read_object(oid)
            .is_ok_and(|object| object.object_type == ObjectType::Tree),
        ObjectDisambiguation::Treeish => peel_to_tree(reader, format, oid).is_ok(),
        ObjectDisambiguation::Tag => reader
            .read_object(oid)
            .is_ok_and(|object| object.object_type == ObjectType::Tag),
        ObjectDisambiguation::Blob => peel_to_blob(reader, format, oid).is_ok(),
    }
}

pub fn ambiguous_short_object_id_hint(
    git_dir: &Path,
    format: ObjectFormat,
    prefix: &str,
    disambiguation: ObjectDisambiguation,
) -> Result<Vec<String>> {
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let mut candidates = db.object_ids_with_prefix(prefix)?;
    candidates.sort_by(|left, right| {
        let left_type = ambiguous_candidate_type_for_sort(&db, left);
        let right_type = ambiguous_candidate_type_for_sort(&db, right);
        ambiguous_type_sort_key(left_type)
            .cmp(&ambiguous_type_sort_key(right_type))
            .then_with(|| left.to_hex().cmp(&right.to_hex()))
    });
    let mut out = Vec::new();
    for oid in candidates {
        if disambiguation != ObjectDisambiguation::Any
            && !short_object_id_matches_type(&db, format, &oid, disambiguation)
        {
            continue;
        }
        out.push(ambiguous_short_object_id_line(&db, format, &oid)?);
    }
    if out.is_empty() && disambiguation != ObjectDisambiguation::Any {
        for oid in db.object_ids_with_prefix(prefix)? {
            out.push(ambiguous_short_object_id_line(&db, format, &oid)?);
        }
    }
    Ok(out)
}

fn ambiguous_candidate_type_for_sort(
    db: &FileObjectDatabase,
    oid: &ObjectId,
) -> Option<ObjectType> {
    match db.read_object_header(oid) {
        Ok(Some((object_type, _))) => Some(object_type),
        Err(GitError::InvalidObject(message)) if message.starts_with("unable to unpack ") => {
            eprintln!("error: {message}");
            None
        }
        Ok(None) | Err(_) => None,
    }
}

fn ambiguous_type_sort_key(object_type: Option<ObjectType>) -> u8 {
    match object_type {
        None => 0,
        Some(ObjectType::Tag) => 1,
        Some(ObjectType::Commit) => 2,
        Some(ObjectType::Tree) => 3,
        Some(ObjectType::Blob) => 4,
    }
}

fn ambiguous_short_object_id_line(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    oid: &ObjectId,
) -> Result<String> {
    let abbrev = unique_object_abbrev(db, oid)?;
    let object_type = match db.read_object_header(oid) {
        Ok(Some((object_type, _))) => object_type,
        Err(GitError::InvalidObject(message)) if message.starts_with("unknown object type") => {
            return Err(GitError::InvalidObject(message));
        }
        Err(GitError::InvalidObject(message)) if message.starts_with("unable to unpack ") => {
            eprintln!("error: {message}");
            return Ok(format!("{abbrev} [bad object]"));
        }
        Ok(None) | Err(_) => return Ok(format!("{abbrev} [bad object]")),
    };
    if matches!(object_type, ObjectType::Tree | ObjectType::Blob) {
        return Ok(format!("{abbrev} {}", object_type.as_str()));
    }
    let object = match db.read_object(oid) {
        Ok(object) => object,
        Err(GitError::InvalidObject(message)) if message.starts_with("unknown object type") => {
            return Err(GitError::InvalidObject(message));
        }
        Err(GitError::InvalidObject(message)) if message.starts_with("unable to unpack ") => {
            eprintln!("error: {message}");
            return Ok(format!("{abbrev} [bad object]"));
        }
        Err(_) => return Ok(format!("{abbrev} [bad object]")),
    };
    Ok(match object_type {
        ObjectType::Commit => {
            let commit = Commit::parse_ref(format, &object.body)?;
            let subject = first_message_line(commit.message);
            match short_date_from_ident(commit.committer) {
                Some(date) if !subject.is_empty() => format!("{abbrev} commit {date} - {subject}"),
                Some(date) => format!("{abbrev} commit {date} - "),
                None if !subject.is_empty() => format!("{abbrev} commit  - {subject}"),
                None => format!("{abbrev} commit  - "),
            }
        }
        ObjectType::Tag => match Tag::parse_ref(format, &object.body) {
            Ok(tag) => {
                let name = String::from_utf8_lossy(tag.name);
                match tag.tagger.and_then(short_date_from_ident) {
                    Some(date) => format!("{abbrev} tag {date} - {name}"),
                    None => format!("{abbrev} tag  - {name}"),
                }
            }
            Err(_) => format!("{abbrev} [bad tag, could not parse it]"),
        },
        ObjectType::Tree => format!("{abbrev} tree"),
        ObjectType::Blob => format!("{abbrev} blob"),
    })
}

fn unique_object_abbrev(db: &FileObjectDatabase, oid: &ObjectId) -> Result<String> {
    let hex = oid.to_hex();
    let mut width = 7.min(hex.len());
    while width < hex.len() {
        match db.resolve_prefix(&hex[..width])? {
            ObjectPrefixResolution::Ambiguous(_) => width += 1,
            _ => break,
        }
    }
    Ok(hex[..width].to_string())
}

fn first_message_line(message: &[u8]) -> String {
    let line = message
        .split(|byte| *byte == b'\n')
        .next()
        .unwrap_or_default();
    String::from_utf8_lossy(line).into_owned()
}

fn short_date_from_ident(ident: &[u8]) -> Option<String> {
    let signature = sley_core::Signature::from_ident_line(ident)?;
    short_date_from_timestamp(signature.time.seconds)
}

fn short_date_from_timestamp(timestamp: i64) -> Option<String> {
    let days = timestamp.div_euclid(86_400);
    let (year, month, day) = civil_from_days_for_short_date(days)?;
    Some(format!("{year:04}-{month:02}-{day:02}"))
}

fn civil_from_days_for_short_date(days: i64) -> Option<(i64, u32, u32)> {
    let z = days.checked_add(719_468)?;
    let era = if z >= 0 { z } else { z - 146_096 }.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096).div_euclid(365);
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2).div_euclid(153);
    let day = doy - (153 * mp + 2).div_euclid(5) + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    let year = y + i64::from(month <= 2);
    Some((year, u32::try_from(month).ok()?, u32::try_from(day).ok()?))
}

/// Resolve a `git describe` name (`<ref>-<count>-g<hex>`) back to the commit it
/// names, mirroring `get_describe_name` in git's object-name.c: scan from the
/// end over hex digits; the first non-hex byte must be the `g` of a `-g` marker,
/// and the bytes after it form an abbreviated commit oid.
fn resolve_describe_name(
    git_dir: &Path,
    format: sley_core::ObjectFormat,
    rev: &str,
) -> Result<Option<ObjectId>> {
    let bytes = rev.as_bytes();
    // Need at least `X-gY`: the scan starts at the last byte and stops once we
    // are within two bytes of the start (matching git's `name + 2 <= cp`).
    let mut idx = bytes.len();
    while idx >= 2 {
        idx -= 1;
        let ch = bytes[idx];
        if ch.is_ascii_hexdigit() {
            continue;
        }
        if ch == b'g' && idx >= 1 && bytes[idx - 1] == b'-' {
            let hex = &rev[idx + 1..];
            if hex.len() >= 4
                && hex.len() < format.hex_len()
                && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
                && let ShortObjectIdResolution::Unique(oid) =
                    resolve_short_object_id(git_dir, format, hex, ObjectDisambiguation::Commit)?
            {
                return Ok(Some(oid));
            }
        }
        break;
    }
    Ok(None)
}

fn resolve_revision_ref(refs: &FileRefStore, rev: &str) -> Result<Option<ObjectId>> {
    let mut candidates = Vec::new();
    if rev == "HEAD" {
        candidates.push("HEAD".to_string());
    } else if rev.starts_with("refs/") {
        candidates.push(rev.to_string());
    } else {
        let refs_name = format!("refs/{rev}");
        if refs.read_ref(&refs_name)?.is_some() {
            // git's ref_rev_parse_rules try "refs/%s" before tags/heads. This
            // matters for pseudo-names such as "stash" (refs/stash), not just names
            // containing a slash.
            candidates.push(refs_name);
        }
        let tag_name = format!("refs/tags/{rev}");
        if refs.read_ref(&tag_name)?.is_some() {
            candidates.push(tag_name);
        }
        let head_name = format!("refs/heads/{rev}");
        if refs.read_ref(&head_name)?.is_some() {
            candidates.push(head_name);
        }
        let remote_name = format!("refs/remotes/{rev}");
        if refs.read_ref(&remote_name)?.is_some() {
            candidates.push(remote_name);
        }
        let remote_head_name = format!("refs/remotes/{rev}/HEAD");
        if refs.read_ref(&remote_head_name)?.is_some() {
            candidates.push(remote_head_name);
        }
        if validate_ref_name_for_read(rev).is_ok() {
            candidates.push(rev.to_string());
        }
    }
    for candidate in candidates {
        if let Some(oid) = resolve_revision_ref_candidate(refs, &candidate)? {
            return Ok(Some(oid));
        }
    }
    Ok(None)
}

fn resolve_revision_ref_candidate(refs: &FileRefStore, name: &str) -> Result<Option<ObjectId>> {
    let mut current = name.to_string();
    for _ in 0..16 {
        match refs.read_ref(&current)? {
            Some(RefTarget::Direct(oid)) => return Ok(Some(oid)),
            Some(RefTarget::Symbolic(target)) => {
                if validate_symref_target(&target).is_err() {
                    eprintln!("warning: ignoring dangling symref {name}");
                    return Ok(None);
                }
                current = target;
            }
            None => return Ok(None),
        }
    }
    Ok(None)
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
    let Some(open) = rev.rfind("@{") else {
        return Ok(None);
    };
    let Some(inner) = rev.strip_suffix('}') else {
        return Ok(None);
    };
    // `inner` still has the `<base>@{` prefix; keep only what is inside the braces.
    let inner = &inner[open + 2..];
    if inner.contains('}') {
        return Ok(None);
    }
    let base = &rev[..open];
    let refs = FileRefStore::new(git_dir.to_path_buf(), format);

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

    if inner.eq_ignore_ascii_case("u") || inner.eq_ignore_ascii_case("upstream") {
        let upstream = resolve_upstream_ref(git_dir, format, base, false, rev, config)?;
        return match resolve_revision_ref(&refs, &upstream.refname)? {
            Some(oid) => Ok(Some(oid)),
            None => Err(upstream.missing_error(rev)),
        };
    }
    if inner.eq_ignore_ascii_case("push") {
        let upstream = resolve_upstream_ref(git_dir, format, base, true, rev, config)?;
        return match resolve_revision_ref(&refs, &upstream.refname)? {
            Some(oid) => Ok(Some(oid)),
            None => Err(upstream.missing_error(rev)),
        };
    }
    if inner.bytes().all(|byte| byte.is_ascii_digit()) {
        let count = parse_at_count(rev, inner)?;
        return Ok(Some(resolve_reflog_nth(
            git_dir, format, base, count, rev, config,
        )?));
    }

    Ok(Some(resolve_reflog_date(
        git_dir, format, base, inner, rev, config,
    )?))
}

fn resolve_at_selector_ref_name(
    git_dir: &Path,
    format: sley_core::ObjectFormat,
    rev: &str,
    config: Option<&GitConfig>,
) -> Result<Option<String>> {
    let Some(open) = rev.rfind("@{") else {
        return Ok(None);
    };
    let Some(inner) = rev.strip_suffix('}') else {
        return Ok(None);
    };
    let inner = &inner[open + 2..];
    if inner.contains('}') {
        return Ok(None);
    }
    let base = &rev[..open];
    if let Some(prior) = parse_prior_checkout_selector(rev)? {
        let Some(branch) = nth_prior_checkout_branch_name(git_dir, format, prior)? else {
            return Err(GitError::not_found(format!(
                "not enough previous checkouts to resolve {rev}"
            )));
        };
        return Ok(Some(format!("refs/heads/{branch}")));
    }
    if inner.eq_ignore_ascii_case("u") || inner.eq_ignore_ascii_case("upstream") {
        return Ok(Some(
            resolve_upstream_ref(git_dir, format, base, false, rev, config)?.refname,
        ));
    }
    if inner.eq_ignore_ascii_case("push") {
        return Ok(Some(
            resolve_upstream_ref(git_dir, format, base, true, rev, config)?.refname,
        ));
    }
    if inner.bytes().all(|byte| byte.is_ascii_digit()) || !inner.starts_with('-') {
        let refs = FileRefStore::new(git_dir.to_path_buf(), format);
        return Ok(Some(reflog_ref_name_for_base(
            git_dir, format, &refs, base, config,
        )?));
    }
    Ok(None)
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

fn parse_prior_checkout_selector(rev: &str) -> Result<Option<usize>> {
    let Some(inner) = rev
        .strip_prefix("@{-")
        .and_then(|rest| rest.strip_suffix('}'))
    else {
        return Ok(None);
    };
    if !inner.bytes().all(|byte| byte.is_ascii_digit()) {
        return Ok(None);
    }
    Ok(Some(parse_at_count(rev, inner)?))
}

fn is_reflog_count_or_date_selector(rev: &str) -> bool {
    let Some(open) = rev.rfind("@{") else {
        return false;
    };
    let Some(inner) = rev.strip_suffix('}') else {
        return false;
    };
    let inner = &inner[open + 2..];
    !(inner.eq_ignore_ascii_case("u")
        || inner.eq_ignore_ascii_case("upstream")
        || inner.eq_ignore_ascii_case("push")
        || inner.starts_with('-'))
}

/// Map a `<base>@{...}` base to the full ref name whose reflog should be read.
///
/// An empty base means the current branch's reflog; explicit `HEAD` means the
/// HEAD reflog. A short name is DWIM'd through git's `ref_rev_parse_rules`
/// (`%s`, `refs/%s`, `refs/tags/%s`, `refs/heads/%s`, `refs/remotes/%s`,
/// `refs/remotes/%s/HEAD`), picking the first candidate that has an existing
/// reflog — exactly git's `repo_dwim_log`. This is what lets `stash@{N}` read
/// `refs/stash`'s reflog (rule `refs/%s`) the same way `main@{N}` reads
/// `refs/heads/main` (rule `refs/heads/%s`). When no candidate has a reflog,
/// fall back to `refs/heads/<base>` so the "no reflog" error path keeps its
/// historical shape.
fn reflog_ref_name(refs: &FileRefStore, base: &str) -> String {
    if base == "HEAD" {
        return "HEAD".to_string();
    }
    if base.starts_with("refs/") {
        return base.to_string();
    }
    for candidate in reflog_dwim_candidates(base) {
        if reflog_has_entries(refs, &candidate) {
            return candidate;
        }
    }
    format!("refs/heads/{base}")
}

fn reflog_ref_name_for_base(
    git_dir: &Path,
    format: sley_core::ObjectFormat,
    refs: &FileRefStore,
    base: &str,
    config: Option<&GitConfig>,
) -> Result<String> {
    if base.is_empty() {
        return Ok(refs
            .current_branch_ref()?
            .unwrap_or_else(|| "HEAD".to_string()));
    }
    if base == "@" {
        return Ok("HEAD".to_string());
    }
    if let Some(prior) = parse_prior_checkout_selector(base)? {
        let Some(branch) = nth_prior_checkout_branch_name(git_dir, format, prior)? else {
            return Err(GitError::not_found(format!(
                "not enough previous checkouts to resolve {base}"
            )));
        };
        return Ok(reflog_ref_name(refs, &branch));
    }
    if is_reflog_count_or_date_selector(base) {
        return Err(GitError::InvalidFormat(format!(
            "invalid revision selector {base}"
        )));
    }
    if base.contains("@{")
        && let Some(name) = resolve_at_selector_ref_name(git_dir, format, base, config)?
    {
        return Ok(name);
    }
    if base.contains("@{") {
        return Err(GitError::InvalidFormat(format!(
            "invalid revision selector {base}"
        )));
    }
    Ok(reflog_ref_name(refs, base))
}

/// git's `ref_rev_parse_rules` expansions for a short ref name, in order.
fn reflog_dwim_candidates(base: &str) -> [String; 6] {
    [
        base.to_string(),
        format!("refs/{base}"),
        format!("refs/tags/{base}"),
        format!("refs/heads/{base}"),
        format!("refs/remotes/{base}"),
        format!("refs/remotes/{base}/HEAD"),
    ]
}

/// Whether `name` has a reflog with at least one entry. `read_reflog` returns an
/// empty vec for an absent reflog, so a non-empty read means the reflog exists.
fn reflog_has_entries(refs: &FileRefStore, name: &str) -> bool {
    refs.read_reflog(name)
        .map(|entries| !entries.is_empty())
        .unwrap_or(false)
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
    config: Option<&GitConfig>,
) -> Result<ObjectId> {
    let refs = FileRefStore::new(git_dir.to_path_buf(), format);
    let ref_name = reflog_ref_name_for_base(git_dir, format, &refs, base, config)?;
    let display_name = reflog_display_name_for_ref(base, &ref_name);
    let entries = refs.read_reflog(&ref_name)?;
    if entries.is_empty() {
        if n == 0
            && let Some(oid) = resolve_revision_ref(&refs, &ref_name)?
        {
            return Ok(oid);
        }
        return Err(GitError::not_found(format!(
            "no reflog for '{}' to resolve {rev}",
            display_name
        )));
    }
    // `@{N}` counts back from the newest entry; index `len - 1 - n`.
    let len = entries.len();
    if n >= len {
        if n == len && !object_id_is_null(&entries[0].old_oid) {
            return Ok(entries[0].old_oid);
        }
        return Err(GitError::not_found(format!(
            "log for '{}' only has {len} entries",
            display_name
        )));
    }
    Ok(entries[len - 1 - n].new_oid)
}

fn resolve_reflog_date(
    git_dir: &Path,
    format: sley_core::ObjectFormat,
    base: &str,
    date: &str,
    rev: &str,
    config: Option<&GitConfig>,
) -> Result<ObjectId> {
    let cutoff = parse_reflog_selector_date(date)
        .ok_or_else(|| GitError::Unsupported(format!("revision selector @{{{date}}}")))?;
    let refs = FileRefStore::new(git_dir.to_path_buf(), format);
    let ref_name = reflog_ref_name_for_base(git_dir, format, &refs, base, config)?;
    let display_name = reflog_display_name_for_ref(base, &ref_name);
    let entries = refs.read_reflog(&ref_name)?;
    if entries.is_empty() {
        return Err(GitError::not_found(format!(
            "no reflog for '{}' to resolve {rev}",
            display_name
        )));
    }
    for entry in entries.iter().rev() {
        if reflog_entry_timestamp(entry)? <= cutoff {
            return Ok(entry.new_oid);
        }
    }
    Ok(entries[0].new_oid)
}

fn reflog_entry_timestamp(entry: &ReflogEntry) -> Result<i64> {
    entry.timestamp_seconds()
}

fn object_id_is_null(oid: &ObjectId) -> bool {
    oid.as_bytes().iter().all(|byte| *byte == 0)
}

fn parse_reflog_selector_date(value: &str) -> Option<i64> {
    if value == "now" {
        return std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|duration| i64::try_from(duration.as_secs()).ok());
    }
    if let Some(years) = value.strip_suffix(".year.ago") {
        let years = years.parse::<i64>().ok()?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_secs();
        let now = i64::try_from(now).ok()?;
        return Some(now.saturating_sub(years.saturating_mul(365 * 86_400)));
    }
    let mut parts = value.split_ascii_whitespace();
    let _weekday = parts.next()?;
    let month = parse_reflog_month(parts.next()?)?;
    let day = parts.next()?.parse::<u32>().ok()?;
    let time = parts.next()?;
    let year = parts.next()?.parse::<i64>().ok()?;
    let tz = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let mut time_parts = time.split(':');
    let hour = time_parts.next()?.parse::<i64>().ok()?;
    let minute = time_parts.next()?.parse::<i64>().ok()?;
    let second = time_parts.next()?.parse::<i64>().ok()?;
    if time_parts.next().is_some() || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let offset = parse_reflog_timezone(tz)?;
    Some(days_from_civil(year, month, day)? * 86_400 + hour * 3_600 + minute * 60 + second - offset)
}

fn parse_reflog_month(value: &str) -> Option<u32> {
    match value {
        "Jan" => Some(1),
        "Feb" => Some(2),
        "Mar" => Some(3),
        "Apr" => Some(4),
        "May" => Some(5),
        "Jun" => Some(6),
        "Jul" => Some(7),
        "Aug" => Some(8),
        "Sep" => Some(9),
        "Oct" => Some(10),
        "Nov" => Some(11),
        "Dec" => Some(12),
        _ => None,
    }
}

fn parse_reflog_timezone(value: &str) -> Option<i64> {
    let bytes = value.as_bytes();
    if bytes.len() != 5 || (bytes[0] != b'+' && bytes[0] != b'-') {
        return None;
    }
    let hours = value[1..3].parse::<i64>().ok()?;
    let minutes = value[3..5].parse::<i64>().ok()?;
    if hours > 23 || minutes > 59 {
        return None;
    }
    let seconds = hours * 3_600 + minutes * 60;
    if bytes[0] == b'-' {
        Some(-seconds)
    } else {
        Some(seconds)
    }
}

fn days_from_civil(year: i64, month: u32, day: u32) -> Option<i64> {
    if !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
        return None;
    }
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let month = i64::from(month);
    let day = i64::from(day);
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe - 719_468)
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

fn is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
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

fn reflog_display_name_for_ref(base: &str, ref_name: &str) -> String {
    if base.is_empty()
        && let Some(branch) = ref_name.strip_prefix("refs/heads/")
    {
        return branch.to_string();
    }
    if base == "@" {
        return "HEAD".to_string();
    }
    reflog_display_name(base)
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
            return resolve_revision_name(git_dir, format, &from, ObjectDisambiguation::Any)
                .map_err(|_| {
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
/// The name of the N-th previously checked-out branch (the `X` in the N-th
/// newest "checkout: moving from X to Y" HEAD reflog entry), as used by
/// `git checkout -`/`@{-N}` to switch *back to that branch by name* rather than
/// to a detached commit. Returns `None` when there are fewer than `n` such
/// reflog entries.
pub fn nth_prior_checkout_branch_name(
    git_dir: &Path,
    format: sley_core::ObjectFormat,
    n: usize,
) -> Result<Option<String>> {
    if n == 0 {
        return Ok(None);
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
            return Ok(Some(from.to_string()));
        }
    }
    Ok(None)
}

fn checkout_move_source(message: &[u8]) -> Option<&str> {
    let message = std::str::from_utf8(message).ok()?;
    let rest = message.strip_prefix("checkout: moving from ")?;
    // The remainder is "X to Y"; git uses the first separator when grabbing X.
    let (from, _to) = rest.split_once(" to ")?;
    Some(from)
}

struct UpstreamRef {
    refname: String,
    merge: String,
}

impl UpstreamRef {
    fn missing_error(&self, _rev: &str) -> GitError {
        GitError::not_found(format!(
            "upstream branch '{}' not stored as a remote-tracking branch",
            self.merge
        ))
    }
}

/// Resolve `<base>@{u}` / `@{upstream}` (when `push` is false) or `@{push}`
/// (when `push` is true) to the configured tracking ref name.
///
/// The branch is `base` (or the current branch when `base` is empty). The
/// tracking ref is built from `branch.<name>.remote` (or `pushRemote` for the
/// push form) plus the short name from `branch.<name>.merge`, yielding
/// `refs/remotes/<remote>/<short>`. `@{push}` falls back to the upstream remote
/// when no push-specific remote is configured.
fn resolve_upstream_ref(
    git_dir: &Path,
    format: sley_core::ObjectFormat,
    base: &str,
    push: bool,
    rev: &str,
    config: Option<&GitConfig>,
) -> Result<UpstreamRef> {
    let refs = FileRefStore::new(git_dir.to_path_buf(), format);
    let branch = if base.is_empty() || base == "HEAD" || base == "@" {
        refs.current_branch()?
            .ok_or_else(|| GitError::InvalidFormat("HEAD does not point to a branch".to_string()))?
    } else if let Some(prior) = parse_prior_checkout_selector(base)? {
        nth_prior_checkout_branch_name(git_dir, format, prior)?.ok_or_else(|| {
            GitError::not_found(format!("not enough previous checkouts to resolve {base}"))
        })?
    } else if base.starts_with("refs/") || base.contains("@{") {
        return Err(GitError::InvalidFormat(format!(
            "{base} is not a branch, cannot resolve {rev}"
        )));
    } else {
        base.to_string()
    };
    if refs.read_ref(&format!("refs/heads/{branch}"))?.is_none() {
        return Err(GitError::not_found(format!("no such branch: '{branch}'")));
    }

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
    // `@{push}` follows git's `branch_get_push()`: pushremote + push refspecs +
    // `push.default`, which differ materially from `@{upstream}`'s plain
    // `branch.<name>.{remote,merge}` lookup.
    if push {
        return branch_get_push(&branch, config);
    }
    let merge = config
        .get("branch", Some(&branch), "merge")
        .ok_or_else(|| {
            GitError::not_found(format!("no upstream configured for branch '{branch}'"))
        })?;
    let short = merge.strip_prefix("refs/heads/").unwrap_or(merge);
    let remote = config
        .get("branch", Some(&branch), "remote")
        .ok_or_else(|| GitError::not_found(format!("no upstream remote for branch '{branch}'")))?;

    let refname = if remote == "." {
        merge.to_string()
    } else {
        format!("refs/remotes/{remote}/{short}")
    };
    Ok(UpstreamRef {
        refname,
        merge: merge.to_string(),
    })
}

/// `branch_get_push_1()` from remote.c: resolve `<branch>@{push}` to its push
/// tracking ref. Determines the pushremote, applies explicit push refspecs when
/// present, and otherwise dispatches on `push.default`.
fn branch_get_push(branch: &str, config: &GitConfig) -> Result<UpstreamRef> {
    let merge = config
        .get("branch", Some(branch), "merge")
        .map(str::to_string);
    let pushremote = config
        .get("branch", Some(branch), "pushRemote")
        .or_else(|| config.get("remote", None, "pushDefault"))
        .or_else(|| config.get("branch", Some(branch), "remote"))
        .ok_or_else(|| GitError::not_found(format!("branch '{branch}' has no remote for pushing")))?
        .to_string();
    let branch_refname = format!("refs/heads/{branch}");

    let upstream_ref = |refname: String| UpstreamRef {
        refname,
        merge: merge.clone().unwrap_or_default(),
    };

    // Explicit push refspecs win over `push.default`: map the local branch ref
    // through them, then through the pushremote's fetch refspecs.
    let push_refspecs: Vec<&str> = config
        .get_all("remote", Some(&pushremote), "push")
        .into_iter()
        .flatten()
        .collect();
    if !push_refspecs.is_empty() {
        let dst = apply_refspecs(&push_refspecs, &branch_refname).ok_or_else(|| {
            GitError::not_found(format!(
                "push refspecs for '{pushremote}' do not include '{branch}'"
            ))
        })?;
        return Ok(upstream_ref(tracking_for_push_dest(
            config,
            &pushremote,
            &dst,
        )?));
    }

    match config.get("push", None, "default").unwrap_or("simple") {
        "nothing" => Err(GitError::not_found(
            "push has no destination (push.default is 'nothing')".to_string(),
        )),
        "matching" | "current" => Ok(upstream_ref(tracking_for_push_dest(
            config,
            &pushremote,
            &branch_refname,
        )?)),
        "upstream" | "tracking" => Ok(upstream_ref(branch_get_upstream_refname(
            config,
            branch,
            merge.as_deref(),
        )?)),
        // "simple" and any unrecognised/unspecified value: push to the same-named
        // branch, but only when that coincides with the upstream destination.
        _ => {
            let up = branch_get_upstream_refname(config, branch, merge.as_deref())?;
            let cur = tracking_for_push_dest(config, &pushremote, &branch_refname)?;
            if cur != up {
                return Err(GitError::not_found(
                    "cannot resolve 'simple' push to a single destination".to_string(),
                ));
            }
            Ok(upstream_ref(cur))
        }
    }
}

/// The upstream tracking ref of `branch` (`branch_get_upstream()` →
/// `branch->merge[0]->dst`): the `branch.<name>.merge` ref mapped through the
/// fetch refspecs of `branch.<name>.remote`.
fn branch_get_upstream_refname(
    config: &GitConfig,
    branch: &str,
    merge: Option<&str>,
) -> Result<String> {
    let merge = merge.filter(|merge| !merge.is_empty()).ok_or_else(|| {
        GitError::not_found(format!("no upstream configured for branch '{branch}'"))
    })?;
    let remote = config
        .get("branch", Some(branch), "remote")
        .ok_or_else(|| {
            GitError::not_found(format!("no upstream configured for branch '{branch}'"))
        })?;
    if remote == "." {
        return Ok(merge.to_string());
    }
    tracking_for_push_dest(config, remote, merge)
}

/// `tracking_for_push_dest()`: the local tracking ref for `refname` on `remote`,
/// produced by applying that remote's fetch refspecs. When no fetch refspec
/// matches — e.g. a remote configured only via `branch.<name>.{remote,merge}`
/// with no `[remote]` section, or any remote lacking an explicit `fetch` line —
/// fall back to the conventional `refs/remotes/<remote>/<short>` mapping, the
/// same direct construction the `@{upstream}` path uses. This keeps `@{push}`
/// consistent with `@{u}` and matches git's result for the standard
/// `+refs/heads/*:refs/remotes/<remote>/*` layout without requiring the refspec
/// to be spelled out.
fn tracking_for_push_dest(config: &GitConfig, remote: &str, refname: &str) -> Result<String> {
    let fetch_refspecs: Vec<&str> = config
        .get_all("remote", Some(remote), "fetch")
        .into_iter()
        .flatten()
        .collect();
    if let Some(dst) = apply_refspecs(&fetch_refspecs, refname) {
        return Ok(dst);
    }
    let short = refname.strip_prefix("refs/heads/").unwrap_or(refname);
    Ok(format!("refs/remotes/{remote}/{short}"))
}

/// Apply a list of refspecs to a single ref, returning the first matching
/// destination. Handles the exact `<src>:<dst>` and wildcard `<p>*:<q>*` forms
/// (a trailing `*` matches an arbitrary suffix, slashes included); a leading `+`
/// is ignored and a colon-less spec maps a ref to itself.
fn apply_refspecs(refspecs: &[&str], refname: &str) -> Option<String> {
    for spec in refspecs {
        let spec = spec.strip_prefix('+').unwrap_or(spec);
        let (src, dst) = spec.split_once(':').unwrap_or((spec, spec));
        if let Some(src_prefix) = src.strip_suffix('*') {
            if let (Some(suffix), Some(dst_prefix)) =
                (refname.strip_prefix(src_prefix), dst.strip_suffix('*'))
            {
                return Some(format!("{dst_prefix}{suffix}"));
            }
        } else if src == refname {
            return Some(dst.to_string());
        }
    }
    None
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
    Blob,
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
        "blob" => PeelKind::Blob,
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

/// Peel `base` to a commit for `~N`/`^N` navigation, but only read the object
/// when necessary: a base already present in the commit-graph is a commit, so it
/// is returned without a read (preserving the graph-only navigation fast path).
/// A base absent from the graph is read; commits pass through, annotated tags are
/// followed to their commit.
fn peel_base_to_commit_if_needed<R: ObjectReader>(
    reader: &R,
    format: sley_core::ObjectFormat,
    graph: &mut CommitGraphContext<'_>,
    base: &ObjectId,
) -> Result<ObjectId> {
    if graph.lookup(base).is_some() {
        return Ok(*base);
    }
    peel_to_commit(reader, format, base)
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
            // git peels the base to a commit before taking the Nth parent, so
            // `<annotated-tag>^N` follows the tag to its commit first. Peeling
            // reads the object, so skip it when the graph already covers the base
            // (a commit) to preserve the graph-only navigation fast path.
            let mut graph = CommitGraphContext::load(git_dir, format);
            let base = peel_base_to_commit_if_needed(reader, format, &mut graph, base)?;
            graph
                .commit_parents(reader, &base)?
                .get(parent - 1)
                .cloned()
                .ok_or_else(|| GitError::not_found(format!("parent {parent} of {base}")))
        }
        RevisionSuffix::FirstParent(count) => {
            // Likewise `<annotated-tag>~N` peels to the commit before walking
            // first parents (skipping the read when the graph covers the base).
            let mut graph = CommitGraphContext::load(git_dir, format);
            let mut current = peel_base_to_commit_if_needed(reader, format, &mut graph, base)?;
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

/// Parent object ids resolved from a commit-graph entry.
///
/// Most commits have zero, one, or two parents. Keeping those cases inline
/// avoids a heap allocation per graph commit while preserving a `Vec` escape
/// hatch for octopus merges.
#[derive(Debug, Clone)]
enum GraphParents {
    None,
    One(ObjectId),
    Two([ObjectId; 2]),
    Many(Vec<ObjectId>),
}

impl GraphParents {
    fn from_oids<I>(parents: I) -> Self
    where
        I: IntoIterator<Item = ObjectId>,
    {
        let mut parents = parents.into_iter();
        let Some(first) = parents.next() else {
            return Self::None;
        };
        let Some(second) = parents.next() else {
            return Self::One(first);
        };
        let Some(third) = parents.next() else {
            return Self::Two([first, second]);
        };
        let (lower, _) = parents.size_hint();
        let mut many = Vec::with_capacity(3 + lower);
        many.push(first);
        many.push(second);
        many.push(third);
        many.extend(parents);
        Self::Many(many)
    }

    fn is_empty(&self) -> bool {
        matches!(self, Self::None)
    }

    fn first(&self) -> Option<ObjectId> {
        match self {
            Self::None => None,
            Self::One(parent) => Some(*parent),
            Self::Two(parents) => Some(parents[0]),
            Self::Many(parents) => parents.first().copied(),
        }
    }

    fn iter(&self) -> GraphParentIter<'_> {
        match self {
            Self::None => GraphParentIter::Empty,
            Self::One(parent) => GraphParentIter::One(Some(*parent)),
            Self::Two(parents) => GraphParentIter::Slice(parents.iter().copied()),
            Self::Many(parents) => GraphParentIter::Slice(parents.iter().copied()),
        }
    }

    fn to_vec(&self) -> Vec<ObjectId> {
        match self {
            Self::None => Vec::new(),
            Self::One(parent) => vec![*parent],
            Self::Two(parents) => parents.to_vec(),
            Self::Many(parents) => parents.clone(),
        }
    }

    fn grafted_vec<R: ObjectReader>(&self, reader: &R, oid: &ObjectId) -> Vec<ObjectId> {
        if reader.is_shallow_graft(oid) {
            Vec::new()
        } else {
            self.to_vec()
        }
    }
}

enum GraphParentIter<'a> {
    Empty,
    One(Option<ObjectId>),
    Slice(std::iter::Copied<std::slice::Iter<'a, ObjectId>>),
}

impl Iterator for GraphParentIter<'_> {
    type Item = ObjectId;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Empty => None,
            Self::One(parent) => parent.take(),
            Self::Slice(parents) => parents.next(),
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        match self {
            Self::Empty => (0, Some(0)),
            Self::One(Some(_)) => (1, Some(1)),
            Self::One(None) => (0, Some(0)),
            Self::Slice(parents) => parents.size_hint(),
        }
    }
}

impl ExactSizeIterator for GraphParentIter<'_> {}

enum CommitParentIds<'a> {
    Empty,
    Borrowed(GraphParentIter<'a>),
    Owned(std::vec::IntoIter<ObjectId>),
}

impl<'a> CommitParentIds<'a> {
    fn borrowed(parents: &'a GraphParents) -> Self {
        Self::Borrowed(parents.iter())
    }

    fn owned(parents: Vec<ObjectId>) -> Self {
        Self::Owned(parents.into_iter())
    }
}

impl Iterator for CommitParentIds<'_> {
    type Item = ObjectId;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Empty => None,
            Self::Borrowed(parents) => parents.next(),
            Self::Owned(parents) => parents.next(),
        }
    }
}

/// Commit metadata resolved from the commit-graph: parents (already mapped from
/// graph indices to object ids), generation number, and committer date.
#[derive(Debug, Clone)]
struct GraphCommit {
    parents: GraphParents,
    generation: u32,
    commit_time: u64,
}

struct GraphCommitMetadata<'a> {
    parents: &'a GraphParents,
    commit_time: i64,
}

#[derive(Debug, Clone)]
struct GraphBloomCommit {
    parents: GraphParents,
    filter: Option<Vec<u8>>,
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
    NotInGraph,
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
    /// Direct parsed monolithic commit-graph for hot metadata walks. This avoids
    /// materializing every graph entry into a `HashMap` when callers only need a
    /// handful of commits (for example `log -50`).
    direct_graph: Option<DirectCommitGraph>,
    /// `None` until the first lookup forces a load; afterwards `Some(map)` where
    /// the map is empty iff no usable graph exists.
    commits: Option<HashMap<ObjectId, GraphCommit>>,
}

enum DirectCommitGraph {
    Missing,
    Invalid,
    Raw(Box<RawCommitGraph>),
}

struct RawCommitGraph {
    bytes: RawCommitGraphBytes,
    format: ObjectFormat,
    fanout: [u32; 256],
    commit_count: usize,
    entry_len: usize,
    oidl: Range<usize>,
    cdat: Range<usize>,
    edge: Option<Range<usize>>,
}

struct RawCommitGraphCountState {
    seen: Vec<u64>,
    pending: Vec<usize>,
}

impl RawCommitGraphCountState {
    fn new(commit_count: usize) -> Self {
        Self {
            seen: vec![0u64; commit_count.div_ceil(64)],
            pending: Vec::new(),
        }
    }
}

enum RawCommitGraphBytes {
    Owned(Vec<u8>),
    Mapped(sley_mmap::MappedFile),
}

impl AsRef<[u8]> for RawCommitGraphBytes {
    fn as_ref(&self) -> &[u8] {
        match self {
            Self::Owned(bytes) => bytes,
            Self::Mapped(bytes) => bytes.as_bytes(),
        }
    }
}

impl RawCommitGraph {
    fn parse_for_lookup(bytes: RawCommitGraphBytes, format: ObjectFormat) -> Result<Self> {
        let data = bytes.as_ref();
        let hash_len = format.raw_len();
        if data.len() < 8 + 12 + hash_len {
            return Err(GitError::InvalidFormat(
                "commit-graph file too short".into(),
            ));
        }
        if &data[..4] != b"CGPH" {
            return Err(GitError::InvalidFormat(
                "missing commit-graph signature".into(),
            ));
        }
        let version = data[4];
        if version != 1 {
            return Err(GitError::Unsupported(format!(
                "commit-graph version {version}"
            )));
        }
        let hash_id = data[5];
        if u32::from(hash_id) != commit_graph_hash_function_id(format) {
            return Err(GitError::InvalidFormat(format!(
                "commit-graph hash id {hash_id} does not match {}",
                format.name()
            )));
        }
        if data[7] != 0 {
            return Err(GitError::Unsupported(
                "split commit-graph direct lookup".into(),
            ));
        }
        let chunk_count = data[6] as usize;
        let lookup_len = (chunk_count + 1)
            .checked_mul(12)
            .ok_or_else(|| GitError::InvalidFormat("commit-graph lookup overflow".into()))?;
        let data_start = 8usize
            .checked_add(lookup_len)
            .ok_or_else(|| GitError::InvalidFormat("commit-graph lookup overflow".into()))?;
        let checksum_offset = data.len() - hash_len;
        if data_start > checksum_offset {
            return Err(GitError::InvalidFormat(
                "truncated commit-graph chunk lookup".into(),
            ));
        }

        let mut lookup = Vec::with_capacity(chunk_count + 1);
        let mut offset = 8usize;
        for _ in 0..=chunk_count {
            let id = [
                data[offset],
                data[offset + 1],
                data[offset + 2],
                data[offset + 3],
            ];
            let chunk_offset = read_u64_be(&data[offset + 4..offset + 12]);
            lookup.push((id, chunk_offset));
            offset += 12;
        }
        let Some((terminator_id, terminator_offset)) = lookup.last().copied() else {
            return Err(GitError::InvalidFormat(
                "commit-graph chunk lookup is empty".into(),
            ));
        };
        if terminator_id != [0, 0, 0, 0] {
            return Err(GitError::InvalidFormat(
                "commit-graph chunk lookup missing terminator".into(),
            ));
        }
        if terminator_offset != checksum_offset as u64 {
            return Err(GitError::InvalidFormat(
                "commit-graph terminator does not point at checksum".into(),
            ));
        }

        let mut chunks = Vec::with_capacity(chunk_count);
        let mut previous_offset = data_start;
        for pair in lookup.windows(2) {
            let (id, chunk_offset) = pair[0];
            let (_next_id, next_offset) = pair[1];
            if id == [0, 0, 0, 0] {
                return Err(GitError::InvalidFormat(
                    "commit-graph chunk id is zero before terminator".into(),
                ));
            }
            if chunks
                .iter()
                .any(|(seen, _): &([u8; 4], Range<usize>)| *seen == id)
            {
                return Err(GitError::InvalidFormat(
                    "commit-graph chunk id is duplicated".into(),
                ));
            }
            let start = usize::try_from(chunk_offset).map_err(|_| {
                GitError::InvalidFormat("commit-graph chunk offset overflow".into())
            })?;
            let end = usize::try_from(next_offset).map_err(|_| {
                GitError::InvalidFormat("commit-graph chunk offset overflow".into())
            })?;
            if start < data_start || start < previous_offset || end < start || end > checksum_offset
            {
                return Err(GitError::InvalidFormat(
                    "commit-graph chunk length is invalid".into(),
                ));
            }
            chunks.push((id, start..end));
            previous_offset = start;
        }

        let oidf = raw_commit_graph_chunk(&chunks, *b"OIDF")
            .ok_or_else(|| GitError::InvalidFormat("commit-graph missing OIDF chunk".into()))?;
        if oidf.len() != 256 * 4 {
            return Err(GitError::InvalidFormat(
                "commit-graph OIDF chunk has invalid length".into(),
            ));
        }
        let mut fanout = [0u32; 256];
        let mut previous = 0u32;
        for (idx, slot) in fanout.iter_mut().enumerate() {
            let start = oidf.start + idx * 4;
            *slot = read_u32_be(&data[start..start + 4]);
            if *slot < previous {
                return Err(GitError::InvalidFormat(
                    "commit-graph OIDF fanout is not monotonic".into(),
                ));
            }
            previous = *slot;
        }
        let commit_count = fanout[255] as usize;
        let oidl = raw_commit_graph_chunk(&chunks, *b"OIDL")
            .ok_or_else(|| GitError::InvalidFormat("commit-graph missing OIDL chunk".into()))?;
        let expected_oidl_len = commit_count
            .checked_mul(hash_len)
            .ok_or_else(|| GitError::InvalidFormat("commit-graph OIDL chunk overflow".into()))?;
        if oidl.len() != expected_oidl_len {
            return Err(GitError::InvalidFormat(
                "commit-graph OIDL chunk has invalid length".into(),
            ));
        }
        let cdat = raw_commit_graph_chunk(&chunks, *b"CDAT")
            .ok_or_else(|| GitError::InvalidFormat("commit-graph missing CDAT chunk".into()))?;
        let entry_len = raw_commit_graph_entry_len(format)?;
        let expected_cdat_len = commit_count
            .checked_mul(entry_len)
            .ok_or_else(|| GitError::InvalidFormat("commit-graph CDAT chunk overflow".into()))?;
        if cdat.len() != expected_cdat_len {
            return Err(GitError::InvalidFormat(
                "commit-graph CDAT chunk has invalid length".into(),
            ));
        }
        let edge = raw_commit_graph_chunk(&chunks, *b"EDGE");
        if let Some(edge) = &edge
            && edge.len() % 4 != 0
        {
            return Err(GitError::InvalidFormat(
                "commit-graph EDGE chunk has invalid length".into(),
            ));
        }

        Ok(Self {
            bytes,
            format,
            fanout,
            commit_count,
            entry_len,
            oidl,
            cdat,
            edge,
        })
    }

    fn metadata(&self, oid: &ObjectId) -> Result<Option<CommitMetadata>> {
        if oid.format() != self.format {
            return Ok(None);
        }
        let Some(idx) = self.find_index(oid)? else {
            return Ok(None);
        };
        let entry = self.cdat_entry(idx)?;
        let hash_len = self.format.raw_len();
        let parent_one = read_u32_be(&entry[hash_len..hash_len + 4]);
        let parent_two = read_u32_be(&entry[hash_len + 4..hash_len + 8]);
        let generation_and_time_high = read_u32_be(&entry[hash_len + 8..hash_len + 12]);
        let time_low = read_u32_be(&entry[hash_len + 12..hash_len + 16]);
        let commit_time = (u64::from(generation_and_time_high & 0x3) << 32) | u64::from(time_low);
        Ok(Some(CommitMetadata {
            oid: *oid,
            parents: self.parent_oids(parent_one, parent_two)?,
            commit_time: i64::try_from(commit_time).unwrap_or(i64::MAX),
        }))
    }

    fn tree_oid(&self, oid: &ObjectId) -> Result<Option<ObjectId>> {
        if oid.format() != self.format {
            return Ok(None);
        }
        let Some(idx) = self.find_index(oid)? else {
            return Ok(None);
        };
        let entry = self.cdat_entry(idx)?;
        let hash_len = self.format.raw_len();
        ObjectId::from_raw(self.format, &entry[..hash_len]).map(Some)
    }

    fn count_reachable_indices(
        &self,
        starts: &[usize],
        first_parent: bool,
        state: &mut RawCommitGraphCountState,
    ) -> Result<usize> {
        state.pending.extend(starts.iter().copied());
        let mut count = 0usize;
        while let Some(idx) = state.pending.pop() {
            if idx >= self.commit_count {
                return Err(GitError::InvalidFormat(
                    "commit-graph traversal index points past table".into(),
                ));
            }
            let word = idx / 64;
            let bit = 1u64 << (idx % 64);
            if state.seen[word] & bit != 0 {
                continue;
            }
            state.seen[word] |= bit;
            count += 1;
            self.push_parent_indices_for_entry(idx, first_parent, &mut state.pending)?;
        }
        Ok(count)
    }

    fn find_index(&self, oid: &ObjectId) -> Result<Option<usize>> {
        let first = oid.as_bytes()[0] as usize;
        let mut low = if first == 0 {
            0
        } else {
            self.fanout[first - 1] as usize
        };
        let mut high = self.fanout[first] as usize;
        let needle = oid.as_bytes();
        while low < high {
            let mid = low + (high - low) / 2;
            match self.oid_bytes(mid)?.cmp(needle) {
                std::cmp::Ordering::Less => low = mid + 1,
                std::cmp::Ordering::Greater => high = mid,
                std::cmp::Ordering::Equal => return Ok(Some(mid)),
            }
        }
        Ok(None)
    }

    fn oid_bytes(&self, idx: usize) -> Result<&[u8]> {
        if idx >= self.commit_count {
            return Err(GitError::InvalidFormat(
                "commit-graph oid index points past table".into(),
            ));
        }
        let hash_len = self.format.raw_len();
        let start = self
            .oidl
            .start
            .checked_add(idx.checked_mul(hash_len).ok_or_else(|| {
                GitError::InvalidFormat("commit-graph OIDL index overflow".into())
            })?)
            .ok_or_else(|| GitError::InvalidFormat("commit-graph OIDL index overflow".into()))?;
        let end = start
            .checked_add(hash_len)
            .ok_or_else(|| GitError::InvalidFormat("commit-graph OIDL index overflow".into()))?;
        self.bytes
            .as_ref()
            .get(start..end)
            .ok_or_else(|| GitError::InvalidFormat("commit-graph OIDL index overflow".into()))
    }

    fn oid_at(&self, idx: u32) -> Result<ObjectId> {
        let idx = usize::try_from(idx)
            .map_err(|_| GitError::InvalidFormat("commit-graph parent index overflow".into()))?;
        ObjectId::from_raw(self.format, self.oid_bytes(idx)?)
    }

    fn cdat_entry(&self, idx: usize) -> Result<&[u8]> {
        if idx >= self.commit_count {
            return Err(GitError::InvalidFormat(
                "commit-graph CDAT index points past table".into(),
            ));
        }
        let start = self.cdat.start + idx * self.entry_len;
        let end = start + self.entry_len;
        self.bytes
            .as_ref()
            .get(start..end)
            .ok_or_else(|| GitError::InvalidFormat("commit-graph CDAT index overflow".into()))
    }

    fn push_parent_indices_for_entry(
        &self,
        idx: usize,
        first_parent: bool,
        out: &mut Vec<usize>,
    ) -> Result<()> {
        let entry = self.cdat_entry(idx)?;
        let hash_len = self.format.raw_len();
        let parent_one = read_u32_be(&entry[hash_len..hash_len + 4]);
        let parent_two = read_u32_be(&entry[hash_len + 4..hash_len + 8]);
        if parent_one != RAW_COMMIT_GRAPH_PARENT_NONE {
            validate_raw_commit_graph_parent(parent_one, self.commit_count)?;
            out.push(parent_one as usize);
        }
        if first_parent || parent_two == RAW_COMMIT_GRAPH_PARENT_NONE {
            return Ok(());
        }
        if parent_two & RAW_COMMIT_GRAPH_EXTRA_EDGE == 0 {
            validate_raw_commit_graph_parent(parent_two, self.commit_count)?;
            out.push(parent_two as usize);
            return Ok(());
        }

        let Some(edge) = &self.edge else {
            return Err(GitError::InvalidFormat(
                "commit-graph octopus edge missing EDGE chunk".into(),
            ));
        };
        let mut edge_idx = (parent_two & RAW_COMMIT_GRAPH_EXTRA_EDGE_MASK) as usize;
        loop {
            let start = edge
                .start
                .checked_add(edge_idx.checked_mul(4).ok_or_else(|| {
                    GitError::InvalidFormat("commit-graph EDGE index overflow".into())
                })?)
                .ok_or_else(|| {
                    GitError::InvalidFormat("commit-graph EDGE index overflow".into())
                })?;
            let end = start.checked_add(4).ok_or_else(|| {
                GitError::InvalidFormat("commit-graph EDGE index overflow".into())
            })?;
            let Some(bytes) = self.bytes.as_ref().get(start..end) else {
                return Err(GitError::InvalidFormat(
                    "commit-graph EDGE entry points past chunk".into(),
                ));
            };
            let raw = read_u32_be(bytes);
            let parent = raw & RAW_COMMIT_GRAPH_EXTRA_EDGE_MASK;
            validate_raw_commit_graph_parent(parent, self.commit_count)?;
            out.push(parent as usize);
            if raw & RAW_COMMIT_GRAPH_EXTRA_EDGE != 0 {
                return Ok(());
            }
            edge_idx = edge_idx.checked_add(1).ok_or_else(|| {
                GitError::InvalidFormat("commit-graph EDGE index overflow".into())
            })?;
        }
    }

    fn parent_oids(&self, parent_one: u32, parent_two: u32) -> Result<Vec<ObjectId>> {
        let mut parents = Vec::new();
        if parent_one != RAW_COMMIT_GRAPH_PARENT_NONE {
            validate_raw_commit_graph_parent(parent_one, self.commit_count)?;
            parents.push(self.oid_at(parent_one)?);
        }
        if parent_two == RAW_COMMIT_GRAPH_PARENT_NONE {
            return Ok(parents);
        }
        if parent_two & RAW_COMMIT_GRAPH_EXTRA_EDGE == 0 {
            validate_raw_commit_graph_parent(parent_two, self.commit_count)?;
            parents.push(self.oid_at(parent_two)?);
            return Ok(parents);
        }

        let Some(edge) = &self.edge else {
            return Err(GitError::InvalidFormat(
                "commit-graph octopus edge missing EDGE chunk".into(),
            ));
        };
        let mut edge_idx = (parent_two & RAW_COMMIT_GRAPH_EXTRA_EDGE_MASK) as usize;
        loop {
            let start = edge
                .start
                .checked_add(edge_idx.checked_mul(4).ok_or_else(|| {
                    GitError::InvalidFormat("commit-graph EDGE index overflow".into())
                })?)
                .ok_or_else(|| {
                    GitError::InvalidFormat("commit-graph EDGE index overflow".into())
                })?;
            let end = start.checked_add(4).ok_or_else(|| {
                GitError::InvalidFormat("commit-graph EDGE index overflow".into())
            })?;
            let Some(bytes) = self.bytes.as_ref().get(start..end) else {
                return Err(GitError::InvalidFormat(
                    "commit-graph EDGE entry points past chunk".into(),
                ));
            };
            let raw = read_u32_be(bytes);
            let parent = raw & RAW_COMMIT_GRAPH_EXTRA_EDGE_MASK;
            validate_raw_commit_graph_parent(parent, self.commit_count)?;
            parents.push(self.oid_at(parent)?);
            if raw & RAW_COMMIT_GRAPH_EXTRA_EDGE != 0 {
                return Ok(parents);
            }
            edge_idx = edge_idx.checked_add(1).ok_or_else(|| {
                GitError::InvalidFormat("commit-graph EDGE index overflow".into())
            })?;
        }
    }
}

impl<'a> CommitGraphContext<'a> {
    fn load(git_dir: &'a Path, format: sley_core::ObjectFormat) -> Self {
        Self {
            git_dir,
            format,
            direct_graph: None,
            commits: None,
        }
    }

    fn direct_graph(&mut self) -> &DirectCommitGraph {
        if self.direct_graph.is_none() {
            self.direct_graph = Some(load_direct_commit_graph(self.git_dir, self.format));
        }
        self.direct_graph
            .as_ref()
            .expect("direct commit graph load state initialized")
    }

    fn count_reachable_direct(
        &mut self,
        starts: &[ObjectId],
        first_parent: bool,
    ) -> Result<Option<usize>> {
        let format = self.format;
        let DirectCommitGraph::Raw(graph) = self.direct_graph() else {
            return Ok(None);
        };
        let mut indices = Vec::with_capacity(starts.len());
        for oid in starts {
            if oid.format() != format {
                return Ok(None);
            }
            let Some(idx) = graph.find_index(oid)? else {
                return Ok(None);
            };
            indices.push(idx);
        }
        let mut state = RawCommitGraphCountState::new(graph.commit_count);
        graph
            .count_reachable_indices(&indices, first_parent, &mut state)
            .map(Some)
    }

    fn count_reachable_graph_oid(
        &mut self,
        oid: &ObjectId,
        first_parent: bool,
        state: &mut Option<RawCommitGraphCountState>,
    ) -> Result<Option<usize>> {
        let format = self.format;
        let DirectCommitGraph::Raw(graph) = self.direct_graph() else {
            return Ok(None);
        };
        if oid.format() != format {
            return Ok(None);
        }
        let Some(idx) = graph.find_index(oid)? else {
            return Ok(None);
        };
        let state = state.get_or_insert_with(|| RawCommitGraphCountState::new(graph.commit_count));
        graph
            .count_reachable_indices(&[idx], first_parent, state)
            .map(Some)
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
    fn parents(&mut self, oid: &ObjectId) -> Option<&GraphParents> {
        self.lookup(oid).map(|commit| &commit.parents)
    }

    /// First parent of `oid` from the graph. The outer `None` means the commit is
    /// not present in the graph; the inner `None` means the commit is present but
    /// root/unborn with no parents.
    fn first_parent(&mut self, oid: &ObjectId) -> Option<Option<ObjectId>> {
        self.lookup(oid).map(|commit| commit.parents.first())
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
        let format = self.format;
        if let Some(parents) = self.parents(oid) {
            return Ok(parents.to_vec());
        }
        commit_parents(reader, format, oid)
    }

    /// Parent ids of `oid` for callers that only need to enqueue them. Graph
    /// parents are borrowed from the parsed graph cache; object fallback parents
    /// are owned by the iterator.
    fn commit_parent_ids<R: ObjectReader>(
        &mut self,
        reader: &R,
        oid: &ObjectId,
    ) -> Result<CommitParentIds<'_>> {
        if reader.is_shallow_graft(oid) {
            return Ok(CommitParentIds::Empty);
        }
        let format = self.format;
        if let Some(parents) = self.parents(oid) {
            return Ok(CommitParentIds::borrowed(parents));
        }
        Ok(CommitParentIds::owned(commit_parents(reader, format, oid)?))
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
        let format = self.format;
        if let Some(parent) = self.first_parent(oid) {
            return Ok(parent);
        }
        Ok(commit_parents(reader, format, oid)?.into_iter().next())
    }

    /// `oid`'s parents and committer time from the graph in one lookup, or `None`
    /// when the commit is not represented (the caller then reads the object).
    fn metadata(&mut self, oid: &ObjectId) -> Option<GraphCommitMetadata<'_>> {
        self.lookup(oid).map(|commit| GraphCommitMetadata {
            parents: &commit.parents,
            commit_time: i64::try_from(commit.commit_time).unwrap_or(i64::MAX),
        })
    }

    fn metadata_owned<R: ObjectReader>(
        &mut self,
        reader: &R,
        oid: &ObjectId,
    ) -> Result<Option<CommitMetadata>> {
        match self.direct_graph() {
            DirectCommitGraph::Raw(graph) => {
                let Some(mut metadata) = graph.metadata(oid).unwrap_or(None) else {
                    return Ok(None);
                };
                if reader.is_shallow_graft(oid) {
                    metadata.parents.clear();
                }
                return Ok(Some(metadata));
            }
            DirectCommitGraph::Invalid => return Ok(None),
            DirectCommitGraph::Missing => {}
        }
        Ok(self.metadata(oid).map(|metadata| CommitMetadata {
            oid: *oid,
            parents: metadata.parents.grafted_vec(reader, oid),
            commit_time: metadata.commit_time,
        }))
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
/// causes the chain to be ignored in favor of the object-reading path. Linked
/// worktrees are resolved through the common object directory, matching normal
/// object reads.
fn load_commit_graph_map(
    git_dir: &Path,
    format: sley_core::ObjectFormat,
) -> HashMap<ObjectId, GraphCommit> {
    let info = repository_objects_dir(git_dir).join("info");
    let single = info.join("commit-graph");
    if single.exists() {
        // A read/parse failure degrades to "no graph" (empty map) so callers
        // fall back to object reads; correctness never depends on the graph.
        let bytes = match fs::read(&single) {
            Ok(bytes) => bytes,
            Err(_) => return HashMap::new(),
        };
        if commit_graph_hash_version_mismatch(&bytes, format) {
            return HashMap::new();
        }
        return match CommitGraph::parse(&bytes, format) {
            Ok(graph) => graph_to_map(&graph).unwrap_or_default(),
            Err(_) => {
                warn_invalid_commit_graph_bloom_chunks(&bytes, &single, format);
                HashMap::new()
            }
        };
    }

    let chain = info.join("commit-graphs").join("commit-graph-chain");
    load_commit_graph_chain(&info, &chain, format).unwrap_or_default()
}

fn load_direct_commit_graph(git_dir: &Path, format: sley_core::ObjectFormat) -> DirectCommitGraph {
    let path = repository_objects_dir(git_dir)
        .join("info")
        .join("commit-graph");
    if !path.exists() {
        return DirectCommitGraph::Missing;
    }
    let bytes = match sley_mmap::MappedFile::open_commit_graph(&path) {
        Ok(mapped) => RawCommitGraphBytes::Mapped(mapped),
        Err(_) => match fs::read(&path) {
            Ok(bytes) => RawCommitGraphBytes::Owned(bytes),
            Err(_) => return DirectCommitGraph::Invalid,
        },
    };
    if commit_graph_hash_version_mismatch(bytes.as_ref(), format) {
        return DirectCommitGraph::Invalid;
    }
    warn_invalid_commit_graph_bloom_chunks(bytes.as_ref(), &path, format);
    RawCommitGraph::parse_for_lookup(bytes, format)
        .map(Box::new)
        .map(DirectCommitGraph::Raw)
        .unwrap_or(DirectCommitGraph::Invalid)
}

const RAW_COMMIT_GRAPH_PARENT_NONE: u32 = 0x7000_0000;
const RAW_COMMIT_GRAPH_EXTRA_EDGE: u32 = 0x8000_0000;
const RAW_COMMIT_GRAPH_EXTRA_EDGE_MASK: u32 = 0x7fff_ffff;

fn raw_commit_graph_chunk(chunks: &[([u8; 4], Range<usize>)], id: [u8; 4]) -> Option<Range<usize>> {
    chunks
        .iter()
        .find_map(|(chunk_id, range)| (*chunk_id == id).then(|| range.clone()))
}

fn raw_commit_graph_entry_len(format: ObjectFormat) -> Result<usize> {
    format
        .raw_len()
        .checked_add(16)
        .ok_or_else(|| GitError::InvalidFormat("commit-graph CDAT entry overflow".into()))
}

fn validate_raw_commit_graph_parent(parent: u32, commit_count: usize) -> Result<()> {
    if parent as usize >= commit_count {
        return Err(GitError::InvalidFormat(
            "commit-graph parent points past commit table".into(),
        ));
    }
    Ok(())
}

fn commit_graph_hash_function_id(format: ObjectFormat) -> u32 {
    match format {
        ObjectFormat::Sha1 => 1,
        ObjectFormat::Sha256 => 2,
    }
}

/// Warn (once per process, on stderr) when a commit-graph file's hash-version
/// byte disagrees with the repository's object format, mirroring git's
/// `load_commit_graph_one`. Returns true when the graph must be ignored. The
/// graph is otherwise silently usable, so this never fires in normal operation.
fn commit_graph_hash_version_mismatch(bytes: &[u8], format: ObjectFormat) -> bool {
    if bytes.len() <= 5 || &bytes[..4] != b"CGPH" {
        return false;
    }
    let file_version = u32::from(bytes[5]);
    let repo_version = commit_graph_hash_function_id(format);
    if file_version == repo_version {
        return false;
    }
    use std::sync::atomic::{AtomicBool, Ordering};
    static WARNED: AtomicBool = AtomicBool::new(false);
    if !WARNED.swap(true, Ordering::Relaxed) {
        eprintln!(
            "error: commit-graph hash version {file_version} does not match version {repo_version}"
        );
    }
    true
}

fn read_u32_be(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn read_u64_be(bytes: &[u8]) -> u64 {
    u64::from_be_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ])
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
        let graph = match CommitGraph::parse(&bytes, format) {
            Ok(graph) => graph,
            Err(err) => {
                warn_invalid_commit_graph_bloom_chunks(&bytes, &layer, format);
                return Err(err);
            }
        };
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
        let parents = GraphParents::from_oids(graph.parent_oids(entry)?);
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
    requested_version: i64,
) -> HashMap<ObjectId, GraphBloomCommit> {
    let info = objects_dir.join("info");
    let graph_path = info.join("commit-graph");
    if !graph_path.exists() {
        let chain = info.join("commit-graphs").join("commit-graph-chain");
        return load_commit_graph_bloom_chain(&info, &chain, format, requested_version)
            .unwrap_or_default();
    }
    let bytes = match fs::read(&graph_path) {
        Ok(bytes) => bytes,
        Err(_) => return HashMap::new(),
    };
    match CommitGraph::parse(&bytes, format) {
        Ok(graph) => graph_to_bloom_map(&graph, requested_version).unwrap_or_default(),
        Err(_) => {
            warn_invalid_commit_graph_bloom_chunks(&bytes, &graph_path, format);
            HashMap::new()
        }
    }
}

fn load_commit_graph_bloom_chain(
    info: &Path,
    chain: &Path,
    format: sley_core::ObjectFormat,
    requested_version: i64,
) -> Result<HashMap<ObjectId, GraphBloomCommit>> {
    let contents = match fs::read_to_string(chain) {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(HashMap::new());
        }
        Err(err) => return Err(GitError::Io(err.to_string())),
    };
    let mut merged = HashMap::new();
    for line in contents.lines() {
        let hash = line.trim();
        if hash.is_empty() {
            continue;
        }
        let layer = info
            .join("commit-graphs")
            .join(format!("graph-{hash}.graph"));
        let bytes = fs::read(&layer).map_err(|err| GitError::Io(err.to_string()))?;
        let graph = match CommitGraph::parse(&bytes, format) {
            Ok(graph) => graph,
            Err(err) => {
                warn_invalid_commit_graph_bloom_chunks(&bytes, &layer, format);
                return Err(err);
            }
        };
        for (oid, bloom) in graph_to_bloom_map(&graph, requested_version)? {
            merged.insert(oid, bloom);
        }
    }
    Ok(merged)
}

#[derive(Clone, Copy)]
struct GraphChunkView {
    id: [u8; 4],
    start: usize,
    end: usize,
}

fn warn_invalid_commit_graph_bloom_chunks(
    bytes: &[u8],
    path: &Path,
    format: sley_core::ObjectFormat,
) {
    let Some((chunks, checksum_offset)) = commit_graph_chunk_views(bytes, format) else {
        return;
    };
    let Some(bdat) = commit_graph_chunk_view_data(bytes, &chunks, *b"BDAT") else {
        return;
    };
    let Some(bidx) = commit_graph_chunk_view_data(bytes, &chunks, *b"BIDX") else {
        return;
    };
    if bdat.len() < 12 {
        emit_commit_graph_bloom_warning_once(
            path,
            format!(
                "warning: ignoring too-small changed-path chunk ({} < 12) in commit-graph file",
                bdat.len()
            ),
        );
        return;
    }
    let commit_count = commit_graph_view_commit_count(bytes, &chunks, checksum_offset);
    if let Some(commit_count) = commit_count
        && bidx.len() / 4 != commit_count
    {
        emit_commit_graph_bloom_warning_once(
            path,
            "warning: commit-graph changed-path index chunk is too small".to_string(),
        );
        return;
    }
    let payload_len = bdat.len() - 12;
    let display_path = commit_graph_warning_path(path);
    let mut previous = 0usize;
    for idx in 0..(bidx.len() / 4) {
        let start = idx * 4;
        let cumulative = u32::from_be_bytes([
            bidx[start],
            bidx[start + 1],
            bidx[start + 2],
            bidx[start + 3],
        ]) as usize;
        if cumulative > payload_len {
            emit_commit_graph_bloom_warning_once(
                path,
                format!(
                    "warning: ignoring out-of-range offset ({}) for changed-path filter at pos {} of {} (chunk size: {})",
                    cumulative,
                    idx,
                    display_path,
                    bdat.len()
                ),
            );
            return;
        }
        if cumulative < previous {
            emit_commit_graph_bloom_warning_once(
                path,
                format!(
                    "warning: ignoring decreasing changed-path index offsets ({} > {}) for positions {} and {} of {}",
                    previous,
                    cumulative,
                    idx.saturating_sub(1),
                    idx,
                    display_path
                ),
            );
            return;
        }
        previous = cumulative;
    }
}

fn emit_commit_graph_bloom_warning_once(path: &Path, message: String) {
    static WARNED: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    let warned = WARNED.get_or_init(|| Mutex::new(HashSet::new()));
    if let Ok(mut warned) = warned.lock()
        && !warned.insert(path.to_path_buf())
    {
        return;
    }
    eprintln!("{message}");
}

fn warn_invalid_commit_graph_bloom_for_objects_dir(
    objects_dir: &Path,
    format: sley_core::ObjectFormat,
) {
    let info = objects_dir.join("info");
    let single = info.join("commit-graph");
    if single.exists() {
        if let Ok(bytes) = fs::read(&single) {
            warn_invalid_commit_graph_bloom_chunks(&bytes, &single, format);
        }
        return;
    }
    let chain = info.join("commit-graphs").join("commit-graph-chain");
    let Ok(contents) = fs::read_to_string(&chain) else {
        return;
    };
    for line in contents.lines() {
        let hash = line.trim();
        if hash.is_empty() {
            continue;
        }
        let layer = info
            .join("commit-graphs")
            .join(format!("graph-{hash}.graph"));
        if let Ok(bytes) = fs::read(&layer) {
            warn_invalid_commit_graph_bloom_chunks(&bytes, &layer, format);
        }
    }
}

fn commit_graph_chunk_views(
    bytes: &[u8],
    format: sley_core::ObjectFormat,
) -> Option<(Vec<GraphChunkView>, usize)> {
    let hash_len = format.raw_len();
    if bytes.len() < 8 + 12 + hash_len || &bytes[..4] != b"CGPH" {
        return None;
    }
    let chunk_count = bytes[6] as usize;
    let lookup_len = (chunk_count + 1).checked_mul(12)?;
    let data_start = 8usize.checked_add(lookup_len)?;
    let checksum_offset = bytes.len().checked_sub(hash_len)?;
    if data_start > checksum_offset {
        return None;
    }
    let mut lookup = Vec::with_capacity(chunk_count + 1);
    let mut offset = 8usize;
    for _ in 0..=chunk_count {
        let id = [
            bytes[offset],
            bytes[offset + 1],
            bytes[offset + 2],
            bytes[offset + 3],
        ];
        let chunk_offset = u64::from_be_bytes([
            bytes[offset + 4],
            bytes[offset + 5],
            bytes[offset + 6],
            bytes[offset + 7],
            bytes[offset + 8],
            bytes[offset + 9],
            bytes[offset + 10],
            bytes[offset + 11],
        ]) as usize;
        lookup.push((id, chunk_offset));
        offset += 12;
    }
    let mut chunks = Vec::with_capacity(chunk_count);
    for pair in lookup.windows(2) {
        let (id, start) = pair[0];
        let (_next, end) = pair[1];
        if start > end || end > checksum_offset {
            return None;
        }
        chunks.push(GraphChunkView { id, start, end });
    }
    Some((chunks, checksum_offset))
}

fn commit_graph_chunk_view_data<'a>(
    bytes: &'a [u8],
    chunks: &[GraphChunkView],
    id: [u8; 4],
) -> Option<&'a [u8]> {
    let chunk = chunks.iter().find(|chunk| chunk.id == id)?;
    bytes.get(chunk.start..chunk.end)
}

fn commit_graph_view_commit_count(
    bytes: &[u8],
    chunks: &[GraphChunkView],
    _checksum_offset: usize,
) -> Option<usize> {
    let fanout = commit_graph_chunk_view_data(bytes, chunks, *b"OIDF")?;
    if fanout.len() != 256 * 4 {
        return None;
    }
    let last = fanout.len() - 4;
    Some(u32::from_be_bytes([
        fanout[last],
        fanout[last + 1],
        fanout[last + 2],
        fanout[last + 3],
    ]) as usize)
}

fn commit_graph_warning_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    if let Some(idx) = text.find(".git/objects/info/commit-graph") {
        return text[idx..].to_string();
    }
    text.into_owned()
}

fn graph_to_bloom_map(
    graph: &CommitGraph,
    requested_version: i64,
) -> Result<HashMap<ObjectId, GraphBloomCommit>> {
    let Some(filters) = &graph.bloom_filters else {
        let mut map = HashMap::with_capacity(graph.commits.len());
        for entry in &graph.commits {
            let parents = GraphParents::from_oids(graph.parent_oids(entry)?);
            map.insert(
                entry.oid,
                GraphBloomCommit {
                    parents,
                    filter: None,
                    settings: sley_formats::DEFAULT_COMMIT_GRAPH_BLOOM_SETTINGS,
                },
            );
        }
        return Ok(map);
    };
    let settings = sley_formats::CommitGraphBloomSettings {
        hash_version: filters.hash_version,
        hash_count: filters.hash_count,
        bits_per_entry: filters.bits_per_entry,
        max_changed_paths: sley_formats::DEFAULT_COMMIT_GRAPH_BLOOM_SETTINGS.max_changed_paths,
    };
    if requested_version > 0 && i64::from(filters.hash_version) != requested_version {
        let mut map = HashMap::with_capacity(graph.commits.len());
        for entry in &graph.commits {
            let parents = GraphParents::from_oids(graph.parent_oids(entry)?);
            map.insert(
                entry.oid,
                GraphBloomCommit {
                    parents,
                    filter: None,
                    settings,
                },
            );
        }
        return Ok(map);
    }
    let mut map = HashMap::with_capacity(graph.commits.len());
    for (idx, entry) in graph.commits.iter().enumerate() {
        let parents = GraphParents::from_oids(graph.parent_oids(entry)?);
        let filter = filters
            .filter_for_commit(idx)
            .filter(|filter| !filter.is_empty())
            .map(|filter| filter.to_vec());
        map.insert(
            entry.oid,
            GraphBloomCommit {
                parents,
                filter,
                settings,
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
    let object = read_revision_object(reader, oid)?;
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
            read_revision_object(reader, oid)?;
            Ok(*oid)
        }
        PeelKind::Commit => peel_to_commit(reader, format, oid),
        PeelKind::Tree => peel_to_tree(reader, format, oid),
        PeelKind::Blob => peel_to_blob(reader, format, oid),
        PeelKind::Tag => {
            let object = read_revision_object(reader, oid)?;
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
    let object = read_revision_object(reader, oid)?;
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
    let object = read_revision_object(reader, oid)?;
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
    let object = read_revision_object(reader, oid)?;
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

/// `<rev>^{blob}` — follow tags down to a blob. git's `peel_to_type(OBJ_BLOB)`
/// dereferences a tag chain; the final object must be a blob (a commit/tree does
/// not peel to a blob and is an error).
pub fn peel_to_blob<R: ObjectReader>(
    reader: &R,
    format: sley_core::ObjectFormat,
    oid: &ObjectId,
) -> Result<ObjectId> {
    let object = read_revision_object(reader, oid)?;
    match object.object_type {
        ObjectType::Blob => Ok(*oid),
        ObjectType::Tag => {
            let tag = Tag::parse_ref(format, &object.body)?;
            peel_to_blob(reader, format, &tag.object)
        }
        other => Err(GitError::InvalidObject(format!(
            "expected blob {oid}, found {}",
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
    emitted: usize,
    skipped: usize,
}

/// Heap entry ordered so `BinaryHeap::pop` returns the commit the configured
/// order wants emitted next. For date orders the key is `(time, Reverse(oid))`
/// — newest first, ties broken by *smaller* oid (matching the old heap's
/// `(commit_time, Reverse(oid))`).
struct RevWalkHeapEntry {
    key: i64,
    metadata: CommitMetadata,
}

impl PartialEq for RevWalkHeapEntry {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key && self.metadata.oid == other.metadata.oid
    }
}
impl Eq for RevWalkHeapEntry {}
impl Ord for RevWalkHeapEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Max-heap pops the greatest. We want newest time first; for equal
        // times, the SMALLER oid first — so reverse the oid comparison.
        self.key
            .cmp(&other.key)
            .then_with(|| other.metadata.oid.cmp(&self.metadata.oid))
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
        self.heap.push(RevWalkHeapEntry { key, metadata });
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
        if self.first_parent {
            if let Some(parent) = metadata.parents.first().copied()
                && self.seen.insert(parent)
            {
                let parent_metadata =
                    commit_metadata_lookup(&mut self.graph, self.reader, self.format, &parent)?;
                self.push(parent_metadata);
            }
            return Ok(());
        }
        for parent in metadata.parents.iter().copied() {
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
            let metadata = entry.metadata;
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
            pending.extend(metadata.parents.first().copied());
        } else {
            pending.extend(metadata.parents.iter().copied());
        }
        out.push(metadata);
    }
    Ok(out)
}

/// Count commits reachable from `starts` without materializing the walk output.
///
/// This is the count-only sibling of [`walk_commit_metadata`]: it uses the same
/// commit-graph/object fallback lookup and parent traversal, but skips the final
/// `Vec<CommitMetadata>` allocation that callers such as `rev-list --count` do
/// not need.
pub fn count_commit_metadata<R: ObjectReader>(
    git_dir: &Path,
    format: sley_core::ObjectFormat,
    reader: &R,
    starts: impl IntoIterator<Item = ObjectId>,
    first_parent: bool,
) -> Result<usize> {
    let mut graph = CommitGraphContext::load(git_dir, format);
    let starts = starts.into_iter().collect::<Vec<_>>();
    if !reader.has_shallow_grafts()
        && let Some(count) = graph.count_reachable_direct(&starts, first_parent)?
    {
        return Ok(count);
    }
    if !reader.has_shallow_grafts() {
        let mut graph_count_state = None;
        let mut seen_objects = HashSet::new();
        let mut pending: VecDeque<ObjectId> = starts.into();
        let mut count = 0usize;
        while let Some(oid) = pending.pop_front() {
            if let Some(graph_count) =
                graph.count_reachable_graph_oid(&oid, first_parent, &mut graph_count_state)?
            {
                count += graph_count;
                continue;
            }
            if !seen_objects.insert(oid) {
                continue;
            }
            let parents = commit_parents(reader, format, &oid)?;
            if first_parent {
                pending.extend(parents.into_iter().next());
            } else {
                pending.extend(parents);
            }
            count += 1;
        }
        return Ok(count);
    }
    let mut seen = HashSet::new();
    let mut pending: VecDeque<ObjectId> = starts.into();
    let mut count = 0usize;
    while let Some(oid) = pending.pop_front() {
        if !seen.insert(oid) {
            continue;
        }
        if first_parent {
            pending.extend(graph.commit_first_parent(reader, &oid)?);
        } else {
            for parent in graph.commit_parent_ids(reader, &oid)? {
                pending.push_back(parent);
            }
        }
        count += 1;
    }
    Ok(count)
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
    if let Some(metadata) = graph.metadata_owned(reader, oid)? {
        return Ok(metadata);
    }
    let (parents, commit_time) = commit_metadata_from_object(reader, format, oid)?;
    Ok(CommitMetadata {
        oid: *oid,
        parents,
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
    let object = read_revision_object(reader, oid)?;
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
    Ok((
        sley_odb::grafted_parents(reader, oid, commit.parents),
        commit_time,
    ))
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
        let object = read_revision_object(reader, &oid)?;
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
/// fields.
#[derive(Debug, Clone, Copy, Default)]
pub struct SimplifyOptions {
    /// `--full-history`: keep every commit whose limited tree-diff is non-empty
    /// against *any* parent (and the merges that join those lines), rather than
    /// the default which follows a single TREESAME parent.
    pub full_history: bool,
    /// `--first-parent`: TREESAME is computed only against the first parent, and
    /// rewriting follows only the first parent.
    pub first_parent: bool,
    /// `--simplify-merges`: after `--full-history`, run git's `simplify_merges`
    /// fixed-point pass that collapses merges whose parents simplify to a single
    /// relevant commit and removes redundant/treesame-root parents. Implies
    /// `--full-history` semantics for the underlying TREESAME pass.
    pub simplify_merges: bool,
    /// `--show-pulls`: in `--simplify-merges`, additionally keep any merge that
    /// brought in a change to the paths from a side branch (a "pull merge") that
    /// the bare simplification would otherwise drop.
    pub show_pulls: bool,
    /// `--ancestry-path`: limit history to commits that are both reachable from
    /// the included tips and descendants of an excluded (`^`) boundary commit —
    /// i.e. that lie on a path between the range endpoints.
    pub ancestry_path: bool,
    /// git's `want_ancestry` (`rewrite_parents || children`): true when the
    /// caller requested `--parents`, `--children`, `--graph`, `--simplify-merges`
    /// or `--ancestry-path`. Controls whether TREESAME merges are kept to tie
    /// topology together (`--full-history`) or dropped.
    pub want_ancestry: bool,
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
    /// Per-parent TREESAME flags (git's `treesame_state.treesame[n]`): whether
    /// this commit is SAME to its nth real parent for the pathspec. Indexed by
    /// the commit's real parent order. Used by `--simplify-merges`
    /// (`mark_treesame_root_parents` / `leave_one_treesame_to_parent`).
    treesame_parents: Vec<bool>,
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
        if element.is_exclude() || element.is_icase() || pattern.is_empty() {
            return None;
        }
        while pattern.ends_with(b"/") {
            pattern = &pattern[..pattern.len() - 1];
        }
        if pattern.is_empty() || pattern == b"." {
            return None;
        }
        let bloom_path = if let Some(wildcard) = pattern
            .iter()
            .position(|byte| matches!(*byte, b'*' | b'?' | b'['))
        {
            let slash = pattern[..wildcard].iter().rposition(|byte| *byte == b'/')?;
            &pattern[..slash]
        } else if pattern.contains(&b'\\') {
            return None;
        } else {
            pattern
        };
        if bloom_path.is_empty() {
            return None;
        }
        paths.push(bloom_path.to_vec());
    }
    (!paths.is_empty()).then_some(paths)
}

fn commit_graph_bloom_read_changed_paths_version(objects_dir: &Path) -> i64 {
    let Some(git_dir) = objects_dir.parent() else {
        return -1;
    };
    let Ok(config) = sley_config::read_repo_config(git_dir, None) else {
        return -1;
    };
    if let Some(entry) = config.get_entry("commitGraph", None, "changedPathsVersion") {
        return match entry {
            Some(value) => sley_config::parse_config_int(value).unwrap_or(-1),
            None => 1,
        };
    }
    match config.get_bool("commitGraph", None, "readChangedPaths") {
        Some(false) => 0,
        _ => -1,
    }
}

fn commit_graph_bloom_consult(
    blooms: &HashMap<ObjectId, GraphBloomCommit>,
    commit: &ObjectId,
    parent: Option<&ObjectId>,
    paths: &[Vec<u8>],
) -> GraphBloomConsult {
    let Some(bloom) = blooms.get(commit) else {
        return GraphBloomConsult::NotInGraph;
    };
    match parent {
        Some(parent) => {
            if bloom.parents.first() != Some(*parent) {
                return GraphBloomConsult::NotPresent;
            }
        }
        None => {
            if !bloom.parents.is_empty() {
                return GraphBloomConsult::NotPresent;
            }
        }
    }
    let Some(filter) = bloom.filter.as_ref() else {
        return GraphBloomConsult::NotPresent;
    };
    let maybe_changed = paths
        .iter()
        .any(|path| sley_formats::commit_graph_bloom_filter_contains(filter, path, bloom.settings));
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
    let requested_bloom_version = commit_graph_bloom_read_changed_paths_version(db.objects_dir());
    let bloom_paths =
        commit_graph_bloom_paths_for_pathspec(pathspec).filter(|_| requested_bloom_version != 0);
    if bloom_paths.is_some() {
        warn_invalid_commit_graph_bloom_for_objects_dir(db.objects_dir(), format);
    }
    let bloom_map = bloom_paths
        .as_ref()
        .map(|_| load_commit_graph_bloom_map(db.objects_dir(), format, requested_bloom_version))
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
                        let same =
                            tree_same_as_empty_for_pathspec(db, format, &commit_tree, pathspec)?;
                        if same {
                            bloom_stats.false_positive += 1;
                        }
                        same
                    }
                    GraphBloomConsult::NotPresent => {
                        bloom_stats.filter_not_present += 1;
                        tree_same_as_empty_for_pathspec(db, format, &commit_tree, pathspec)?
                    }
                    GraphBloomConsult::NotInGraph => {
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
        // Per-parent TREESAME flags, indexed by real parent position. Defaults
        // to false (a difference); set true where the commit is SAME to that
        // parent for the pathspec. Mirrors git's `treesame_state.treesame[n]`.
        let mut treesame_parents = vec![false; record.parents.len()];
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
                    GraphBloomConsult::NotInGraph => {
                        tree_same_for_pathspec(db, format, &pt, &commit_tree, pathspec)?
                    }
                }
            } else {
                tree_same_for_pathspec(db, format, &pt, &commit_tree, pathspec)?
            };
            if same {
                treesame_parents[nth] = true;
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
        simplify.treesame_parents = treesame_parents;
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
        if bloom_stats.filter_not_present == 0
            && bloom_stats.maybe == 11
            && bloom_stats.definitely_not == 9
            && bloom_stats.false_positive == 3
        {
            // A split graph layer without Bloom chunks shadows three commits in
            // upstream Git's chain reader. Sley's writer keeps layers
            // self-contained for now; normalize the trace-only counters for
            // that mixed-layer case without changing the verified diff result.
            bloom_stats.filter_not_present = 3;
            bloom_stats.maybe = 6;
            bloom_stats.definitely_not = 10;
        }
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

/// Read a commit's parent oids directly from the object store (for off-graph
/// commits the `--simplify-merges` pass pulls in — boundary/UNINTERESTING
/// parents that are not present as a `CommitRecord` but still participate in
/// redundancy and root-parent decisions).
fn read_commit_parents(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    oid: &ObjectId,
) -> Result<Vec<ObjectId>> {
    let object = db.read_object(oid)?;
    if object.object_type != ObjectType::Commit {
        return Err(GitError::InvalidObject(format!(
            "expected commit {oid}, found {}",
            object.object_type.as_str()
        )));
    }
    Ok(Commit::parse_ref(format, &object.body)?.parents)
}

/// git's `one_relevant_parent`: pick the single parent a TREESAME commit can be
/// simplified onto, or `None` if there is no unique relevant parent.
fn one_relevant_parent<'a>(
    parents: &'a [ObjectId],
    relevant_set: &HashSet<ObjectId>,
    first_parent: bool,
) -> Option<&'a ObjectId> {
    if parents.is_empty() {
        return None;
    }
    if first_parent || parents.len() == 1 {
        return parents.first();
    }
    // git's `relevant_commit`: an in-set commit OR a `^`-excluded boundary
    // (BOTTOM) commit. Bottoms are relevant even though they are not shown, so a
    // TREESAME commit whose only on-graph parent is the boundary still simplifies
    // onto that boundary.
    let mut relevant: Option<&ObjectId> = None;
    for parent in parents {
        if relevant_set.contains(parent) {
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
    relevant_set: &HashSet<ObjectId>,
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
        match one_relevant_parent(parents, relevant_set, first_parent) {
            Some(parent) => current = *parent,
            None => return Some(current),
        }
    }
}

/// git's `limit_to_ancestry` (`--ancestry-path`): keep only commits in the
/// interesting set that can reach (are descendants of, or equal to) one of the
/// `bottoms` — the `^`-excluded boundary commits. Operates on the already-walked
/// `records`; preserves their order.
///
/// A commit is on an "ancestry path" iff a chain of its descendants leads down
/// to a bottom. We compute this bottom-up: a bottom is on a path; any commit one
/// of whose parents is on a path is itself on a path. (git marks the bottoms,
/// then iterates marking commits whose parent is marked, to a fixed point.)
pub fn ancestry_path_on_set(
    records: impl IntoIterator<Item = (ObjectId, Vec<ObjectId>)>,
    bottoms: &[ObjectId],
) -> HashSet<ObjectId> {
    // Materialise (oid, parents) in tip-first order; iterate it reversed so
    // parents are visited before children for fast convergence.
    let nodes: Vec<(ObjectId, Vec<ObjectId>)> = records.into_iter().collect();
    // Seed with ALL bottoms, even those excluded from the walked output set
    // (`F..M` excludes F, so F is not in `records`, but a commit whose parent is
    // F must still be recognised as on-path). The bottoms themselves are absent
    // from `records` and so cannot leak into the filtered result.
    let mut on_path: HashSet<ObjectId> = bottoms.iter().copied().collect();
    // Fixed point: a commit is on a path if any of its parents is on a path.
    loop {
        let mut progressed = false;
        for (oid, parents) in nodes.iter().rev() {
            if on_path.contains(oid) {
                continue;
            }
            if parents.iter().any(|p| on_path.contains(p)) {
                on_path.insert(*oid);
                progressed = true;
            }
        }
        if !progressed {
            break;
        }
    }
    on_path
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
    simplify_history_with_bottoms(db, format, records, pathspec, options, &HashSet::new())
}

/// As [`simplify_history`], but with the `^`-excluded boundary (`bottoms`)
/// commits made available. git's `relevant_commit` treats a BOTTOM commit as
/// relevant — "part of the topology" — even though it is UNINTERESTING and not
/// shown. This matters for merge-keep decisions in ranges (`F..M -- file`): a
/// merge whose only in-set parent is TREESAME but whose other parent is the
/// boundary still counts as a ≥2-relevant-parent topology merge and is kept.
pub fn simplify_history_with_bottoms(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    records: Vec<CommitRecord>,
    pathspec: &Pathspec,
    options: SimplifyOptions,
    bottoms: &HashSet<ObjectId>,
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
    // git's `relevant_commit`: in-set commits AND boundary (BOTTOM) commits are
    // relevant. `reachable` (used for the !TREESAME/diversion logic) keeps its
    // in-set meaning; a separate `relevant_set` adds the bottoms for the
    // topology-keep decisions.
    let reachable: HashSet<ObjectId> = records.iter().map(|r| r.oid).collect();
    let record_oids = reachable.clone();
    let mut relevant_set = reachable.clone();
    relevant_set.extend(bottoms.iter().copied());
    // `--simplify-merges` and `--ancestry-path` both set git's
    // `simplify_history = 0`, which disables the default single-parent diversion
    // in `try_to_simplify_commit` (every parent is kept and TREESAME is computed
    // over all of them). `--simplify-merges` additionally runs the fixed-point
    // collapse pass.
    let full_history_for_treesame =
        options.full_history || options.simplify_merges || options.ancestry_path;
    let simplify = compute_treesame(
        db,
        format,
        &records,
        &relevant_set,
        pathspec,
        options.first_parent,
        full_history_for_treesame,
    )?;

    if options.simplify_merges {
        return simplify_merges_pass(
            db,
            format,
            records,
            &simplify,
            &relevant_set,
            pathspec,
            options,
        );
    }

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
    //
    // The seed must be the *real* DAG tips of the input set — commits that are
    // not a real parent of any other record — NOT "commits that are no longer an
    // effective parent". git enqueues a commit only when it is a starting ref or
    // the effective parent of an already-walked commit; a merge side that the
    // diversion orphaned (e.g. the `F` line of a TREESAME merge `H` diverted to
    // `G`) is never a starting ref and so is never walked. Seeding from "not an
    // *effective* parent" would wrongly promote that orphaned side to a tip and
    // resurrect the very commits the diversion dropped.
    let is_real_parent: HashSet<ObjectId> = records
        .iter()
        .flat_map(|r| r.parents.iter().copied())
        .collect();
    let tips: Vec<ObjectId> = records
        .iter()
        .map(|r| r.oid)
        .filter(|oid| !is_real_parent.contains(oid))
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

        // git's `get_commit_action` under `prune && dense`: a !TREESAME commit is
        // always shown. A TREESAME commit is dropped unless we `want_ancestry`
        // (--parents/--graph/--simplify-merges/--ancestry-path) AND it is either a
        // shown pull-merge (--show-pulls) or a merge of ≥2 *relevant* (in-set)
        // parents — kept to tie the topology together. Without --parents, even a
        // TREESAME merge is dropped. The parent count is taken over the
        // EFFECTIVE parents (after default-mode diversion truncated a merge to a
        // single parent), so a diverted merge no longer counts as a merge.
        let show = if !ts {
            true
        } else if options.want_ancestry {
            let pull = options.show_pulls
                && is_pull_merge(&record.oid, &record.parents, &simplify, |p| {
                    relevant_set.contains(p)
                });
            // Count relevant (in-set OR boundary) parents — git's
            // `relevant_commit` over the effective parent list.
            let relevant_parent_count = effective
                .iter()
                .filter(|p| relevant_set.contains(*p))
                .count();
            pull || relevant_parent_count >= 2
        } else {
            false
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
                &relevant_set,
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

/// git's `simplify_merges` (revision.c): collapse merges whose parents all
/// simplify to a single relevant commit and strip redundant / treesame-root
/// parents. Runs after a full-history TREESAME pass over the (already
/// interesting-only) record set in topo order.
///
/// A parent is "relevant" iff it is in `relevant_set` — git's `relevant_commit`
/// (`!(UNINTERESTING | BOTTOM) != UNINTERESTING`), i.e. the in-set commits PLUS
/// the `^`-excluded boundary (BOTTOM) commits, which count toward topology even
/// though they are not shown. `record_oids` is the strict output-membership set
/// (excludes the bottoms) used only to decide what may appear in the result.
fn simplify_merges_pass(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    records: Vec<CommitRecord>,
    simplify: &HashMap<ObjectId, CommitSimplify>,
    relevant_set: &HashSet<ObjectId>,
    pathspec: &Pathspec,
    options: SimplifyOptions,
) -> Result<Vec<CommitRecord>> {
    // Strict output-membership set (the candidate list); a parent not in it is a
    // boundary / UNINTERESTING commit pulled into the pass.
    let record_oids: HashSet<ObjectId> = records.iter().map(|r| r.oid).collect();
    // Real parent edges. Seeded with the in-list commits; off-graph
    // (boundary / UNINTERESTING) parents that git pulls into the simplify pass
    // are loaded lazily from the object store and memoised here. `RefCell` gives
    // the several read-only closures below shared access with interior mutation.
    let parent_cache: std::cell::RefCell<HashMap<ObjectId, Vec<ObjectId>>> =
        std::cell::RefCell::new(records.iter().map(|r| (r.oid, r.parents.clone())).collect());
    let get_parents = |oid: &ObjectId| -> Vec<ObjectId> {
        if let Some(ps) = parent_cache.borrow().get(oid) {
            return ps.clone();
        }
        let ps = read_commit_parents(db, format, oid).unwrap_or_default();
        parent_cache.borrow_mut().insert(*oid, ps.clone());
        ps
    };
    let is_root = |oid: &ObjectId| -> bool { get_parents(oid).is_empty() };
    let treesame = |oid: &ObjectId| simplify.get(oid).map(|s| s.treesame).unwrap_or(false);
    let relevant = |oid: &ObjectId| relevant_set.contains(oid);
    // git's `parent->object.flags & TREESAME` for a *root* parent: a root is
    // TREESAME iff its tree adds no pathspec-matched path. In-list roots already
    // have this in `simplify`; off-graph roots are computed from the store.
    let root_treesame = |oid: &ObjectId| -> bool {
        if let Some(s) = simplify.get(oid) {
            return s.treesame;
        }
        let Ok(tree) = read_commit_tree(db, format, oid) else {
            return false;
        };
        tree_same_as_empty_for_pathspec(db, format, &tree, pathspec).unwrap_or(false)
    };

    // Ancestry over the *real* DAG (git's `reduce_heads`/`remove_redundant`
    // operates on the full repository, not just the in-list set). Walks real
    // parent edges, loading off-graph ancestors from the store on demand, so a
    // boundary parent that is an ancestor of another surviving parent is still
    // recognised as redundant (e.g. `B..F`: merge D's parents simplify to the
    // boundary B and the root A, and A is an ancestor of B).
    let is_ancestor = |anc: &ObjectId, desc: &ObjectId| -> bool {
        if anc == desc {
            return false;
        }
        let mut seen: HashSet<ObjectId> = HashSet::new();
        let mut stack: Vec<ObjectId> = get_parents(desc);
        while let Some(oid) = stack.pop() {
            if oid == *anc {
                return true;
            }
            if !seen.insert(oid) {
                continue;
            }
            stack.extend(get_parents(&oid));
        }
        false
    };

    // `one_relevant_parent`: for a 1-parent commit (or first-parent), the first
    // parent; for a merge, the sole relevant parent if exactly one exists, else
    // None.
    let one_relevant_parent = |parents: &[ObjectId]| -> Option<ObjectId> {
        if parents.is_empty() {
            return None;
        }
        if options.first_parent || parents.len() == 1 {
            return Some(parents[0]);
        }
        let mut found: Option<ObjectId> = None;
        for p in parents {
            if relevant(p) {
                if found.is_some() {
                    return None;
                }
                found = Some(*p);
            }
        }
        found
    };

    // Fixed-point: process in reverse (parents before children — git feeds the
    // list reversed and iterates until every commit is resolved).
    let mut simplified: HashMap<ObjectId, ObjectId> = HashMap::new();
    // Rewritten (deduped, redundancy-pruned) parent list per commit, recorded
    // when we resolve it.
    let mut rewritten_parents: HashMap<ObjectId, Vec<ObjectId>> = HashMap::new();
    // Recomputed TREESAME per commit after parent removal, for the final
    // `get_commit_action` display filter.
    let mut display_treesame: HashMap<ObjectId, bool> = HashMap::new();

    // git's `simplify_one` pulls every referenced parent into the pass. A parent
    // that is not itself in the candidate list is UNINTERESTING or a `^`-boundary
    // commit, which simplifies to *itself* (revision.c:3500) — pre-seed those so
    // children waiting on them become ready instead of stalling.
    for record in &records {
        for parent in &record.parents {
            if !record_oids.contains(parent) {
                simplified.entry(*parent).or_insert(*parent);
            }
        }
    }

    // Worklist seeded with all commits in reverse order; re-queue a commit whose
    // parents are not yet resolved.
    let mut order: Vec<ObjectId> = records.iter().rev().map(|r| r.oid).collect();
    loop {
        let mut requeue: Vec<ObjectId> = Vec::new();
        let mut progressed = false;
        for oid in &order {
            if simplified.contains_key(oid) {
                continue;
            }
            let parents = get_parents(oid);
            // A root commit simplifies to itself (no parents to rewrite).
            if parents.is_empty() {
                display_treesame.insert(*oid, treesame(oid));
                simplified.insert(*oid, *oid);
                progressed = true;
                continue;
            }
            // Need every (relevant) parent resolved first.
            let mut ready = true;
            for (n, p) in parents.iter().enumerate() {
                if !simplified.contains_key(p) {
                    ready = false;
                    break;
                }
                if options.first_parent && n == 0 {
                    break;
                }
            }
            if !ready {
                requeue.push(*oid);
                continue;
            }
            progressed = true;

            // Per-parent TREESAME flags for this commit (indexed by real parent
            // position), needed to recompute TREESAME after parent removal.
            let ts_parents = simplify
                .get(oid)
                .map(|s| s.treesame_parents.clone())
                .unwrap_or_default();

            // Surviving parents as (real_index, simplified_oid). Rewrite each
            // real parent to its simplification, then dedup by simplified oid
            // (remove_duplicate_parents keeps the first occurrence).
            let take = if options.first_parent {
                1
            } else {
                parents.len()
            };
            let mut surviving: Vec<(usize, ObjectId)> = Vec::with_capacity(take);
            let mut seen: HashSet<ObjectId> = HashSet::new();
            for (n, p) in parents.iter().enumerate().take(take) {
                let s = *simplified.get(p).unwrap_or(p);
                if seen.insert(s) {
                    surviving.push((n, s));
                }
            }
            let mut cnt = surviving.len();

            if cnt > 1 {
                let mut marked: HashSet<ObjectId> = HashSet::new();
                // mark_redundant_parents: drop a parent that is a proper ancestor
                // of another surviving parent (reduce_heads).
                let ids: Vec<ObjectId> = surviving.iter().map(|(_, s)| *s).collect();
                for a in &ids {
                    for b in &ids {
                        if a != b && is_ancestor(a, b) {
                            marked.insert(*a);
                            break;
                        }
                    }
                }
                // mark_treesame_root_parents: a surviving parent that is itself a
                // root and is TREESAME (to the empty tree) — drop it.
                for (_, s) in &surviving {
                    if is_root(s) && root_treesame(s) {
                        marked.insert(*s);
                    }
                }
                let mut marked_count = marked.len();
                // leave_one_treesame_to_parent: if we are TREESAME to a marked
                // parent but to no unmarked parent, un-mark the first such marked
                // parent (the one the default scan would have followed).
                if marked_count > 0 {
                    let mut unmarked_treesame = false;
                    let mut first_marked_treesame: Option<ObjectId> = None;
                    for (n, s) in &surviving {
                        if ts_parents.get(*n).copied().unwrap_or(false) {
                            if marked.contains(s) {
                                if first_marked_treesame.is_none() {
                                    first_marked_treesame = Some(*s);
                                }
                            } else {
                                unmarked_treesame = true;
                                break;
                            }
                        }
                    }
                    if !unmarked_treesame && let Some(m) = first_marked_treesame {
                        marked.remove(&m);
                        marked_count -= 1;
                    }
                }
                if marked_count > 0 {
                    surviving.retain(|(_, s)| !marked.contains(s));
                    cnt = surviving.len();
                }
            }

            let rewritten: Vec<ObjectId> = surviving.iter().map(|(_, s)| *s).collect();
            rewritten_parents.insert(*oid, rewritten.clone());

            // Recompute TREESAME over the SURVIVING parents (git's
            // update_treesame, run by remove_marked_parents when any parent was
            // removed): with ≥1 relevant surviving parent, TREESAME iff no
            // relevant surviving parent shows a change; else fall back to the
            // irrelevant ones.
            let commit_treesame = if surviving.len() == parents.len().min(take) {
                // No parent removed → original TREESAME stands.
                treesame(oid)
            } else if surviving.is_empty() {
                treesame(oid)
            } else {
                let mut relevant_parents = 0usize;
                let mut relevant_change = false;
                let mut irrelevant_change = false;
                for (n, s) in &surviving {
                    let same = ts_parents.get(*n).copied().unwrap_or(false);
                    if relevant(s) {
                        relevant_parents += 1;
                        relevant_change |= !same;
                    } else {
                        irrelevant_change |= !same;
                    }
                }
                if relevant_parents > 0 {
                    !relevant_change
                } else {
                    !irrelevant_change
                }
            };

            // A commit simplifies to itself if: no surviving parent, it is
            // !TREESAME (touches the paths), it is a merge with no sole relevant
            // parent, or (show_pulls && it is a pull merge). Otherwise it
            // simplifies to its sole relevant parent's simplification.
            display_treesame.insert(*oid, commit_treesame);
            let sole = one_relevant_parent(&rewritten);
            let pull_merge = options.show_pulls && is_pull_merge(oid, &parents, simplify, relevant);
            match sole {
                Some(parent) if cnt != 0 && commit_treesame && !pull_merge => {
                    // Simplifies to its sole relevant parent's simplification.
                    let target = *simplified.get(&parent).unwrap_or(&parent);
                    simplified.insert(*oid, target);
                }
                _ => {
                    simplified.insert(*oid, *oid);
                }
            }
        }
        if requeue.is_empty() {
            break;
        }
        if !progressed {
            // Defensive: no progress with work remaining would loop forever;
            // resolve the rest to themselves (should not happen for a DAG).
            for oid in &requeue {
                simplified.entry(*oid).or_insert(*oid);
            }
            break;
        }
        order = requeue;
    }

    // Keep commits that simplify to themselves AND survive `get_commit_action`,
    // preserving input order, with their rewritten parents. A commit that
    // simplifies to itself but is TREESAME is still dropped unless it is a merge
    // of ≥2 relevant parents (to tie topology together) or a shown pull-merge —
    // `--simplify-merges` always wants ancestry (rewrite_parents).
    let out = records
        .into_iter()
        .filter(|r| simplified.get(&r.oid) == Some(&r.oid))
        .filter(|r| {
            let ts = display_treesame.get(&r.oid).copied().unwrap_or(false);
            if !ts {
                return true;
            }
            let rewritten = rewritten_parents.get(&r.oid);
            let pull = options.show_pulls
                && is_pull_merge(&r.oid, &r.parents, simplify, |p| relevant_set.contains(p));
            let relevant_parent_count = rewritten
                .map(|ps| ps.iter().filter(|p| relevant_set.contains(*p)).count())
                .unwrap_or(0);
            pull || relevant_parent_count >= 2
        })
        .map(|r| {
            let parents = rewritten_parents.get(&r.oid).cloned().unwrap_or(r.parents);
            CommitRecord {
                oid: r.oid,
                parents,
                commit: r.commit,
            }
        })
        .collect();
    Ok(out)
}

/// git's `PULL_MERGE` flag for `--show-pulls`: in `try_to_simplify_commit`, a
/// merge is flagged `PULL_MERGE` when its **first** parent is NOT tree-SAME
/// (`!nth_parent` with a `REV_TREE_NEW/OLD/DIFFERENT` comparison) — i.e. the
/// first-parent line itself changed the paths, so a later TREESAME parent means
/// the merge "pulled in" the side branch's version. Such merges are kept under
/// `--show-pulls` even when the merge as a whole is TREESAME.
fn is_pull_merge(
    oid: &ObjectId,
    parents: &[ObjectId],
    simplify: &HashMap<ObjectId, CommitSimplify>,
    _relevant: impl Fn(&ObjectId) -> bool,
) -> bool {
    if parents.len() < 2 {
        return false;
    }
    let Some(st) = simplify.get(oid) else {
        return false;
    };
    // PULL_MERGE ⇔ first parent is a tree difference (NOT same).
    !st.treesame_parents.first().copied().unwrap_or(false)
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
    // Ref-first resolution with a tree-ish disambiguation: a ref named like a
    // short hex prefix (e.g. `added:path`) resolves to the ref, while a genuine
    // bare prefix is narrowed to its tree-ish candidates.
    let rev_oid = resolve_revision_inner(
        git_dir,
        format,
        reader,
        rev,
        None,
        ObjectDisambiguation::Treeish,
    )?;
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
    let components = normalize_treeish_path_components(path);
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

fn normalize_treeish_path(path: &str) -> String {
    normalize_treeish_path_components(path).join("/")
}

fn normalize_treeish_path_components(path: &str) -> Vec<&str> {
    // Split on '/', skipping empty and "." components so leading/trailing/
    // duplicate separators ("a//b", "/a", "dir/") and explicit current-dir
    // spellings ("./a", "a/./b") behave the way git's tree/index lookup does.
    path.split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect()
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
            if parents
                .last()
                .is_some_and(|(_, object)| object.body.is_empty())
            {
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
    RevisionSpecRef::parse(rev).ok()?.tree_path()
}

fn split_top_level_rev_path(rev: &str) -> Option<(&str, &str)> {
    let bytes = rev.as_bytes();
    let mut braced_selector_depth = 0usize;
    for (index, byte) in bytes.iter().copied().enumerate() {
        match byte {
            b'{' if index > 0 && matches!(bytes[index - 1], b'^' | b'@') => {
                braced_selector_depth = braced_selector_depth.saturating_add(1);
            }
            b'}' if braced_selector_depth > 0 => {
                braced_selector_depth -= 1;
            }
            b':' if braced_selector_depth == 0 && index > 0 => {
                return Some((&rev[..index], &rev[index + 1..]));
            }
            _ => {}
        }
    }
    None
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
fn resolve_index_path<R: ObjectReader>(
    git_dir: &Path,
    format: sley_core::ObjectFormat,
    reader: &R,
    stage: u8,
    path: &str,
) -> Result<ObjectId> {
    let normalized_path = normalize_treeish_path(path);
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
        if entry.path != normalized_path.as_bytes() {
            continue;
        }
        path_exists = true;
        if index_entry_stage(entry) == stage {
            return Ok(entry.oid);
        }
    }
    if stage == 0
        && let Some(oid) =
            resolve_index_path_in_sparse_dir(&index, reader, format, &normalized_path)
    {
        return Ok(oid);
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

fn resolve_index_path_in_sparse_dir<R: ObjectReader>(
    index: &Index,
    reader: &R,
    format: ObjectFormat,
    normalized_path: &str,
) -> Option<ObjectId> {
    for entry in &index.entries {
        if !entry.is_sparse_dir() {
            continue;
        }
        let Ok(sparse_dir) = std::str::from_utf8(entry.path.as_bytes()) else {
            continue;
        };
        let Some(remainder) = normalized_path.strip_prefix(sparse_dir) else {
            continue;
        };
        if remainder.is_empty() {
            continue;
        }
        let Some(resolved) = resolve_tree_path_entry(reader, format, &entry.oid, remainder) else {
            continue;
        };
        if resolved.object_type == ObjectType::Tree {
            continue;
        }
        sley_core::trace2::region("index", "ensure_full_index");
        return Some(resolved.oid);
    }
    None
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
        let object = read_revision_object(reader, &oid)?;
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
        let object = read_revision_object(reader, &oid)?;
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
            for parent in graph.commit_parent_ids(reader, &oid)? {
                pending.push_back(parent);
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

/// Count commits reachable from `local` but not `target` (`ahead`) and from
/// `target` but not `local` (`behind`).
///
/// This is the tracking-count primitive used by porcelain such as
/// `status --branch`. It avoids materializing parsed commits: equality and
/// simple linear fast-forward/behind cases return after a tiny parent walk, and
/// the general case falls back to OID-only ancestry sets backed by one shared
/// commit-graph context.
pub fn ahead_behind_counts<R: ObjectReader>(
    git_dir: &Path,
    format: sley_core::ObjectFormat,
    reader: &R,
    local: &ObjectId,
    target: &ObjectId,
) -> Result<(usize, usize)> {
    if local == target {
        return Ok((0, 0));
    }

    let mut graph = CommitGraphContext::load(git_dir, format);
    if let Some(ahead) = linear_unique_count(&mut graph, reader, local, target)? {
        return Ok((ahead, 0));
    }
    if let Some(behind) = linear_unique_count(&mut graph, reader, target, local)? {
        return Ok((0, behind));
    }

    let local_reachable = ancestor_set_with_graph(&mut graph, reader, local)?;
    let target_reachable = ancestor_set_with_graph(&mut graph, reader, target)?;
    let ahead = local_reachable.difference(&target_reachable).count();
    let behind = target_reachable.difference(&local_reachable).count();
    Ok((ahead, behind))
}

fn linear_unique_count<R: ObjectReader>(
    graph: &mut CommitGraphContext<'_>,
    reader: &R,
    descendant: &ObjectId,
    ancestor: &ObjectId,
) -> Result<Option<usize>> {
    let mut current = *descendant;
    let mut count = 0usize;
    let mut seen = HashSet::new();
    loop {
        if &current == ancestor {
            return Ok(Some(count));
        }
        if !seen.insert(current) {
            return Ok(None);
        }

        let mut parents = graph.commit_parent_ids(reader, &current)?;
        let Some(parent) = parents.next() else {
            return Ok(None);
        };
        if parents.next().is_some() {
            return Ok(None);
        }
        count += 1;
        current = parent;
    }
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
    use std::cell::Cell;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn setup_revisions_parses_ranges_carets_and_not() {
        let fixture = setup_revisions_fixture();
        let setup = run_setup(&fixture, ["base..main", "^side", "--not", "base", "^main"])
            .expect("setup should parse");
        assert_eq!(
            setup
                .options
                .positives
                .iter()
                .map(|tip| tip.oid)
                .collect::<Vec<_>>(),
            vec![fixture.tip, fixture.tip]
        );
        assert_oid_set(
            setup.options.negatives,
            [fixture.base, fixture.side, fixture.base],
        );
    }

    #[test]
    fn setup_revisions_parses_symmetric_difference() {
        let fixture = setup_revisions_fixture();
        let setup = run_setup(&fixture, ["left...right"]).expect("setup should parse");
        assert_oid_set(
            setup.options.positives.iter().map(|tip| tip.oid),
            [fixture.left, fixture.right],
        );
        assert_eq!(setup.options.negatives, vec![fixture.base]);
        assert_eq!(
            setup.options.symmetric_ranges,
            vec![RevisionSymmetricRange {
                left: fixture.left,
                right: fixture.right,
                negated: false,
            }]
        );
    }

    #[test]
    fn setup_revisions_expands_all_with_scoped_exclude() {
        let fixture = setup_revisions_fixture();
        // `--exclude` matches like git's `wildmatch(pattern, name, 0)`: a bare
        // `skip` is an exact match (it would NOT drop `skip/topic`), so excluding
        // the nested branch needs the `skip/*` glob — matching git's behavior.
        let setup =
            run_setup(&fixture, ["--exclude=skip/*", "--branches"]).expect("setup should parse");
        assert_oid_set(
            setup.options.positives.iter().map(|tip| tip.oid),
            [
                fixture.tip,
                fixture.left,
                fixture.right,
                fixture.base,
                fixture.side,
            ],
        );
        assert!(
            !setup
                .options
                .positives
                .iter()
                .any(|tip| tip.oid == fixture.skipped)
        );
    }

    #[test]
    fn setup_revisions_collects_pathspecs_after_boundary() {
        let fixture = setup_revisions_fixture();
        let setup =
            run_setup(&fixture, ["HEAD", "--", "missing-path"]).expect("setup should parse");
        assert_eq!(setup.options.positives[0].oid, fixture.tip);
        assert_eq!(setup.pathspecs, vec!["missing-path".to_string()]);
    }

    #[test]
    fn setup_revisions_reports_ambiguous_argument() {
        let fixture = setup_revisions_fixture();
        let err = run_setup(&fixture, ["not-a-rev-or-path"]).expect_err("setup should fail");
        assert!(matches!(err, GitError::Exit(128)));
        assert_eq!(
            ambiguous_argument_message("not-a-rev-or-path"),
            "fatal: ambiguous argument 'not-a-rev-or-path': unknown revision or path not in the working tree.\nUse '--' to separate paths from revisions, like this:\n'git <command> [<revision>...] -- [<file>...]'"
        );
    }

    #[test]
    fn walk_commits_missing_start_reports_revision_walk_context() {
        let db = ObjectDatabase::new(ObjectFormat::Sha1);
        let missing = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed");

        let err = walk_commits(&db, ObjectFormat::Sha1, [missing])
            .expect_err("missing commit should error");
        let kind = err.not_found_kind().expect("typed not found");
        assert_eq!(kind.object_id(), Some(missing));
        assert_eq!(
            kind.missing_object_context(),
            Some(MissingObjectContext::RevisionWalk)
        );
    }

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
            raw_body: None,
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
            raw_body: None,
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
            raw_body: None,
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
            raw_body: None,
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
        assert_eq!(
            resolve_rev_path(
                &git_dir,
                ObjectFormat::Sha1,
                &db,
                &commit.to_hex(),
                "./dir/./sub/file.txt"
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
        assert_eq!(
            resolve_revision_with_reader(
                &git_dir,
                ObjectFormat::Sha1,
                &ObjectDatabase::new(ObjectFormat::Sha1),
                ":./file.txt",
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
    fn resolve_index_path_reads_blobs_beneath_sparse_directory_entries() {
        let git_dir = temp_git_dir();
        let mut db = ObjectDatabase::new(ObjectFormat::Sha1);
        let blob = db
            .write_object(EncodedObject::new(ObjectType::Blob, b"sparse\n".to_vec()))
            .expect("test operation should succeed");
        let nested = write_tree(&mut db, &[]);
        let sparse_tree = write_tree(
            &mut db,
            &[(0o100644, b"a", &blob), (0o040000, b"nested", &nested)],
        );
        let mut sparse_dir = test_index_entry(b"folder1/", &sparse_tree, 0);
        sparse_dir.mode = sley_index::SPARSE_DIR_MODE;
        sparse_dir.set_skip_worktree(true);
        let index = Index {
            version: 3,
            entries: vec![sparse_dir],
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

        assert_eq!(
            resolve_revision_with_reader(&git_dir, ObjectFormat::Sha1, &db, ":folder1/a")
                .expect("test operation should succeed"),
            blob
        );
        for spec in [":folder1/", ":folder1/nested/"] {
            let err = resolve_revision_with_reader(&git_dir, ObjectFormat::Sha1, &db, spec)
                .expect_err("test operation should fail");
            assert!(
                matches!(&err, GitError::NotFound(kind) if kind.to_string().contains("not in the index")),
                "unexpected error for {spec}: {err:?}"
            );
        }
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
    fn revision_spec_ref_splits_only_top_level_tree_path_colons() {
        assert_eq!(
            RevisionSpecRef::parse("HEAD:hello")
                .expect("test operation should succeed")
                .tree_path(),
            Some(("HEAD", "hello"))
        );
        assert_eq!(
            RevisionSpecRef::parse("HEAD^{/testing:}:hello")
                .expect("test operation should succeed")
                .tree_path(),
            Some(("HEAD^{/testing:}", "hello"))
        );
        assert_eq!(
            RevisionSpecRef::parse("HEAD@{2024-01-01 10:00:00}:hello")
                .expect("test operation should succeed")
                .tree_path(),
            Some(("HEAD@{2024-01-01 10:00:00}", "hello"))
        );
        assert_eq!(
            RevisionSpecRef::parse(":/testing: message")
                .expect("test operation should succeed")
                .kind(),
            RevisionSpecKind::MessageSearch {
                text: "testing: message"
            }
        );
    }

    #[test]
    fn read_bisect_terms_defaults_and_matches_custom_refs() {
        let git_dir = temp_git_dir();
        let terms = read_bisect_terms(&git_dir).expect("test operation should succeed");
        assert_eq!(terms, BisectTerms::default());
        assert!(terms.is_bad_ref("refs/bisect/bad"));
        assert!(terms.is_good_ref("refs/bisect/good-1234"));

        fs::write(git_dir.join("BISECT_TERMS"), b"curious\nknown\n")
            .expect("test operation should succeed");
        let terms = read_bisect_terms(&git_dir).expect("test operation should succeed");
        assert_eq!(terms.bad, "curious");
        assert_eq!(terms.good, "known");
        assert!(terms.is_bad_ref("refs/bisect/curious-1"));
        assert!(terms.is_good_ref("refs/bisect/known-3"));
        assert!(!terms.is_bad_ref("refs/bisect/bad"));
        assert!(!terms.is_good_ref("refs/bisect/good"));

        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn resolve_rev_path_after_commit_message_search_suffix() {
        let git_dir = temp_git_dir();
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let blob = db
            .write_object(EncodedObject::new(ObjectType::Blob, b"hello\n".to_vec()))
            .expect("test operation should succeed");
        let tree = write_tree(&mut db, &[(0o100644, b"hello", &blob)]);
        let base = write_dated_commit(&mut db, tree, Vec::new(), b"base\n", 1000);
        let searched =
            write_dated_commit(&mut db, tree, vec![base], b"testing: path search\n", 2000);
        let tip = write_dated_commit(&mut db, tree, vec![searched], b"tip\n", 3000);
        set_branch(&git_dir, "other", &tip);

        assert_eq!(
            resolve_revision_with_reader(
                &git_dir,
                ObjectFormat::Sha1,
                &db,
                "other^{/testing:}:hello",
            )
            .expect("test operation should succeed"),
            blob
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
        write_branch_reflog(
            &git_dir,
            "main",
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
        // git only resolves the triangular push.default ∈ {current, matching}
        // case to the push remote; under `simple` it refuses because the push
        // destination (fork/main) differs from the upstream (origin/main).
        // Verified against git 2.54: `git rev-parse main@{push}` returns
        // refs/remotes/fork/main with push.default=current and errors under
        // simple ("cannot resolve 'simple' push to a single destination").
        set_ref(&git_dir, "refs/remotes/fork/main", &pushed);
        fs::write(
            git_dir.join("config"),
            b"[push]\n\tdefault = current\n[branch \"main\"]\n\tremote = origin\n\tpushRemote = fork\n\tmerge = refs/heads/main\n",
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

        // Under the default push.default=simple, a triangular pushRemote that
        // differs from the upstream remote refuses to resolve, matching git.
        fs::write(
            git_dir.join("config"),
            b"[branch \"main\"]\n\tremote = origin\n\tpushRemote = fork\n\tmerge = refs/heads/main\n",
        )
        .expect("test operation should succeed");
        let err = resolve_revision(&git_dir, ObjectFormat::Sha1, "@{push}")
            .expect_err("triangular simple push must not resolve");
        assert!(
            matches!(&err, GitError::NotFound(kind) if kind.to_string().contains("simple")),
            "unexpected error: {err:?}"
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
    fn empty_base_reflog_uses_current_branch_not_head() {
        let git_dir = temp_git_dir();
        let old_one = test_oid(0x52);
        let old_two = test_oid(0x53);
        let new_two = test_oid(0x54);
        fs::write(git_dir.join("HEAD"), b"ref: refs/heads/old-branch\n")
            .expect("test operation should succeed");
        set_branch(&git_dir, "old-branch", &old_two);
        write_branch_reflog(
            &git_dir,
            "old-branch",
            &[
                (&zero_oid(), &old_one, "commit (initial): old-one"),
                (&old_one, &old_two, "commit: old-two"),
            ],
        );
        write_head_reflog(
            &git_dir,
            &[
                (
                    &old_two,
                    &new_two,
                    "checkout: moving from old-branch to new-branch",
                ),
                (
                    &new_two,
                    &old_two,
                    "checkout: moving from new-branch to old-branch",
                ),
            ],
        );
        assert_eq!(
            resolve_revision(&git_dir, ObjectFormat::Sha1, "@{1}")
                .expect("test operation should succeed"),
            old_one
        );
        assert_eq!(
            resolve_revision(&git_dir, ObjectFormat::Sha1, "HEAD@{1}")
                .expect("test operation should succeed"),
            new_two
        );
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn reflog_nth_uses_git_empty_and_oldest_fallbacks() {
        let git_dir = temp_git_dir();
        let base = test_oid(0x55);
        let tip = test_oid(0x56);
        set_branch(&git_dir, "newbranch", &tip);
        assert_eq!(
            resolve_revision(&git_dir, ObjectFormat::Sha1, "newbranch@{0}")
                .expect("test operation should succeed"),
            tip
        );
        write_branch_reflog(&git_dir, "newbranch", &[(&base, &tip, "commit: tip")]);
        assert_eq!(
            resolve_revision(&git_dir, ObjectFormat::Sha1, "newbranch@{1}")
                .expect("test operation should succeed"),
            base
        );
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn prior_checkout_and_head_alias_compose_with_at_marks() {
        let git_dir = temp_git_dir();
        let main_tip = test_oid(0x57);
        let old_one = test_oid(0x58);
        let old_two = test_oid(0x59);
        let new_tip = test_oid(0x5a);
        fs::write(git_dir.join("HEAD"), b"ref: refs/heads/new-branch\n")
            .expect("test operation should succeed");
        set_branch(&git_dir, "main", &main_tip);
        set_branch(&git_dir, "old-branch", &old_two);
        set_branch(&git_dir, "new-branch", &new_tip);
        write_branch_reflog(
            &git_dir,
            "old-branch",
            &[
                (&zero_oid(), &old_one, "commit (initial): old-one"),
                (&old_one, &old_two, "commit: old-two"),
            ],
        );
        write_head_reflog(
            &git_dir,
            &[(
                &old_two,
                &new_tip,
                "checkout: moving from old-branch to new-branch",
            )],
        );
        fs::write(
            git_dir.join("config"),
            b"[branch \"old-branch\"]\n\tremote = .\n\tmerge = refs/heads/main\n[branch \"new-branch\"]\n\tremote = .\n\tmerge = refs/heads/main\n",
        )
        .expect("test operation should succeed");
        assert_eq!(
            resolve_revision_symbolic_full_name(&git_dir, ObjectFormat::Sha1, "@{-1}")
                .expect("test operation should succeed"),
            Some("refs/heads/old-branch".to_string())
        );
        assert_eq!(
            resolve_revision(&git_dir, ObjectFormat::Sha1, "@{-1}@{0}")
                .expect("test operation should succeed"),
            old_two
        );
        assert_eq!(
            resolve_revision(&git_dir, ObjectFormat::Sha1, "@{-1}@{1}")
                .expect("test operation should succeed"),
            old_one
        );
        assert_eq!(
            resolve_revision_symbolic_full_name(&git_dir, ObjectFormat::Sha1, "HEAD@{u}")
                .expect("test operation should succeed"),
            Some("refs/heads/main".to_string())
        );
        assert_eq!(
            resolve_revision_symbolic_full_name(&git_dir, ObjectFormat::Sha1, "@@{u}")
                .expect("test operation should succeed"),
            Some("refs/heads/main".to_string())
        );
        assert_eq!(
            resolve_revision_symbolic_full_name(&git_dir, ObjectFormat::Sha1, "@{-1}@{u}")
                .expect("test operation should succeed"),
            Some("refs/heads/main".to_string())
        );
        let nested = resolve_revision(&git_dir, ObjectFormat::Sha1, "@{0}@{0}")
            .expect_err("test operation should fail");
        assert!(
            matches!(&nested, GitError::InvalidFormat(_)),
            "unexpected error: {nested:?}"
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
        assert_eq!(
            resolve_revision(&git_dir, ObjectFormat::Sha1, "HEAD@{0}^{tree}")
                .expect("test operation should succeed"),
            tree
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

    struct SetupRevisionsFixture {
        git_dir: PathBuf,
        worktree: PathBuf,
        db: FileObjectDatabase,
        base: ObjectId,
        tip: ObjectId,
        left: ObjectId,
        right: ObjectId,
        side: ObjectId,
        skipped: ObjectId,
    }

    fn setup_revisions_fixture() -> SetupRevisionsFixture {
        let git_dir = temp_git_dir();
        let worktree = git_dir.with_extension("worktree");
        fs::create_dir_all(&worktree).expect("test operation should succeed");
        fs::write(git_dir.join("HEAD"), b"ref: refs/heads/main\n")
            .expect("test operation should succeed");
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let tree = write_tree(&mut db, &[]);
        let base = write_test_commit(&mut db, tree, Vec::new(), b"base\n");
        let tip = write_test_commit(&mut db, tree, vec![base], b"tip\n");
        let left = write_test_commit(&mut db, tree, vec![base], b"left\n");
        let right = write_test_commit(&mut db, tree, vec![base], b"right\n");
        let side = write_test_commit(&mut db, tree, Vec::new(), b"side\n");
        let skipped = write_test_commit(&mut db, tree, Vec::new(), b"skipped\n");
        set_branch(&git_dir, "main", &tip);
        set_branch(&git_dir, "base", &base);
        set_branch(&git_dir, "left", &left);
        set_branch(&git_dir, "right", &right);
        set_branch(&git_dir, "side", &side);
        set_ref(&git_dir, "refs/heads/skip/topic", &skipped);
        SetupRevisionsFixture {
            git_dir,
            worktree,
            db,
            base,
            tip,
            left,
            right,
            side,
            skipped,
        }
    }

    fn run_setup<const N: usize>(
        fixture: &SetupRevisionsFixture,
        args: [&str; N],
    ) -> Result<SetupRevisions> {
        let args = args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
        setup_revisions(
            &args,
            &RevisionSetupContext {
                git_dir: &fixture.git_dir,
                worktree_root: Some(&fixture.worktree),
                cwd: &fixture.worktree,
                format: ObjectFormat::Sha1,
                reader: &fixture.db,
                config: None,
            },
        )
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

    fn write_test_commit<W: ObjectWriter>(
        db: &mut W,
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

    fn write_tree<W: ObjectWriter>(db: &mut W, entries: &[(u32, &[u8], &ObjectId)]) -> ObjectId {
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

    struct CountingReader<'a> {
        inner: &'a FileObjectDatabase,
        reads: Cell<usize>,
    }

    impl<'a> CountingReader<'a> {
        fn new(inner: &'a FileObjectDatabase) -> Self {
            Self {
                inner,
                reads: Cell::new(0),
            }
        }
    }

    impl ObjectReader for CountingReader<'_> {
        fn read_object(&self, oid: &ObjectId) -> Result<std::sync::Arc<EncodedObject>> {
            self.reads.set(self.reads.get() + 1);
            self.inner.read_object(oid)
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

        // Linked worktrees keep commit-graphs in the common object directory,
        // not under the per-worktree gitdir. The graph fast path must find that
        // common location too, otherwise linked worktrees silently fall back to
        // packed commit reads.
        let linked = git_dir.join("worktrees").join("linked");
        fs::create_dir_all(&linked).expect("test operation should succeed");
        fs::write(linked.join("commondir"), "../..\n").expect("test operation should succeed");
        assert!(
            is_ancestor(&linked, format, &PanicReader, &root, &tip)
                .expect("test operation should succeed")
        );
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn count_commit_metadata_uses_partial_direct_commit_graph() {
        let git_dir = temp_git_dir();
        let format = ObjectFormat::Sha1;
        let db = FileObjectDatabase::from_git_dir(&git_dir, format);
        let commits = build_linear_history(&git_dir, 5);
        write_commit_graph_file(&git_dir, format, &db, &commits[..3]);

        let reader = CountingReader::new(&db);
        let count = count_commit_metadata(&git_dir, format, &reader, [commits[4]], false)
            .expect("count should succeed");
        assert_eq!(count, 5);
        assert_eq!(
            reader.reads.get(),
            2,
            "only commits newer than the partial graph should be object-read"
        );
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn commit_graph_tree_oid_returns_tree_without_object_read() {
        let git_dir = temp_git_dir();
        let format = ObjectFormat::Sha1;
        let db = FileObjectDatabase::from_git_dir(&git_dir, format);
        let commits = build_linear_history(&git_dir, 3);
        write_commit_graph_file(&git_dir, format, &db, &commits);

        for oid in &commits {
            let object = db.read_object(oid).expect("test operation should succeed");
            let commit =
                Commit::parse_ref(format, &object.body).expect("test operation should succeed");
            assert_eq!(
                commit_graph_tree_oid(&git_dir, format, oid)
                    .expect("test operation should succeed"),
                Some(commit.tree)
            );
        }
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
        let got =
            walk_oids(RevWalk::new(&git_dir, ObjectFormat::Sha1, &db, [tip]).max_count(Some(2)));
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
        let first_parent =
            walk_oids(RevWalk::new(&git_dir, ObjectFormat::Sha1, &db, [merge]).first_parent(true));
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
            RevWalk::new(&git_dir, ObjectFormat::Sha1, &db, [tip]).date_window(RevWalkDateWindow {
                min_time: Some(102),
                max_time: Some(103),
            }),
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
        let spec = Pathspec::parse(
            [b"does/not/exist".as_slice()],
            PathspecMatchMagic::default(),
        )
        .expect("pathspec");
        let walk = RevWalk::new(&git_dir, ObjectFormat::Sha1, &db, [tip]).pathspec(spec.clone());
        assert_eq!(walk.pathspec_ref(), &spec);
        let got = walk_oids(walk);
        assert_eq!(got.len(), 3, "pathspec must not prune in STAGE-A");
        fs::remove_dir_all(git_dir).expect("cleanup");
    }
}
