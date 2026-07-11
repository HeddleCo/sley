//! Extracted from the crate root (sley#8 phase 1) — code motion only.
#![allow(clippy::expect_used)]

use sley::plumbing::{sley_config, sley_index, sley_rev, sley_worktree};
// A glob of the crate root brings every shared helper/type into scope via
// descendant-privacy; see commands::stash for the rationale.
use crate::*;

fn warn_diff_rename_limit(diagnostics: sley_diff_merge::RenameLimitDiagnostics) {
    if diagnostics.inexact_copies_degraded {
        eprintln!("warning: only found copies from modified paths due to too many files.");
    } else if diagnostics.any_skipped() {
        eprintln!("warning: exhaustive rename detection was skipped due to too many files.");
    }
}

/// Peel a single revision string to the tree it names (commit/tag/tree all work).
fn diff_peel_rev_tree(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    rev: &str,
) -> Result<ObjectId> {
    let oid = sley_rev::RevisionResolver::new(git_dir, format, db).resolve(rev)?;
    sley_rev::peel_to_tree(db, format, &oid)
}

pub(crate) fn diff_resolve_commit_arg(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    rev: &str,
) -> Result<ObjectId> {
    let oid = sley_rev::RevisionResolver::new(git_dir, format, db).resolve(rev)?;
    match sley_rev::peel_to_commit(db, format, &oid) {
        Ok(commit) => Ok(commit),
        Err(err) => {
            if let Ok(object) = db.read_object(&oid) {
                eprintln!(
                    "error: object {oid} is a {}, not a commit",
                    object.object_type.as_str()
                );
                Err(GitError::Exit(128))
            } else {
                Err(err)
            }
        }
    }
}

pub(crate) fn diff_single_merge_base(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    left: &ObjectId,
    right: &ObjectId,
) -> Result<ObjectId> {
    let bases = sley_rev::merge_bases(git_dir, format, db, left, right)?;
    match bases.as_slice() {
        [] => {
            eprintln!("fatal: no merge base found");
            Err(GitError::Exit(128))
        }
        [base] => Ok(*base),
        _ => {
            eprintln!("fatal: multiple merge bases found");
            Err(GitError::Exit(128))
        }
    }
}

fn diff_split_revisions(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    path_args: Vec<String>,
) -> Result<(Vec<ObjectId>, Vec<String>)> {
    let Some(first) = path_args.first() else {
        return Ok((Vec::new(), Vec::new()));
    };
    // Range forms name exactly two trees and consume only the first token. Check
    // `...` before `..` so `A...B` is not mis-split, and require both sides so a
    // relative path like `../x` (left side empty) is never taken as a range.
    // `A...B` (symmetric): diff merge-base(A,B)..B. An omitted side defaults to
    // HEAD. It is only a range when *both* endpoints resolve as revisions —
    // otherwise the token (e.g. a relative path `../x`) falls through to pathspec
    // handling, matching git's disambiguation.
    if let Some((left, right)) = first.split_once("...") {
        let left_spec = if left.is_empty() { "HEAD" } else { left };
        let right_spec = if right.is_empty() { "HEAD" } else { right };
        if let (Ok(left_oid), Ok(right_oid)) = (
            diff_resolve_commit_arg(git_dir, format, db, left_spec),
            diff_resolve_commit_arg(git_dir, format, db, right_spec),
        ) {
            if path_args[1..]
                .iter()
                .any(|arg| diff_arg_looks_like_extra_revision(git_dir, format, db, arg))
            {
                return diff_usage_error();
            }
            let bases = sley_rev::merge_bases(git_dir, format, db, &left_oid, &right_oid)?;
            let Some(base) = bases.first() else {
                eprintln!("fatal: {first}: no merge base");
                return Err(GitError::Exit(128));
            };
            if bases.len() > 1 {
                eprintln!("warning: {first}: multiple merge bases, using {base}");
            }
            let base_tree = sley_rev::peel_to_tree(db, format, base)?;
            let right_tree = sley_rev::peel_to_tree(db, format, &right_oid)?;
            return Ok((vec![base_tree, right_tree], path_args[1..].to_vec()));
        }
    }
    // `A..B`: diff A..B. Omitted side defaults to HEAD; only a range when both
    // endpoints resolve.
    if let Some((left, right)) = first.split_once("..") {
        let left_spec = if left.is_empty() { "HEAD" } else { left };
        let right_spec = if right.is_empty() { "HEAD" } else { right };
        if let (Ok(left_tree), Ok(right_tree)) = (
            diff_peel_rev_tree(git_dir, format, db, left_spec),
            diff_peel_rev_tree(git_dir, format, db, right_spec),
        ) {
            if path_args[1..]
                .iter()
                .any(|arg| diff_arg_looks_like_extra_revision(git_dir, format, db, arg))
            {
                return diff_usage_error();
            }
            return Ok((vec![left_tree, right_tree], path_args[1..].to_vec()));
        }
    }
    if path_args[1..]
        .iter()
        .any(|arg| diff_arg_is_revision_range(git_dir, format, db, arg))
    {
        return diff_usage_error();
    }
    // Otherwise peel up to three leading args that each resolve as a revision.
    // The three-tree form is a combined diff: result parent1 parent2.
    let mut trees = Vec::new();
    let mut rest = Vec::new();
    let mut iter = path_args.into_iter();
    for token in iter.by_ref() {
        if trees.len() < 3
            && let Ok(tree) = diff_peel_rev_tree(git_dir, format, db, &token)
        {
            trees.push(tree);
            continue;
        }
        rest.push(token);
        break;
    }
    rest.extend(iter);
    Ok((trees, rest))
}

fn diff_split_merge_base(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    _cached: bool,
    path_args: Vec<String>,
) -> Result<(Vec<ObjectId>, Vec<String>)> {
    let mut commits: Vec<(ObjectId, String)> = Vec::new();
    let mut rest = Vec::new();
    let mut iter = path_args.into_iter();
    for token in iter.by_ref() {
        if diff_arg_is_revision_range(git_dir, format, db, &token) {
            eprintln!("fatal: --merge-base does not work with ranges");
            return Err(GitError::Exit(128));
        }
        if commits.len() < 2
            && sley_rev::RevisionResolver::new(git_dir, format, db)
                .resolve(&token)
                .is_ok()
        {
            let commit = diff_resolve_commit_arg(git_dir, format, db, &token)?;
            commits.push((commit, token));
            continue;
        }
        rest.push(token);
        break;
    }
    rest.extend(iter);
    if commits.is_empty() {
        return diff_usage_error();
    }
    if commits.len() == 2
        && rest
            .iter()
            .any(|arg| diff_arg_looks_like_extra_revision(git_dir, format, db, arg))
    {
        return diff_usage_error();
    }

    let head_storage;
    let (left, right) = if commits.len() == 1 {
        head_storage = diff_resolve_commit_arg(git_dir, format, db, "HEAD")?;
        (&head_storage, &commits[0].0)
    } else {
        (&commits[0].0, &commits[1].0)
    };
    let base = diff_single_merge_base(git_dir, format, db, left, right)?;
    let base_tree = sley_rev::peel_to_tree(db, format, &base)?;
    if commits.len() == 1 {
        Ok((vec![base_tree], rest))
    } else {
        let right_tree = sley_rev::peel_to_tree(db, format, right)?;
        Ok((vec![base_tree, right_tree], rest))
    }
}

fn diff_arg_is_revision_range(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    arg: &str,
) -> bool {
    if let Some((left, right)) = arg.split_once("...") {
        let left = if left.is_empty() { "HEAD" } else { left };
        let right = if right.is_empty() { "HEAD" } else { right };
        return diff_resolve_commit_arg(git_dir, format, db, left).is_ok()
            && diff_resolve_commit_arg(git_dir, format, db, right).is_ok();
    }
    if let Some((left, right)) = arg.split_once("..") {
        let left = if left.is_empty() { "HEAD" } else { left };
        let right = if right.is_empty() { "HEAD" } else { right };
        return diff_peel_rev_tree(git_dir, format, db, left).is_ok()
            && diff_peel_rev_tree(git_dir, format, db, right).is_ok();
    }
    false
}

fn diff_arg_looks_like_extra_revision(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    arg: &str,
) -> bool {
    if let Some((left, right)) = arg.split_once("...") {
        let left = if left.is_empty() { "HEAD" } else { left };
        let right = if right.is_empty() { "HEAD" } else { right };
        return diff_resolve_commit_arg(git_dir, format, db, left).is_ok()
            && diff_resolve_commit_arg(git_dir, format, db, right).is_ok();
    }
    if let Some((left, right)) = arg.split_once("..") {
        let left = if left.is_empty() { "HEAD" } else { left };
        let right = if right.is_empty() { "HEAD" } else { right };
        return diff_peel_rev_tree(git_dir, format, db, left).is_ok()
            && diff_peel_rev_tree(git_dir, format, db, right).is_ok();
    }
    diff_peel_rev_tree(git_dir, format, db, arg).is_ok()
}

#[derive(Clone)]
struct IndexBlobSpec {
    path: Vec<u8>,
    mode: u32,
    oid: ObjectId,
}

#[derive(Clone)]
struct DirectBlobSpec {
    path: Vec<u8>,
    mode: u32,
    oid: ObjectId,
    content: Vec<u8>,
    anonymous: bool,
    file: bool,
}

fn diff_direct_blob_pair(
    cwd: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    args: &[String],
) -> Result<Option<(DirectBlobSpec, DirectBlobSpec)>> {
    let pair = if args.len() == 1 {
        let Some((left, right)) = args[0].split_once("..") else {
            return Ok(None);
        };
        if left.is_empty() || right.is_empty() {
            return Ok(None);
        }
        let Some(left) = resolve_direct_blob_source(cwd, git_dir, format, db, left, false)? else {
            return Ok(None);
        };
        let Some(right) = resolve_direct_blob_source(cwd, git_dir, format, db, right, false)?
        else {
            return Ok(None);
        };
        (left, right)
    } else if args.len() == 2 {
        let Some(left) = resolve_direct_blob_source(cwd, git_dir, format, db, &args[0], true)?
        else {
            return Ok(None);
        };
        let Some(right) = resolve_direct_blob_source(cwd, git_dir, format, db, &args[1], true)?
        else {
            return Ok(None);
        };
        if left.file && right.file {
            return Ok(None);
        }
        (left, right)
    } else {
        return Ok(None);
    };

    let (mut left, mut right) = pair;
    if left.anonymous && right.file {
        left.path.clone_from(&right.path);
    } else if right.anonymous && left.file {
        right.path.clone_from(&left.path);
    }
    Ok(Some((left, right)))
}

fn resolve_direct_blob_source(
    cwd: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    spec: &str,
    allow_file: bool,
) -> Result<Option<DirectBlobSpec>> {
    if let Some((revision, path)) = spec.split_once(':')
        && !revision.is_empty()
        && !path.is_empty()
        && let Ok(entry) = sley_rev::resolve_rev_path_entry(git_dir, format, db, revision, path)
        && entry.object_type == ObjectType::Blob
    {
        let object = db.read_object(&entry.oid)?;
        return Ok(Some(DirectBlobSpec {
            path: path.as_bytes().to_vec(),
            mode: entry.mode.unwrap_or(0o100644),
            oid: entry.oid,
            content: object.body.clone(),
            anonymous: false,
            file: false,
        }));
    }

    if let Ok(oid) = sley_rev::RevisionResolver::new(git_dir, format, db).resolve(spec)
        && let Ok(object) = db.read_object(&oid)
        && object.object_type == ObjectType::Blob
    {
        return Ok(Some(DirectBlobSpec {
            path: spec.as_bytes().to_vec(),
            mode: 0o100644,
            oid,
            content: object.body.clone(),
            anonymous: true,
            file: false,
        }));
    }

    if !allow_file {
        return Ok(None);
    }
    let absolute = cwd.join(spec);
    if !absolute.is_file() {
        return Ok(None);
    }
    let content = fs::read(&absolute)?;
    let oid = sley_core::object_id_for_bytes(format, "blob", &content)?;
    let mode = direct_blob_file_mode(git_dir, format, spec.as_bytes(), &absolute)?;
    Ok(Some(DirectBlobSpec {
        path: spec.as_bytes().to_vec(),
        mode,
        oid,
        content,
        anonymous: false,
        file: true,
    }))
}

fn direct_blob_file_mode(
    git_dir: &Path,
    format: ObjectFormat,
    path: &[u8],
    absolute: &Path,
) -> Result<u32> {
    let mode = sley_worktree::read_repository_index(git_dir, format)?
        .and_then(|index| {
            index
                .entries
                .into_iter()
                .find(|entry| {
                    entry.stage() == sley_index::Stage::Normal && entry.path.as_bytes() == path
                })
                .map(|entry| entry.mode)
        })
        .unwrap_or_else(|| direct_blob_filesystem_mode(absolute));
    Ok(mode)
}

#[cfg(unix)]
fn direct_blob_filesystem_mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;

    fs::metadata(path)
        .ok()
        .map(|metadata| {
            if metadata.permissions().mode() & 0o111 == 0 {
                0o100644
            } else {
                0o100755
            }
        })
        .unwrap_or(0o100644)
}

#[cfg(not(unix))]
fn direct_blob_filesystem_mode(_path: &Path) -> u32 {
    0o100644
}

fn diff_index_blob_pair(
    git_dir: &Path,
    format: ObjectFormat,
    args: &[String],
) -> Result<Option<(IndexBlobSpec, IndexBlobSpec)>> {
    if args.len() != 2 || !args.iter().all(|arg| arg.starts_with(':')) {
        return Ok(None);
    }
    let index_path = sley_worktree::repository_index_path(git_dir);
    let index = Index::parse(&fs::read(index_path)?, format)?;
    let left = resolve_stage0_index_blob(&index, &args[0])?;
    let right = resolve_stage0_index_blob(&index, &args[1])?;
    Ok(Some((left, right)))
}

fn resolve_stage0_index_blob(index: &Index, spec: &str) -> Result<IndexBlobSpec> {
    let path = spec
        .strip_prefix(':')
        .filter(|path| !path.is_empty() && !path.starts_with(':'))
        .ok_or_else(|| GitError::Command(format!("unsupported index blob spec {spec}")))?;
    let path = path.as_bytes();
    let entry = index
        .entries
        .iter()
        .find(|entry| entry.stage() == sley_index::Stage::Normal && entry.path.as_bytes() == path)
        .ok_or_else(|| {
            GitError::Command(format!(
                "path '{}' is not in the index",
                String::from_utf8_lossy(path)
            ))
        })?;
    Ok(IndexBlobSpec {
        path: path.to_vec(),
        mode: entry.mode,
        oid: entry.oid,
    })
}

fn write_index_blob_raw_diff(
    left: &IndexBlobSpec,
    right: &IndexBlobSpec,
    abbrev: Option<usize>,
    format: ObjectFormat,
    z: bool,
) -> Result<()> {
    if left.oid == right.oid && left.mode == right.mode {
        return Ok(());
    }
    let mut stdout = io::stdout().lock();
    let old_oid = abbreviate_index_blob_oid(&left.oid, abbrev, format);
    let new_oid = abbreviate_index_blob_oid(&right.oid, abbrev, format);
    write!(
        stdout,
        ":{:06o} {:06o} {} {} M\t{}",
        left.mode,
        right.mode,
        old_oid,
        new_oid,
        String::from_utf8_lossy(&right.path)
    )?;
    if z {
        stdout.write_all(&[0])?;
    } else {
        writeln!(stdout)?;
    }
    Ok(())
}

fn abbreviate_index_blob_oid(
    oid: &ObjectId,
    abbrev: Option<usize>,
    format: ObjectFormat,
) -> String {
    let hex = oid.to_hex();
    match abbrev {
        Some(width) => hex[..width.min(format.hex_len())].to_string(),
        None => hex,
    }
}

fn diff_usage_error<T>() -> Result<T> {
    eprintln!("usage: git diff [<options>] [<commit>] [--] [<path>...]");
    Err(GitError::Exit(129))
}

/// The `whitespace` attribute + `core.whitespace` config, resolved into the
/// per-path rule lookups `git diff --check` / `git apply` need.
///
/// `core.whitespace` forms the base rule; the per-path `whitespace` attribute
/// (read from the real worktree's `.gitattributes`) overrides it the way git's
/// `whitespace_rule` does.
pub(crate) struct WhitespaceRuleResolver {
    config_rule: sley_diff_merge::ws::WsRule,
    matcher: Option<sley_worktree::StandardAttributeMatcher>,
}

const DEFAULT_CONFLICT_MARKER_SIZE: usize = 7;

struct DiffCheckRules {
    whitespace: sley_diff_merge::ws::WsRule,
    conflict_marker_size: usize,
}

impl WhitespaceRuleResolver {
    /// Build a resolver from a git dir: reads `core.whitespace` and opens the
    /// worktree attribute matcher (best-effort — a bare repo has no worktree).
    ///
    /// A conflicting `core.whitespace` (both `tab-in-indent` and
    /// `indent-with-non-tab`) is fatal, mirroring git's `parse_whitespace_rule`
    /// `die`.
    pub(crate) fn from_git_dir(git_dir: &Path) -> Result<Self> {
        let config = read_repo_config(git_dir).ok();
        Self::from_git_dir_with_config(git_dir, config.as_ref())
    }

    pub(crate) fn from_git_dir_with_config(
        git_dir: &Path,
        config: Option<&GitConfig>,
    ) -> Result<Self> {
        let config_rule = match config
            .and_then(|config| config.get("core", None, "whitespace").map(str::to_owned))
        {
            Some(value) => match sley_diff_merge::ws::parse_whitespace_rule(&value) {
                Some(rule) => rule,
                None => return Err(whitespace_conflict_error()),
            },
            None => sley_diff_merge::ws::WS_DEFAULT_RULE,
        };
        let matcher = sley_worktree::worktree_root_for_git_dir(git_dir)
            .ok()
            .flatten()
            .and_then(|root| {
                sley_worktree::StandardAttributeMatcher::from_worktree_root(root).ok()
            });
        Ok(Self {
            config_rule,
            matcher,
        })
    }

    /// Resolve the effective rule for `path`. A conflicting attribute *value*
    /// is fatal (git `die`s), like a conflicting `core.whitespace`.
    pub(crate) fn rule_for_path(&self, path: &[u8]) -> Result<sley_diff_merge::ws::WsRule> {
        use sley::plumbing::sley_diff_merge::ws::{WsAttr, resolve_whitespace_rule};
        let Some(matcher) = &self.matcher else {
            return Ok(self.config_rule);
        };
        let requested = vec![b"whitespace".to_vec()];
        let checks = matcher.attributes_for_path(path, &requested, false);
        let value_storage;
        let attr = match checks.first().and_then(|check| check.state.as_ref()) {
            Some(sley_worktree::AttributeState::Set) => WsAttr::True,
            Some(sley_worktree::AttributeState::Unset) => WsAttr::False,
            Some(sley_worktree::AttributeState::Value(value)) => {
                value_storage = String::from_utf8_lossy(value).into_owned();
                WsAttr::Value(&value_storage)
            }
            None => WsAttr::Unset,
        };
        resolve_whitespace_rule(self.config_rule, attr).ok_or_else(whitespace_conflict_error)
    }

    fn check_rules_for_path(&self, path: &[u8]) -> Result<DiffCheckRules> {
        use sley::plumbing::sley_diff_merge::ws::{WsAttr, resolve_whitespace_rule};
        let Some(matcher) = &self.matcher else {
            return Ok(DiffCheckRules {
                whitespace: self.config_rule,
                conflict_marker_size: DEFAULT_CONFLICT_MARKER_SIZE,
            });
        };
        let requested = vec![b"whitespace".to_vec(), b"conflict-marker-size".to_vec()];
        let checks = matcher.attributes_for_path(path, &requested, false);
        let mut value_storage = String::new();
        let whitespace_attr = match checks.first().and_then(|check| check.state.as_ref()) {
            Some(sley_worktree::AttributeState::Set) => WsAttr::True,
            Some(sley_worktree::AttributeState::Unset) => WsAttr::False,
            Some(sley_worktree::AttributeState::Value(value)) => {
                value_storage = String::from_utf8_lossy(value).into_owned();
                WsAttr::Value(&value_storage)
            }
            None => WsAttr::Unset,
        };
        let whitespace = resolve_whitespace_rule(self.config_rule, whitespace_attr)
            .ok_or_else(whitespace_conflict_error)?;
        let conflict_marker_size =
            conflict_marker_size_from_attr(checks.get(1).and_then(|check| check.state.as_ref()));
        Ok(DiffCheckRules {
            whitespace,
            conflict_marker_size,
        })
    }
}

/// git's fatal error for an unenforceable whitespace rule pair.
fn whitespace_conflict_error() -> GitError {
    eprintln!("fatal: cannot enforce both tab-in-indent and indent-with-non-tab");
    GitError::Exit(128)
}

fn conflict_marker_size_from_attr(state: Option<&sley_worktree::AttributeState>) -> usize {
    let Some(sley_worktree::AttributeState::Value(value)) = state else {
        return DEFAULT_CONFLICT_MARKER_SIZE;
    };
    let raw = String::from_utf8_lossy(value);
    match raw.parse::<isize>() {
        Ok(size) if size > 0 => size as usize,
        _ => {
            eprintln!("warning: invalid marker-size '{raw}', expecting an integer");
            DEFAULT_CONFLICT_MARKER_SIZE
        }
    }
}

/// Run `git diff --check` over the computed diff entries. For each entry it
/// diffs old vs new content, runs git's whitespace check on every introduced
/// (`+`) line, and prints `<path>:<lineno>: <error>.` plus the offending line,
/// mirroring git's `checkdiff`. Returns `true` if any whitespace error (or
/// leftover conflict marker) was found.
pub(crate) fn run_diff_check(
    entries: &[sley_diff_merge::NameStatusEntry],
    db: &FileObjectDatabase,
    worktree_root: Option<&Path>,
    use_worktree_old: bool,
    use_worktree_new: bool,
    worktree_clean: Option<&DiffWorktreeCleanContext<'_>>,
    resolver: &WhitespaceRuleResolver,
    lazy_fetch: bool,
) -> Result<bool> {
    let mut stdout = io::stdout();
    let mut status = false;
    for entry in entries {
        // git only checks the new side, and skips entries with no new content
        // (pure deletions) and gitlinks/symlinks-as-content edge cases handled
        // by the content fetchers returning None.
        if entry.new_mode == Some(0o160000) {
            continue;
        }
        let new_content = diff_entry_new_content(
            entry,
            db,
            worktree_root,
            use_worktree_new,
            worktree_clean,
            lazy_fetch,
        )?;
        let Some(new_content) = new_content else {
            continue;
        };
        let old_content = diff_entry_old_content_for_diff(
            entry,
            db,
            worktree_root,
            use_worktree_old,
            worktree_clean,
            lazy_fetch,
        )?
        .unwrap_or_default();
        let path = status_quote_path(&entry.path, false);
        // A symlink target being an incomplete line is not news (git clears
        // WS_INCOMPLETE_LINE for symlinks). We don't track symlink mode here in
        // a way that distinguishes, so leave the rule intact — the t-suite does
        // exercise a symlink incomplete-line case in t4015.
        let rules = resolver.check_rules_for_path(&entry.path)?;
        let mut rule = rules.whitespace;
        if entry.new_mode == Some(0o120000) {
            rule &= !sley_diff_merge::ws::WS_INCOMPLETE_LINE;
        }
        if check_one_diff(
            &mut stdout,
            &old_content,
            &new_content,
            &path,
            rule,
            rules.conflict_marker_size,
        )? {
            status = true;
        }
    }
    Ok(status)
}

fn diff_entry_old_content_for_diff(
    entry: &sley_diff_merge::NameStatusEntry,
    db: &FileObjectDatabase,
    worktree_root: Option<&Path>,
    use_worktree_old: bool,
    worktree_clean: Option<&DiffWorktreeCleanContext<'_>>,
    lazy_fetch: bool,
) -> Result<Option<Vec<u8>>> {
    if !use_worktree_old {
        return diff_entry_old_content(entry, db, lazy_fetch);
    }
    let Some(mode) = entry.old_mode else {
        return Ok(None);
    };
    if mode == 0o160000 {
        return Ok(entry
            .old_oid
            .as_ref()
            .map(|oid| gitlink_diff_content(oid, false)));
    }
    let root = worktree_root.ok_or_else(|| {
        GitError::Command("diff -R requires a worktree for worktree comparisons".into())
    })?;
    let path = root.join(repo_path_to_path(
        entry.old_path.as_deref().unwrap_or(&entry.path),
    ));
    if mode == 0o120000 {
        return read_symlink_bytes_for_diff(&path).map(Some);
    }
    if path.exists() {
        let content = fs::read(path)?;
        let attr_path = entry.old_path.as_deref().unwrap_or(&entry.path);
        return match worktree_clean {
            Some(clean) => clean
                .attributes
                .apply_clean_filter(clean.config, attr_path, &content)
                .map(Some),
            None => Ok(Some(content)),
        };
    }
    Ok(None)
}

#[cfg(unix)]
fn read_symlink_bytes_for_diff(path: &Path) -> Result<Vec<u8>> {
    use std::os::unix::ffi::OsStrExt;
    Ok(fs::read_link(path)?.as_os_str().as_bytes().to_vec())
}

#[cfg(not(unix))]
fn read_symlink_bytes_for_diff(path: &Path) -> Result<Vec<u8>> {
    Ok(fs::read_link(path)?
        .to_string_lossy()
        .replace('\\', "/")
        .into_bytes())
}

/// Check a single old/new pair, writing the `checkdiff` report to `out`.
fn check_one_diff(
    out: &mut impl Write,
    old_content: &[u8],
    new_content: &[u8],
    path: &str,
    rule: sley_diff_merge::ws::WsRule,
    conflict_marker_size: usize,
) -> Result<bool> {
    use sley::plumbing::sley_diff_merge::ws;
    let old = sley_diff_merge::split_lines(old_content);
    let new = sley_diff_merge::split_lines(new_content);
    let ops = sley_diff_merge::myers_diff_lines(&old, &new);

    let mut status = false;
    let mut new_lineno = 0usize; // 1-based number of the current new-side line
    let mut new_idx = 0usize;
    let mut last_kind = b' ';
    for op in ops {
        match op {
            sley_diff_merge::DiffOp::Equal(n) => {
                for _ in 0..n {
                    new_lineno += 1;
                    new_idx += 1;
                    last_kind = b' ';
                }
            }
            sley_diff_merge::DiffOp::Delete(_) => {
                // Removed lines don't advance the new-side counter.
            }
            sley_diff_merge::DiffOp::Insert(n) => {
                for _ in 0..n {
                    new_lineno += 1;
                    let line = new[new_idx].content;
                    new_idx += 1;
                    last_kind = b'+';
                    // git strips the `+` prefix; our `line` is already prefix-free.
                    if is_conflict_marker(line, conflict_marker_size) {
                        status = true;
                        writeln!(out, "{path}:{new_lineno}: leftover conflict marker")?;
                    }
                    let bad = ws::ws_check(line, rule);
                    if bad != 0 {
                        status = true;
                        let err = ws::whitespace_error_string(bad);
                        writeln!(out, "{path}:{new_lineno}: {err}.")?;
                        // Echo the offending `+` line (no color).
                        out.write_all(b"+")?;
                        out.write_all(line)?;
                        if !line.ends_with(b"\n") {
                            out.write_all(b"\n")?;
                        }
                    }
                }
            }
        }
    }
    let _ = last_kind;

    // Blank-at-EOF is detected globally (not per inserted line): git compares
    // the trailing-blank run of pre- and post-images.
    if rule & ws::WS_BLANK_AT_EOF != 0 {
        let l1 = ws::count_trailing_blank(old_content);
        let l2 = ws::count_trailing_blank(new_content);
        if l2 > l1 {
            let at = ws::count_lines(new_content);
            let blank_at_eof = at - l2 + 1;
            let err = ws::whitespace_error_string(ws::WS_BLANK_AT_EOF);
            writeln!(out, "{path}:{blank_at_eof}: {err}.")?;
            status = true;
        }
    }
    Ok(status)
}

fn is_conflict_marker(line: &[u8], marker_size: usize) -> bool {
    if line.len() < marker_size + 1 {
        return false;
    }
    let first = line[0];
    if !matches!(first, b'=' | b'>' | b'<' | b'|') {
        return false;
    }
    if !line[..marker_size].iter().all(|byte| *byte == first) {
        return false;
    }
    line[marker_size].is_ascii_whitespace()
}

#[derive(Clone)]
struct ExternalDiffCommand {
    command: String,
    trust_exit_code: bool,
}

struct ExternalDiffRunOptions<'a> {
    quiet: bool,
    exit_code: bool,
    output: Option<&'a str>,
    autocrlf: bool,
}

fn global_external_diff_command(config: Option<&GitConfig>) -> Option<ExternalDiffCommand> {
    if let Ok(command) = env::var("GIT_EXTERNAL_DIFF")
        && !command.is_empty()
    {
        let trust_exit_code = env::var("GIT_EXTERNAL_DIFF_TRUST_EXIT_CODE")
            .ok()
            .and_then(|value| sley_config::parse_config_bool(&value))
            .unwrap_or(false);
        return Some(ExternalDiffCommand {
            command,
            trust_exit_code,
        });
    }
    let config = config?;
    let command = config.get("diff", None, "external")?.to_string();
    let trust_exit_code = config
        .get_bool("diff", None, "trustexitcode")
        .unwrap_or(false);
    Some(ExternalDiffCommand {
        command,
        trust_exit_code,
    })
}

fn run_external_diff_entries(
    entries: &[sley_diff_merge::NameStatusEntry],
    lookup_entries: &DiffRelativeLookupMap,
    db: &FileObjectDatabase,
    cwd: &Path,
    worktree_root: Option<&Path>,
    use_worktree_new: bool,
    userdiff: &commands::userdiff::UserdiffResolver,
    global: Option<&ExternalDiffCommand>,
    options: ExternalDiffRunOptions<'_>,
    lazy_fetch: bool,
) -> Result<Option<i32>> {
    let mut handled = false;
    let mut found_changes = false;
    let git_prefix = external_diff_git_prefix(cwd, worktree_root);
    let mut output_file = match options.output {
        Some(path) if !options.quiet => Some(
            fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(path)?,
        ),
        _ => None,
    };

    for (idx, entry) in entries.iter().enumerate() {
        let lookup_entry = diff_relative_lookup_entry(entry, lookup_entries);
        let command = external_diff_for_entry(lookup_entry, userdiff, global)?;
        let Some(command) = command else {
            continue;
        };
        handled = true;
        if options.quiet && !command.trust_exit_code {
            found_changes = true;
            continue;
        }
        let mut context = ExternalDiffProcessContext {
            db,
            worktree_root,
            git_prefix: git_prefix.clone(),
            use_worktree_new,
            autocrlf: options.autocrlf,
            quiet: options.quiet,
            output_file: output_file.as_mut(),
            lazy_fetch,
        };
        let rc = run_one_external_diff(
            entry,
            lookup_entry,
            &command,
            idx + 1,
            entries.len(),
            &mut context,
        )?;
        match (command.trust_exit_code, rc) {
            (false, 0) => found_changes = true,
            (true, 0) => {}
            (true, 1) => found_changes = true,
            _ => {
                let path = String::from_utf8_lossy(&entry.path);
                eprintln!("fatal: external diff died, stopping at {path}");
                return Err(GitError::Exit(128));
            }
        }
    }

    if !handled {
        return Ok(None);
    }
    let code = if (options.quiet || options.exit_code) && found_changes {
        1
    } else {
        0
    };
    Ok(Some(code))
}

fn external_diff_for_entry(
    entry: &sley_diff_merge::NameStatusEntry,
    userdiff: &commands::userdiff::UserdiffResolver,
    global: Option<&ExternalDiffCommand>,
) -> Result<Option<ExternalDiffCommand>> {
    let attr_path = entry.old_path.as_deref().unwrap_or(&entry.path);
    if let Some(driver) = userdiff.driver_for_path(attr_path)?
        && let Some(external) = &driver.external
    {
        return Ok(Some(ExternalDiffCommand {
            command: external.command.clone(),
            trust_exit_code: external.trust_exit_code,
        }));
    }
    Ok(global.cloned())
}

struct ExternalDiffProcessContext<'a> {
    db: &'a FileObjectDatabase,
    worktree_root: Option<&'a Path>,
    git_prefix: Option<String>,
    use_worktree_new: bool,
    autocrlf: bool,
    quiet: bool,
    output_file: Option<&'a mut fs::File>,
    lazy_fetch: bool,
}

fn run_one_external_diff(
    entry: &sley_diff_merge::NameStatusEntry,
    lookup_entry: &sley_diff_merge::NameStatusEntry,
    command: &ExternalDiffCommand,
    counter: usize,
    total: usize,
    context: &mut ExternalDiffProcessContext<'_>,
) -> Result<i32> {
    let old_file = prepare_external_diff_file(
        entry,
        lookup_entry,
        context.db,
        context.worktree_root,
        context.use_worktree_new,
        false,
        context.autocrlf,
        context.lazy_fetch,
    )?;
    let new_file = prepare_external_diff_file(
        entry,
        lookup_entry,
        context.db,
        context.worktree_root,
        context.use_worktree_new,
        true,
        context.autocrlf,
        context.lazy_fetch,
    )?;
    let path = String::from_utf8_lossy(&entry.path).into_owned();
    let old_hex = external_diff_oid(
        lookup_entry.old_oid.as_ref(),
        context.db.object_format(),
        false,
    );
    let new_hex = external_diff_oid(
        lookup_entry.new_oid.as_ref(),
        context.db.object_format(),
        context.use_worktree_new,
    );
    let old_mode = external_diff_mode(lookup_entry.old_mode);
    let new_mode = external_diff_mode(lookup_entry.new_mode);
    let args = [
        path,
        old_file.path.to_string_lossy().into_owned(),
        old_hex,
        old_mode,
        new_file.path.to_string_lossy().into_owned(),
        new_hex,
        new_mode,
    ];
    let shell_command = format!("{} \"$@\"", command.command);
    let mut child = ProcessCommand::new("sh");
    child
        .arg("-c")
        .arg(shell_command)
        .arg(&command.command)
        .args(args)
        .env("GIT_DIFF_PATH_COUNTER", counter.to_string())
        .env("GIT_DIFF_PATH_TOTAL", total.to_string());
    if let Some(root) = context.worktree_root {
        child.current_dir(root);
    }
    if let Some(prefix) = &context.git_prefix {
        child.env("GIT_PREFIX", prefix);
    } else {
        child.env_remove("GIT_PREFIX");
    }
    if context.quiet {
        child.stdout(std::process::Stdio::null());
    } else if let Some(file) = context.output_file.as_mut() {
        child.stdout(file.try_clone()?);
    }
    let status = child.status()?;
    Ok(status.code().unwrap_or(128))
}

fn external_diff_git_prefix(cwd: &Path, worktree_root: Option<&Path>) -> Option<String> {
    let root = worktree_root?;
    let relative = cwd.strip_prefix(root).ok()?;
    if relative.as_os_str().is_empty() {
        return None;
    }
    Some(format!(
        "{}/",
        relative.to_string_lossy().replace('\\', "/")
    ))
}

fn external_diff_oid(oid: Option<&ObjectId>, format: ObjectFormat, zero: bool) -> String {
    if zero {
        ObjectId::null(format).to_hex()
    } else {
        oid.copied()
            .unwrap_or_else(|| ObjectId::null(format))
            .to_hex()
    }
}

fn external_diff_mode(mode: Option<u32>) -> String {
    format!("{:06o}", mode.unwrap_or(0))
}

struct ExternalDiffFile {
    path: PathBuf,
    temp_dir: Option<PathBuf>,
}

impl Drop for ExternalDiffFile {
    fn drop(&mut self) {
        if let Some(dir) = &self.temp_dir {
            let _ = fs::remove_dir_all(dir);
        }
    }
}

fn prepare_external_diff_file(
    entry: &sley_diff_merge::NameStatusEntry,
    lookup_entry: &sley_diff_merge::NameStatusEntry,
    db: &FileObjectDatabase,
    worktree_root: Option<&Path>,
    use_worktree_new: bool,
    new_side: bool,
    autocrlf: bool,
    lazy_fetch: bool,
) -> Result<ExternalDiffFile> {
    if new_side
        && use_worktree_new
        && lookup_entry.new_mode != Some(0o160000)
        && let Some(root) = worktree_root
    {
        let absolute = root.join(repo_path_to_path(&lookup_entry.path));
        if absolute.exists() {
            return Ok(ExternalDiffFile {
                path: repo_path_to_path(&lookup_entry.path),
                temp_dir: None,
            });
        }
    }
    let content = if new_side {
        diff_entry_new_content(
            lookup_entry,
            db,
            worktree_root,
            use_worktree_new,
            None,
            lazy_fetch,
        )?
    } else {
        diff_entry_old_content(lookup_entry, db, lazy_fetch)?
    };
    let Some(content) = content else {
        return Ok(ExternalDiffFile {
            path: PathBuf::from("/dev/null"),
            temp_dir: None,
        });
    };
    let temp_dir = unique_external_diff_temp_dir()?;
    let repo_path = if new_side {
        &entry.path
    } else {
        entry.old_path.as_ref().unwrap_or(&entry.path)
    };
    let path = temp_dir.join(repo_path_to_path(repo_path));
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let content = if autocrlf && external_temp_should_crlf(entry, new_side, &content) {
        lf_to_crlf(&content)
    } else {
        content
    };
    fs::write(&path, content)?;
    Ok(ExternalDiffFile {
        path,
        temp_dir: Some(temp_dir),
    })
}

fn external_temp_should_crlf(
    entry: &sley_diff_merge::NameStatusEntry,
    new_side: bool,
    content: &[u8],
) -> bool {
    let mode = if new_side {
        entry.new_mode
    } else {
        entry.old_mode
    };
    mode == Some(0o100644) && !content.contains(&0)
}

fn lf_to_crlf(content: &[u8]) -> Vec<u8> {
    let extra = content.iter().filter(|byte| **byte == b'\n').count();
    let mut out = Vec::with_capacity(content.len() + extra);
    let mut previous = None;
    for byte in content {
        if *byte == b'\n' && previous != Some(b'\r') {
            out.push(b'\r');
        }
        out.push(*byte);
        previous = Some(*byte);
    }
    out
}

fn unique_external_diff_temp_dir() -> Result<PathBuf> {
    for attempt in 0..1000u32 {
        let dir = env::temp_dir().join(format!("sley-extdiff-{}-{}", std::process::id(), attempt));
        match fs::create_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err.into()),
        }
    }
    Err(GitError::Command(
        "unable to create external diff temporary directory".into(),
    ))
}

fn diff_should_use_implicit_no_index(
    path_args: &[String],
    explicit_paths: &[String],
    outside_repository: bool,
) -> bool {
    let total = path_args.len() + explicit_paths.len();
    if total != 2 {
        return false;
    }
    outside_repository
        || path_args
            .iter()
            .chain(explicit_paths)
            .any(|path| diff_arg_looks_outside_worktree(path))
}

fn diff_arg_looks_outside_worktree(path: &str) -> bool {
    if path == "-" || Path::new(path).is_absolute() {
        return true;
    }
    Path::new(path)
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
}

pub(crate) fn cmd_diff(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
    let lazy_fetch = cli_session.lazy_fetch();
    let sley_rev::diff_options::DiffOptions {
        output_format,
        cached,
        quiet,
        exit_code,
        allow_external,
        output,
        line_prefix,
        compact_summary,
        stat_count,
        stat_widths,
        mut dirstat,
        dirstat_cli_params,
        context,
        reverse,
        pickaxe,
        pickaxe_all,
        pickaxe_regex,
        find_object_values,
        raw_abbrev,
        patch_abbrev,
        patch_full_index,
        patch_binary,
        mut color_always,
        color_moved,
        color_moved_ws,
        diff_algorithm_control,
        diff_algorithm,
        anchored,
        diff_driver_control,
        diff_hunk_control,
        interhunk,
        diff_whitespace_control,
        ws_error_highlight,
        indent_heuristic,
        ws_ignore,
        ignore_blank_lines,
        ignore_regexes,
        diff_output_indicator_control,
        diff_patch_context_control,
        diff_patch_output_control,
        diff_rewrite_control,
        diff_submodule_format,
        word_diff_mode,
        word_diff_regex,
        no_index,
        combined,
        mut diff_relative,
        diff_relative_explicit,
        mut src_prefix,
        mut dst_prefix,
        cli_no_prefix,
        cli_default_prefix,
        cli_src_prefix,
        cli_dst_prefix,
        mut head,
        z,
        mut detect_renames,
        mut detect_copies,
        find_copies_harder,
        rename_empty,
        mut inexact_renames,
        renames_explicit,
        rename_threshold,
        copy_threshold,
        rename_limit,
        diff_filter,
        ignore_submodules_cli,
        merge_base,
        orderfile,
        rotate_to,
        rotate_skip,
        mut path_args,
        explicit_paths,
    } = sley_rev::diff_options::setup_diff_options(args)?;

    let name_status = output_format.contains(sley_rev::diff_options::DiffOutputFormat::NAME_STATUS);
    let name_only = output_format.contains(sley_rev::diff_options::DiffOutputFormat::NAME_ONLY);
    let check = output_format.contains(sley_rev::diff_options::DiffOutputFormat::CHECK);
    let summary = output_format.contains(sley_rev::diff_options::DiffOutputFormat::SUMMARY);
    let raw = output_format.contains(sley_rev::diff_options::DiffOutputFormat::RAW);
    let stat = output_format.contains(sley_rev::diff_options::DiffOutputFormat::DIFFSTAT);
    let numstat = output_format.contains(sley_rev::diff_options::DiffOutputFormat::NUMSTAT);
    let shortstat = output_format.contains(sley_rev::diff_options::DiffOutputFormat::SHORTSTAT);
    let patch = output_format.contains(sley_rev::diff_options::DiffOutputFormat::PATCH);
    let no_patch = output_format.contains(sley_rev::diff_options::DiffOutputFormat::NO_OUTPUT);
    if diff_algorithm_control && !name_status && !name_only {
        return Err(GitError::Unsupported(
            "diff algorithm controls are not supported for this output mode".into(),
        ));
    }
    if diff_driver_control && !name_status && !name_only {
        return Err(GitError::Unsupported(
            "diff driver controls are not supported for this output mode".into(),
        ));
    }
    if diff_hunk_control && !name_status && !name_only {
        return Err(GitError::Unsupported(
            "diff hunk context controls are not supported for this output mode".into(),
        ));
    }
    let _ = diff_whitespace_control;
    if diff_output_indicator_control && !name_status && !name_only {
        return Err(GitError::Unsupported(
            "diff output indicator controls are not supported for this output mode".into(),
        ));
    }
    let no_index_patch_context = if diff_patch_context_control {
        sley_diff_merge::render::enable_function_context(context.unwrap_or(3))
    } else {
        context.unwrap_or(3)
    };
    if diff_patch_output_control && !name_status && !name_only {
        return Err(GitError::Unsupported(
            "diff patch output controls are not supported for this output mode".into(),
        ));
    }
    let stat_family = stat || compact_summary || numstat || shortstat;
    if diff_rewrite_control && !name_status && !name_only && !stat_family {
        return Err(GitError::Unsupported(
            "diff rewrite controls are not supported for this output mode".into(),
        ));
    }
    if (pickaxe.is_some() || pickaxe_all || pickaxe_regex) && !name_status && !name_only {
        return Err(GitError::Unsupported(
            "diff pickaxe controls are not supported for this output mode".into(),
        ));
    }
    // Probe once because `diff --no-index` is valid outside a repository while
    // every repository-backed mode must reuse one session-scoped facade.
    let repository = RepositoryContext::from_session(cli_session);
    let outside_repository = repository.is_err();
    if !find_object_values.is_empty() && outside_repository {
        eprintln!("fatal: --find-object requires a git repository");
        return Err(GitError::Exit(128));
    }
    if !find_object_values.is_empty() && !name_status && !name_only {
        return Err(GitError::Unsupported(
            "diff find-object output is not supported for this output mode".into(),
        ));
    }
    // `--check` is handled below (after the entries are computed) rather than
    // here, where the diff content isn't available yet.
    // Compile the `-I<regex>` (`--ignore-matching-lines`) patterns up front so
    // a malformed regex fails like git's `diff_opt_ignore_regex` (exit 129).
    let ignore_regexes = crate::compile_ignore_matching_regexes(&ignore_regexes)?;
    if pickaxe_all && !find_object_values.is_empty() {
        return diff_find_object_pickaxe_all_conflict_error();
    }
    if pickaxe.is_some() && pickaxe_regex {
        return Err(GitError::Unsupported(
            "diff pickaxe regex matching is not supported".into(),
        ));
    }
    let cwd = cli_session.cwd().to_path_buf();
    let implicit_no_index = !no_index
        && diff_should_use_implicit_no_index(&path_args, &explicit_paths, outside_repository);
    if no_index || implicit_no_index {
        let mut paths = path_args;
        if head {
            paths.insert(0, "HEAD".to_string());
        }
        paths.extend(explicit_paths);
        return cmd_diff_no_index(
            &cwd,
            &paths,
            DiffNoIndexParams {
                context: no_index_patch_context,
                color: color_always,
                color_moved_cli: color_moved,
                color_moved_ws_cli: color_moved_ws,
                output_format,
                raw_abbrev,
                patch_abbrev,
                patch_full_index,
                patch_binary,
                allow_external,
                exit_code,
                output: output.as_deref(),
                reverse,
                z,
                word_diff_mode,
                word_diff_regex: word_diff_regex.as_deref(),
                src_prefix: &src_prefix,
                dst_prefix: &dst_prefix,
                cli_no_prefix,
                cli_default_prefix,
                cli_src_prefix: cli_src_prefix.as_deref(),
                cli_dst_prefix: cli_dst_prefix.as_deref(),
                quiet,
                interhunk: interhunk.unwrap_or(0),
                ws_ignore,
                diff_algorithm,
                ignore_blank_lines,
                ignore_regexes: &ignore_regexes,
                // `--no-index` honors the CLI flag; config (when inside a repo)
                // is resolved below for the in-repo path, so here we fall back
                // to git's enabled-by-default behavior absent an explicit flag.
                indent_heuristic: indent_heuristic.unwrap_or(true),
                anchored: &anchored,
                lazy_fetch,
            },
            repository.as_ref().ok(),
        );
    }
    let repo = repository?;
    let git_dir = repo.git_dir().to_path_buf();
    let repo_config = Some(repo.config().clone());
    let suppress_blank_empty = repo_config
        .as_ref()
        .and_then(|config| config.get_bool("diff", None, "suppressblankempty"))
        .unwrap_or(false);
    let resolved_context =
        sley_rev::diff_options::resolve_diff_context(context, repo_config.as_ref())?;
    let patch_context = if diff_patch_context_control {
        sley_diff_merge::render::enable_function_context(resolved_context)
    } else {
        resolved_context
    };
    // git's `quote_path_fully` (`core.quotePath`, default true): drives whether
    // non-ASCII bytes in diffstat names are octal-escaped or shown verbatim.
    let quote_path_fully = repo_config
        .as_ref()
        .and_then(|config| config.get_bool("core", None, "quotepath"))
        .unwrap_or(true);
    let format = repo.format();
    if let Some(config) = repo_config.as_ref() {
        if let Some(value) = config.get("diff", None, "colormoved")
            && let Err(err) = log_validate_color_moved(value)
        {
            eprintln!("fatal: bad config variable 'diff.colormoved' from command-line config");
            return Err(err);
        }
        if let Some(value) = config.get("diff", None, "colormovedws")
            && let Err(err) = log_validate_color_moved_ws(value)
        {
            eprintln!("fatal: bad config variable 'diff.colormovedws' from command-line config");
            return Err(err);
        }
    }
    let color_moved_mode = match color_moved {
        Some(mode) => mode,
        None => match repo_config
            .as_ref()
            .and_then(|config| config.get("diff", None, "colormoved").map(str::to_owned))
        {
            Some(value) => sley_rev::diff_options::parse_color_moved_mode(&value)?,
            None => None,
        },
    };
    let color_moved_ws = match color_moved_ws {
        Some(ws) => ws,
        None => match repo_config
            .as_ref()
            .and_then(|config| config.get("diff", None, "colormovedws").map(str::to_owned))
        {
            Some(value) => sley_rev::diff_options::parse_color_moved_ws(&value)?,
            None => sley_diff_merge::render::ColorMovedWs::default(),
        },
    };
    let color_moved = color_moved_mode.map(|mode| sley_diff_merge::render::ColorMoved {
        mode,
        ws: color_moved_ws,
    });
    if !color_always
        && repo_config
            .as_ref()
            .and_then(|config| config.get("diff", None, "color").map(str::to_owned))
            .is_some_and(|value| git_config_color_is_always(&value))
    {
        color_always = true;
    }
    let ws_error_highlight = ws_error_highlight.or_else(|| {
        repo_config.as_ref().and_then(|config| {
            config
                .get("diff", None, "wserrorhighlight")
                .map(str::to_owned)
        })
    });
    let ws_error_kinds = parse_ws_error_highlight_kinds(ws_error_highlight.as_deref());
    let diff_submodule_format = diff_submodule_format.or_else(|| {
        repo_config
            .as_ref()
            .and_then(|config| config.get("diff", None, "submodule").map(str::to_owned))
            .and_then(|value| match value.as_str() {
                "short" | "log" | "diff" => {
                    Some(sley_rev::diff_options::SubmoduleDiffFormat::parse(&value))
                }
                _ => None,
            })
    });
    let submodule_format =
        diff_submodule_format.unwrap_or(sley_rev::diff_options::SubmoduleDiffFormat::Short);
    // A writable-capable view is cheap and shares the facade's pack/decoded
    // caches; existing diff helpers take a concrete ODB reference.
    let db = repo.repository().objects_mut();
    if !head
        && !merge_base
        && !reverse
        && explicit_paths.is_empty()
        && let Some((left, right)) = diff_index_blob_pair(&git_dir, format, &path_args)?
    {
        let has_differences = left.oid != right.oid || left.mode != right.mode;
        if !quiet && raw {
            let abbrev = raw_abbrev
                .unwrap_or(Some(7))
                .map(|width| width.min(format.hex_len()));
            write_index_blob_raw_diff(&left, &right, abbrev, format, z)?;
        }
        if (quiet || exit_code) && has_differences {
            return Err(GitError::Exit(1));
        }
        return Ok(());
    }
    // Resolve the diff path prefixes from `diff.*Prefix` config, then layer the
    // CLI overrides on top (git's diff_setup_done → diff_opt_* precedence):
    //  * `diff.noPrefix` ⇒ both prefixes empty.
    //  * `diff.mnemonicPrefix` ⇒ the comparison's mnemonic letters. For the
    //    worktree-vs-index `git diff` here that is `i/` (index) and `w/`
    //    (worktree); `diff.{src,dst}Prefix` are ignored under mnemonicPrefix.
    //  * else `diff.srcPrefix`/`diff.dstPrefix` (defaulting to `a/`/`b/`).
    // CLI `--no-prefix`/`--default-prefix`/`--src-prefix`/`--dst-prefix` always
    // win over config.
    if let Some(config) = repo_config.as_ref() {
        let cfg_no_prefix = config.get_bool("diff", None, "noprefix").unwrap_or(false);
        let cfg_mnemonic = config
            .get_bool("diff", None, "mnemonicprefix")
            .unwrap_or(false);
        let (mut cfg_src, mut cfg_dst) = if cfg_no_prefix {
            (String::new(), String::new())
        } else if cfg_mnemonic {
            ("i/".to_string(), "w/".to_string())
        } else {
            (
                config
                    .get("diff", None, "srcprefix")
                    .map(str::to_owned)
                    .unwrap_or_else(|| "a/".to_string()),
                config
                    .get("diff", None, "dstprefix")
                    .map(str::to_owned)
                    .unwrap_or_else(|| "b/".to_string()),
            )
        };
        // CLI overrides (highest precedence).
        if cli_default_prefix {
            cfg_src = "a/".to_string();
            cfg_dst = "b/".to_string();
        }
        if cli_no_prefix {
            cfg_src.clear();
            cfg_dst.clear();
        }
        if let Some(p) = &cli_src_prefix {
            cfg_src = p.clone();
        }
        if let Some(p) = &cli_dst_prefix {
            cfg_dst = p.clone();
        }
        src_prefix = cfg_src;
        dst_prefix = cfg_dst;
    }
    if !diff_relative_explicit
        && let Some(config) = repo_config.as_ref()
        && config.get_bool("diff", None, "relative").unwrap_or(false)
    {
        diff_relative = sley_rev::diff_options::DiffRelativeMode::Cwd;
    }
    // `--indent-heuristic` / `--no-indent-heuristic` (CLI) win over
    // `diff.indentHeuristic` config, which itself defaults to git's
    // enabled-by-default behavior.
    let indent_heuristic = indent_heuristic.unwrap_or_else(|| {
        repo_config
            .as_ref()
            .and_then(|config| config.get_bool("diff", None, "indentheuristic"))
            .unwrap_or(true)
    });
    if !renames_explicit
        && let Some(config) = repo_config.as_ref()
        && let Some(value) = config.get("diff", None, "renames")
    {
        match value.trim().to_ascii_lowercase().as_str() {
            "false" | "no" | "off" | "0" => {
                detect_renames = false;
                detect_copies = false;
                inexact_renames = false;
            }
            "copies" | "copy" => {
                detect_renames = true;
                detect_copies = true;
                inexact_renames = true;
            }
            "true" | "yes" | "on" | "1" | "renames" => {
                detect_renames = true;
                inexact_renames = true;
            }
            _ => {}
        }
    }
    // `-O<orderfile>` / `diff.orderfile`: a CLI `-O` overrides config, and
    // `-O/dev/null` cancels a configured orderfile (it reads as zero patterns).
    // The file itself is only read once a non-empty diff exists, matching git's
    // `diffcore_order` (which early-returns when the queue is empty).
    let resolved_orderfile = orderfile.or_else(|| {
        repo_config
            .as_ref()
            .and_then(|config| config.get("diff", None, "orderfile").map(str::to_owned))
    });
    if let Some(opts) = dirstat.as_mut() {
        // diff.dirstat config forms the base (bad parameters warn); explicit
        // --dirstat parameters apply on top (bad parameters are fatal).
        let mut base = DirstatOptions::default();
        if let Some(config) = repo_config.as_ref()
            && let Some(value) = config.get("diff", None, "dirstat")
        {
            let mut errors = String::new();
            if parse_dirstat_params(value, &mut base, &mut errors) > 0 {
                eprint!("warning: Found errors in 'diff.dirstat' config variable:\n{errors}");
            }
        }
        // Flags parsed inline (--cumulative / --dirstat-by-file) already
        // modified `opts`; merge them onto the config base.
        if opts.cumulative {
            base.cumulative = true;
        }
        if opts.mode == DirstatMode::Files {
            base.mode = DirstatMode::Files;
        }
        let mut errors = String::new();
        let mut error_count = 0usize;
        for params in &dirstat_cli_params {
            error_count += parse_dirstat_params(params, &mut base, &mut errors);
        }
        if error_count > 0 {
            eprint!("fatal: Failed to parse --dirstat/-X option parameter:\n{errors}");
            return Err(GitError::Exit(128));
        }
        *opts = base;
    }
    let direct_blob_patch = !head
        && !cached
        && !merge_base
        && explicit_paths.is_empty()
        && !quiet
        && !color_always
        && !patch_binary
        && !raw
        && !name_status
        && !name_only
        && !stat
        && !numstat
        && !shortstat
        && !summary
        && !check
        && dirstat.is_none()
        && !no_patch
        && output.is_none()
        && line_prefix.is_none()
        && interhunk.unwrap_or(0) == 0
        && color_moved.is_none()
        && word_diff_mode.is_none()
        && anchored.is_empty()
        && ws_ignore.is_empty()
        && ignore_regexes.is_empty()
        && !ignore_blank_lines;
    if direct_blob_patch
        && let Some((left, right)) = diff_direct_blob_pair(&cwd, &git_dir, format, &db, &path_args)?
    {
        let mut stdout = io::stdout().lock();
        sley_diff_merge::porcelain::render_blob_patch(
            &mut stdout,
            sley_diff_merge::porcelain::BlobDiffSide {
                oid: left.oid,
                mode: left.mode,
                path: &left.path,
                content: &left.content,
            },
            sley_diff_merge::porcelain::BlobDiffSide {
                oid: right.oid,
                mode: right.mode,
                path: &right.path,
                content: &right.content,
            },
            sley_diff_merge::porcelain::BlobPatchOptions {
                object_format: format,
                full_index: patch_full_index,
                abbrev: patch_abbrev.unwrap_or(7),
                src_prefix: src_prefix.as_bytes(),
                dst_prefix: dst_prefix.as_bytes(),
                context: patch_context,
                algorithm: diff_algorithm,
                indent_heuristic,
            },
        )
        .map_err(|error| GitError::Io(error.to_string()))?;
        if exit_code && (left.oid != right.oid || left.mode != right.mode) {
            return Err(GitError::Exit(1));
        }
        return Ok(());
    }
    // Pull any leading `<rev>` / `<rev> <rev>` / `<rev>..<rev>` / `<rev>...<rev>`
    // out of the positional arguments; the remainder are pathspecs. Without this,
    // `diff A B` was treated as two paths and silently fell back to an
    // index-vs-worktree diff (wrong output, and a full-worktree rescan on big
    // repos).
    // A bare `diff HEAD` keeps its dedicated head-vs-worktree path, but
    // `diff HEAD <rev>` / `diff HEAD HEAD` means the consumed HEAD is the first of
    // several revisions — hand it back to the splitter.
    if head && (!path_args.is_empty() || merge_base) {
        path_args.insert(0, "HEAD".to_string());
        head = false;
    }
    let (diff_trees, mut path_args) = if merge_base {
        diff_split_merge_base(&git_dir, format, &db, cached, path_args)?
    } else {
        diff_split_revisions(&git_dir, format, &db, path_args)?
    };
    path_args.extend(explicit_paths);
    let find_objects = resolve_diff_find_objects(&git_dir, format, &db, &find_object_values)?;
    let render_selection = sley_diff_merge::porcelain::select_render_formats(
        sley_diff_merge::porcelain::RenderSelectionOptions {
            default_output: sley_diff_merge::porcelain::DefaultDiffOutput::Patch,
            raw,
            patch,
            name_status,
            name_only,
            stat: stat || compact_summary,
            numstat,
            shortstat,
            summary,
            auxiliary_format: dirstat.is_some(),
            suppress_output: no_patch,
        },
    );
    let output_may_show_oids = !quiet && (render_selection.raw || render_selection.patch);
    let needs_raw_abbrev = output_may_show_oids && render_selection.raw && raw_abbrev.is_none();
    let needs_patch_abbrev = output_may_show_oids
        && render_selection.patch
        && !patch_full_index
        && patch_abbrev.is_none();
    let repository_abbrev = if needs_raw_abbrev || needs_patch_abbrev {
        repo.abbrev()?
    } else {
        None
    };
    let raw_abbrev = match raw_abbrev {
        Some(abbrev) => abbrev.map(|width| width.min(format.hex_len())),
        // `git diff` is porcelain: raw oids abbreviate by default (unlike the
        // diff-tree plumbing), to core.abbrev or git's standard 7.
        None => Some(repository_abbrev.unwrap_or(7).min(format.hex_len())),
    };
    let patch_abbrev = if patch_full_index {
        format.hex_len()
    } else {
        patch_abbrev
            .or(repository_abbrev)
            .unwrap_or(7)
            .min(format.hex_len())
    };
    let worktree_root = if cached {
        repo.worktree_root().ok().map(Path::to_path_buf)
    } else {
        Some(repo.worktree_root()?.to_path_buf())
    };
    if !cached
        && diff_trees.len() != 2
        && let Some(worktree_root) = &worktree_root
    {
        commands::submodule::ensure_populated_gitlinks_readable(worktree_root, &git_dir, format)?;
    }
    let pathspec = if path_args.is_empty() {
        DiffPathspec::default()
    } else {
        let worktree_root = match worktree_root.as_deref() {
            Some(root) => root,
            None => repo.worktree_root()?,
        };
        DiffPathspec::new(&cwd, worktree_root, &path_args, repo.pathspec_magic())?
    };
    if diff_trees.len() == 3 {
        let has_differences = write_diff_combined_three_tree(
            &db,
            format,
            &diff_trees,
            &pathspec,
            CombinedDiffOptions {
                dense: combined.unwrap_or(true),
                output_format,
                patch_abbrev,
                raw_abbrev,
                z,
                src_prefix: &src_prefix,
                dst_prefix: &dst_prefix,
                context: patch_context,
                ws_ignore,
                diff_algorithm,
                line_prefix: line_prefix.as_deref(),
                orderfile: resolved_orderfile.as_deref(),
                lazy_fetch,
            },
        )?;
        if (quiet || exit_code) && has_differences {
            return Err(GitError::Exit(1));
        }
        return Ok(());
    }
    // The new-side oid is real (shown, not zeroed) when it comes from a tree or the
    // index; it is zeroed only when the new side is the worktree.
    let zero_worktree_oids = match diff_trees.len() {
        2 => false,
        1 => !cached,
        _ => !cached && !head,
    };
    let plain_index_worktree_diff = diff_trees.is_empty() && !cached && !head;
    // The new side's *content* comes from the worktree only when there is no second
    // tree and we're not diffing the index (`--cached`). A two-tree `diff A B` takes
    // its new content from tree B's blobs, never the worktree.
    let use_worktree_new = !cached && diff_trees.len() != 2;
    let base_options = sley_diff_merge::DiffNameStatusOptions {
        detect_renames,
        detect_copies,
        find_copies_harder,
        rename_empty,
        ..Default::default()
    };
    let options = sley_diff_merge::DiffNameStatusOptions {
        detect_renames,
        detect_copies,
        find_copies_harder,
        rename_empty,
        detect_inexact: true,
        rename_threshold,
        copy_threshold,
        rename_limit,
        ..Default::default()
    };
    let mut precomputed_staged_gitlinks = None;
    let mut rename_limit_diagnostics = sley_diff_merge::RenameLimitDiagnostics::default();
    let entries = if !diff_trees.is_empty() {
        match diff_trees.as_slice() {
            // `diff <rev>`: that tree vs the worktree (or the index with --cached).
            [tree] => {
                reject_duplicate_tree_for_index_diff(&db, format, tree)?;
                if cached {
                    if inexact_renames {
                        let diff =
                            sley_diff_merge::diff_name_status_tree_index_with_options_and_diagnostics(
                            &git_dir,
                            format,
                            tree,
                            options,
                        )?;
                        rename_limit_diagnostics = diff.rename_limit;
                        diff.entries
                    } else {
                        sley_diff_merge::diff_name_status_tree_index_with_options(
                            &git_dir, format, tree, options,
                        )?
                    }
                } else {
                    let worktree_root = worktree_root
                        .as_ref()
                        .expect("worktree root set for diff <rev>");
                    if inexact_renames {
                        sley_diff_merge::diff_name_status_tree_worktree_with_options(
                            worktree_root,
                            &git_dir,
                            format,
                            tree,
                            options,
                        )?
                    } else {
                        sley_diff_merge::diff_name_status_tree_worktree_with_options(
                            worktree_root,
                            &git_dir,
                            format,
                            tree,
                            options,
                        )?
                    }
                }
            }
            // `diff <rev> <rev>` / `<rev>..<rev>` / `<rev>...<rev>`: tree vs tree.
            [left, right] => {
                if inexact_renames {
                    let diff =
                        sley_diff_merge::diff_name_status_trees_with_options_and_diagnostics(
                            &db, format, left, right, options,
                        )?;
                    rename_limit_diagnostics = diff.rename_limit;
                    diff.entries
                } else {
                    sley_diff_merge::diff_name_status_trees_with_options(
                        &db, format, left, right, options,
                    )?
                }
            }
            _ => {
                return Err(GitError::Unsupported(
                    "diff accepts at most two revisions".into(),
                ));
            }
        }
    } else if cached {
        if inexact_renames {
            let diff = sley_diff_merge::diff_name_status_head_index_with_options_and_diagnostics(
                &git_dir, format, options,
            )?;
            rename_limit_diagnostics = diff.rename_limit;
            diff.entries
        } else {
            sley_diff_merge::diff_name_status_head_index_with_options(&git_dir, format, options)?
        }
    } else if head {
        let head_tree = diff_peel_rev_tree(&git_dir, format, &db, "HEAD")?;
        reject_duplicate_tree_for_index_diff(&db, format, &head_tree)?;
        let worktree_root = worktree_root
            .as_ref()
            .expect("worktree root set for diff HEAD");
        if inexact_renames {
            sley_diff_merge::diff_name_status_head_worktree_with_options(
                worktree_root,
                &git_dir,
                format,
                options,
            )?
        } else {
            sley_diff_merge::diff_name_status_head_worktree_with_options(
                worktree_root,
                &git_dir,
                format,
                options,
            )?
        }
    } else {
        let worktree_root = worktree_root.as_ref().expect("worktree root set for diff");
        let mut stat_clean_validator = sley_worktree::StatCleanFilterValidator::new();
        let mut validate_stat_clean =
            |entry: sley_diff_merge::IndexWorktreeValidationEntry<'_>,
             absolute_path: &Path,
             metadata: &fs::Metadata| {
                stat_clean_validator.validate_path(
                    worktree_root,
                    &git_dir,
                    format,
                    entry.mode,
                    entry.oid,
                    entry.size,
                    entry.path,
                    absolute_path,
                    metadata,
                )
            };
        let diff = if inexact_renames {
            sley_diff_merge::diff_name_status_index_worktree_with_options_and_gitlinks_validated(
                worktree_root,
                &git_dir,
                format,
                base_options,
                &mut validate_stat_clean,
            )?
        } else {
            sley_diff_merge::diff_name_status_index_worktree_with_options_and_gitlinks_validated(
                worktree_root,
                &git_dir,
                format,
                options,
                &mut validate_stat_clean,
            )?
        };
        precomputed_staged_gitlinks = Some(diff.staged_gitlinks);
        diff.entries
    };
    // Submodule-ignore handling: drop `all`-ignored gitlink entries, then for
    // worktree-involved diffs collect each staged submodule's dirt (for the
    // `-dirty` patch suffix) and append dirty-but-same-commit pairs the map
    // comparison alone cannot see.
    let skip_submodule_work = matches!(precomputed_staged_gitlinks.as_deref(), Some([]));
    let (entries, dirty_submodules) = if skip_submodule_work {
        (entries, HashMap::new())
    } else {
        let submodule_config = submodule_diff_config_with_config(
            &git_dir,
            worktree_root.as_deref(),
            ignore_submodules_cli,
            repo_config.as_ref(),
        );
        let mut entries = apply_submodule_ignore_filter(entries, &submodule_config);
        let dirty_submodules = match (use_worktree_new, worktree_root.as_deref()) {
            (true, Some(root)) => collect_dirty_submodules(
                &mut entries,
                &git_dir,
                format,
                root,
                &submodule_config,
                precomputed_staged_gitlinks.as_deref(),
            )?,
            _ => HashMap::new(),
        };
        (entries, dirty_submodules)
    };
    let entries = apply_diff_pathspec(entries, &pathspec);
    let entries = if let Some(needle) = pickaxe.as_deref() {
        let worktree_clean_attributes = if use_worktree_new {
            worktree_root
                .as_deref()
                .map(sley_worktree::WorktreeAttributes::from_worktree_root)
                .transpose()?
        } else {
            None
        };
        let worktree_clean = match (repo_config.as_ref(), worktree_clean_attributes.as_ref()) {
            (Some(config), Some(attributes)) => {
                Some(DiffWorktreeCleanContext { config, attributes })
            }
            _ => None,
        };
        apply_diff_pickaxe(
            entries,
            needle.as_bytes(),
            pickaxe_all,
            &db,
            worktree_root.as_deref(),
            use_worktree_new,
            worktree_clean.as_ref(),
            lazy_fetch,
        )?
    } else if pickaxe_all || pickaxe_regex {
        sort_diff_entries_by_path(entries)
    } else {
        entries
    };
    let entries = apply_diff_find_objects(entries, &find_objects);
    let entries = if reverse {
        reverse_diff_entries(entries)
    } else {
        entries
    };
    let use_worktree_old = reverse && use_worktree_new;
    let use_worktree_new = use_worktree_new && !reverse;
    // `--relative` rewrites displayed paths only; content, raw oid, and
    // external-diff worktree lookups still resolve against the original paths.
    let worktree_root = worktree_root;
    let mut relative_lookup_entries = DiffRelativeLookupMap::new();
    let entries = if matches!(diff_relative, sley_rev::diff_options::DiffRelativeMode::Off) {
        entries
    } else {
        let prefix = diff_relative_prefix(cli_session, &diff_relative, &cwd, &git_dir)?;
        let (entries, lookups) = apply_diff_relative(entries, &prefix);
        relative_lookup_entries = lookups;
        entries
    };
    let worktree_clean_attributes = if use_worktree_new || use_worktree_old {
        worktree_root
            .as_deref()
            .map(sley_worktree::WorktreeAttributes::from_worktree_root)
            .transpose()?
    } else {
        None
    };
    let worktree_clean = match (repo_config.as_ref(), worktree_clean_attributes.as_ref()) {
        (Some(config), Some(attributes)) => Some(DiffWorktreeCleanContext { config, attributes }),
        _ => None,
    };
    if reverse {
        std::mem::swap(&mut src_prefix, &mut dst_prefix);
    }
    let entries: Vec<_> = if diff_filter.all_or_none {
        if !diff_filter.includes.is_empty()
            && entries.iter().any(|entry| {
                pathspec.matches(&entry.path) && diff_filter.matches_status(entry.status.code())
            })
        {
            entries
        } else {
            Vec::new()
        }
    } else {
        entries
            .into_iter()
            .filter(|entry| diff_filter.matches_status(entry.status.code()))
            .collect()
    };
    // `--exit-code`/`--quiet` and raw/name/stat output reflect whether any
    // *visible* diff remains. With `-w`/`-b`/eol or `--ignore-blank-lines` /
    // `-I<regex>`, a content change can reduce to nothing; git then drops the
    // pair before formatting.
    let ignore_active = !ws_ignore.is_empty() || ignore_blank_lines || !ignore_regexes.is_empty();
    let entries = if ignore_active {
        let mut visible = Vec::with_capacity(entries.len());
        for entry in entries {
            let lookup_entry = diff_relative_lookup_entry(&entry, &relative_lookup_entries);
            if diff_entry_produces_output(
                lookup_entry,
                &db,
                worktree_root.as_deref(),
                use_worktree_new,
                worktree_clean.as_ref(),
                interhunk.unwrap_or(0),
                patch_context,
                ws_ignore,
                ignore_blank_lines,
                &ignore_regexes,
                lazy_fetch,
            )? {
                visible.push(entry);
            }
        }
        visible
    } else {
        entries
    };
    // `-O<orderfile>` / `diff.orderfile` then `--rotate-to` / `--skip-to`, the
    // last steps of git's `diffcore_std` (both no-op on an empty diff).
    let mut entries = entries;
    if !entries.is_empty()
        && let Some(orderfile) = resolved_orderfile.as_deref()
    {
        let patterns = commands::diff_order::read_orderfile(orderfile)?;
        commands::diff_order::order_entries(&mut entries, &patterns);
    }
    if !entries.is_empty()
        && let Some(target) = rotate_to.as_deref()
    {
        // Plumbing `git diff` is strict: a `--rotate-to`/`--skip-to` naming no
        // diffed path is fatal (builtin/diff.c sets `rotate_to_strict`).
        commands::diff_order::rotate_entries(&mut entries, target.as_bytes(), rotate_skip, true)?;
    }
    let has_differences = !entries.is_empty();
    warn_diff_rename_limit(rename_limit_diagnostics);
    // `--check`: report whitespace errors introduced by the new side, in place
    // of the normal patch body (git's DIFF_FORMAT_CHECKDIFF). It exits 2 on a
    // whitespace error; combined with `--exit-code`/`--quiet` (not exclusive)
    // the change bit (1) is OR-ed in, matching git's exit codes.
    if check && !name_status && !name_only {
        let resolver =
            WhitespaceRuleResolver::from_git_dir_with_config(&git_dir, repo_config.as_ref())?;
        let check_failed = run_diff_check(
            &entries,
            &db,
            worktree_root.as_deref(),
            use_worktree_old,
            use_worktree_new,
            worktree_clean.as_ref(),
            &resolver,
            lazy_fetch,
        )?;
        let mut code = 0;
        if check_failed {
            code |= 0o2;
        }
        if (quiet || exit_code) && has_differences {
            code |= 0o1;
        }
        if code != 0 {
            return Err(GitError::Exit(code));
        }
        return Ok(());
    }
    let show_patch_for_external = render_selection.patch;
    if allow_external && show_patch_for_external {
        let userdiff_attributes = worktree_root
            .as_deref()
            .map(sley_worktree::StandardAttributeMatcher::from_worktree_root)
            .transpose()?;
        let userdiff = commands::userdiff::UserdiffResolver::with_attributes(
            userdiff_attributes,
            repo_config.clone(),
        );
        let global_external = global_external_diff_command(repo_config.as_ref());
        if let Some(code) = run_external_diff_entries(
            &entries,
            &relative_lookup_entries,
            &db,
            &cwd,
            worktree_root.as_deref(),
            use_worktree_new,
            &userdiff,
            global_external.as_ref(),
            ExternalDiffRunOptions {
                quiet,
                exit_code,
                output: output.as_deref(),
                autocrlf: repo_config
                    .as_ref()
                    .and_then(|config| config.get_bool("core", None, "autocrlf"))
                    .unwrap_or(false),
            },
            lazy_fetch,
        )? {
            if code != 0 {
                return Err(GitError::Exit(code));
            }
            return Ok(());
        }
    }
    if !quiet && !no_patch {
        let mut stdout = Vec::new();
        let show_raw = render_selection.raw;
        let show_numstat = render_selection.numstat;
        let show_stat = render_selection.stat;
        let show_shortstat = render_selection.shortstat;
        let show_patch = render_selection.patch;
        let show_summary = render_selection.summary;
        let stat_entries = if render_selection.needs_line_stats() {
            let mut stat_entries = if ignore_active {
                collect_diff_stat_entries_with_ignore(
                    &entries,
                    &relative_lookup_entries,
                    &db,
                    worktree_root.as_deref(),
                    use_worktree_new,
                    worktree_clean.as_ref(),
                    DiffStatIgnoreOptions {
                        ws_ignore,
                        ignore_blank_lines,
                        ignore_regexes: &ignore_regexes,
                        diff_algorithm,
                        indent_heuristic,
                    },
                    lazy_fetch,
                )?
            } else if !relative_lookup_entries.is_empty() {
                collect_diff_stat_entries_with_lookup(
                    &entries,
                    &relative_lookup_entries,
                    &db,
                    worktree_root.as_deref(),
                    use_worktree_new,
                    worktree_clean.as_ref(),
                    lazy_fetch,
                )?
            } else {
                collect_diff_stat_entries_with_worktree_clean(
                    &entries,
                    &db,
                    worktree_root.as_deref(),
                    use_worktree_new,
                    worktree_clean.as_ref(),
                    lazy_fetch,
                )?
            };
            if diff_rewrite_control {
                apply_diff_break_rewrite_stats(
                    &mut stat_entries,
                    &relative_lookup_entries,
                    &db,
                    worktree_root.as_deref(),
                    use_worktree_new,
                    worktree_clean.as_ref(),
                    lazy_fetch,
                )?;
            }
            Some(stat_entries)
        } else {
            None
        };
        let (zero_all_worktree_oids, index_oids): (bool, HashMap<Vec<u8>, ObjectId>) = if show_raw {
            // git zeroes the worktree-side oid only when it cannot be trusted:
            // a stat-clean file keeps its index oid in raw output. The
            // worktree entries carry the freshly-hashed content oid, so
            // matching it against the index entry reproduces that rule.
            let zero_all_worktree_oids = zero_worktree_oids && plain_index_worktree_diff;
            let needs_index_oids = zero_worktree_oids
                && !zero_all_worktree_oids
                && entries.iter().any(|entry| entry.new_oid.is_some());
            let index_oids = if needs_index_oids {
                let index_path = sley_worktree::repository_index_path(&git_dir);
                match fs::read(&index_path) {
                    Ok(bytes) => Index::parse(&bytes, format)?
                        .entries
                        .into_iter()
                        .map(|entry| (entry.path.to_vec(), entry.oid))
                        .collect(),
                    Err(_) => HashMap::new(),
                }
            } else {
                HashMap::new()
            };
            (zero_all_worktree_oids, index_oids)
        } else {
            (false, HashMap::new())
        };
        let mut resolved_stat_widths = stat_widths;
        if show_stat {
            if let Some(config) = repo_config.as_ref() {
                resolved_stat_widths.resolve_config(config);
            } else {
                resolved_stat_widths.resolve_config_defaults();
            }
        }
        let cached_unmerged_paths = if cached && show_patch {
            diff_unmerged_index_paths(&git_dir, format)?
        } else {
            BTreeSet::new()
        };
        let mut render_dirstat = |stdout: &mut dyn Write| {
            if let Some(dirstat_options) = dirstat
                && !name_only
                && !name_status
            {
                write_diff_dirstat(
                    stdout,
                    &entries,
                    &db,
                    worktree_root.as_deref(),
                    use_worktree_new,
                    worktree_clean.as_ref(),
                    dirstat_options,
                    lazy_fetch,
                )?;
            }
            Ok(())
        };
        if show_patch {
            let combined_unmerged = if plain_index_worktree_diff {
                diff_unmerged_worktree_combined_paths(&git_dir, worktree_root.as_deref(), format)?
            } else {
                BTreeMap::new()
            };
            let mut wrote_combined_unmerged = BTreeSet::new();
            let colors = color_always
                .then(|| commands::diff_words::DiffColors::enabled(repo_config.as_ref()));
            let word_request = word_diff_mode.map(|mode| WordDiffRequest {
                mode,
                cli_regex: word_diff_regex.as_deref(),
            });
            // Userdiff driver resolution (`diff=<driver>` attributes +
            // `diff.<name>.*` config) for hunk headings. Attributes always come
            // from the real worktree, even when the content comparison is
            // `--cached`.
            let userdiff_attributes = worktree_root
                .as_deref()
                .map(sley_worktree::StandardAttributeMatcher::from_worktree_root)
                .transpose()?;
            let userdiff = commands::userdiff::UserdiffResolver::with_attributes(
                userdiff_attributes,
                repo_config.clone(),
            );
            // Whitespace-error highlighting needs the per-path rule, but only
            // when color is on (it does nothing otherwise) and word-diff is
            // off (git suppresses ws-highlight under --word-diff).
            let ws_resolver = (colors.is_some() && word_request.is_none())
                .then(|| {
                    WhitespaceRuleResolver::from_git_dir_with_config(&git_dir, repo_config.as_ref())
                })
                .transpose()?;
            render_diff_entries(
                &mut stdout,
                &entries,
                DiffEntryRenderModes {
                    raw: show_raw,
                    numstat: show_numstat,
                    stat: show_stat,
                    shortstat: show_shortstat,
                    summary: show_summary,
                    patch: true,
                },
                DiffEntryRenderContext {
                    raw: DiffEntryRawRenderOptions {
                        z,
                        abbrev: raw_abbrev,
                        format,
                    },
                    stat: DiffEntryStatRenderOptions {
                        source: stat_entries
                            .as_deref()
                            .map(DiffEntryStatSource::Materialized),
                        z,
                        options: DiffStatOptions {
                            compact_summary,
                            stat_count,
                            color: color_always,
                            quote_path_fully,
                        },
                        widths: Some(resolved_stat_widths),
                        config: None,
                    },
                    after_stat: Some(&mut render_dirstat),
                    prefix_already_written: false,
                },
                |entry| {
                    let lookup_entry = diff_relative_lookup_entry(entry, &relative_lookup_entries);
                    zero_all_worktree_oids
                        || (zero_worktree_oids
                            && lookup_entry.new_oid.as_ref().is_none_or(|oid| {
                                index_oids.get(&lookup_entry.path[..]) != Some(oid)
                            }))
                },
                |stdout, entry| {
                    if cached_unmerged_paths.contains(entry.path.as_bytes()) {
                        let path = status_quote_path(&entry.path, false);
                        writeln!(stdout, "* Unmerged path {path}")?;
                        return Ok(());
                    }
                    if let Some(combined) = combined_unmerged.get(entry.path.as_bytes()) {
                        if wrote_combined_unmerged.insert(entry.path.as_bytes().to_vec()) {
                            write_diff_unmerged_worktree_combined(
                                stdout,
                                &db,
                                combined,
                                patch_abbrev,
                                &src_prefix,
                                &dst_prefix,
                                lazy_fetch,
                            )?;
                        }
                        return Ok(());
                    }
                    let ws_error = match (ws_resolver.as_ref(), ws_error_kinds) {
                        (Some(resolver), Some(kinds)) => {
                            let rule = if kinds.plain {
                                0
                            } else {
                                resolver.rule_for_path(&entry.path)?
                            };
                            Some(sley_diff_merge::render::WsErrorHighlight {
                                rule,
                                old: kinds.old,
                                new: kinds.new,
                                context: kinds.context,
                            })
                        }
                        _ => None,
                    };
                    let lookup_entry = diff_relative_lookup_entry(entry, &relative_lookup_entries);
                    let relative_materialized = !relative_lookup_entries.is_empty();
                    let materialized_contents = if !is_gitlink_pair(lookup_entry)
                        && (relative_materialized || use_worktree_old || worktree_clean.is_some())
                    {
                        Some((
                            diff_entry_old_content_for_diff(
                                lookup_entry,
                                &db,
                                worktree_root.as_deref(),
                                use_worktree_old,
                                worktree_clean.as_ref(),
                                lazy_fetch,
                            )?,
                            diff_entry_new_content(
                                lookup_entry,
                                &db,
                                worktree_root.as_deref(),
                                use_worktree_new,
                                worktree_clean.as_ref(),
                                lazy_fetch,
                            )?,
                        ))
                    } else {
                        None
                    };
                    let no_index_contents = materialized_contents
                        .as_ref()
                        .map(|(old, new)| (old.as_deref(), new.as_deref()));
                    let options = DiffRenderOptions {
                        line_indicators: sley_diff_merge::render::LineIndicators::default(),
                        suppress_blank_empty,
                        binary: patch_binary,
                        db: &db,
                        lazy_fetch,
                        worktree_root: worktree_root.as_deref(),
                        use_worktree_new,
                        format,
                        abbrev: patch_abbrev,
                        src_prefix: &src_prefix,
                        dst_prefix: &dst_prefix,
                        context: patch_context,
                        userdiff: Some(&userdiff),
                        funcname: None,
                        colors: colors.as_ref(),
                        word_diff: word_request.as_ref(),
                        no_index_contents,
                        submodule_format,
                        submodule_dirt: Some(&dirty_submodules),
                        ws_error,
                        color_moved,
                        interhunk: interhunk.unwrap_or(0),
                        ws_ignore,
                        diff_algorithm,
                        ignore_blank_lines,
                        ignore_regexes: &ignore_regexes,
                        line_ranges: None,
                        indent_heuristic,
                        anchors: &anchored,
                        allow_textconv: true,
                    };
                    write_diff_patch_entry(stdout, entry, options)
                },
            )?;
        } else {
            render_diff_entries(
                &mut stdout,
                &entries,
                DiffEntryRenderModes {
                    raw: show_raw,
                    numstat: show_numstat,
                    stat: show_stat,
                    shortstat: show_shortstat,
                    summary: show_summary,
                    patch: false,
                },
                DiffEntryRenderContext {
                    raw: DiffEntryRawRenderOptions {
                        z,
                        abbrev: raw_abbrev,
                        format,
                    },
                    stat: DiffEntryStatRenderOptions {
                        source: stat_entries
                            .as_deref()
                            .map(DiffEntryStatSource::Materialized),
                        z,
                        options: DiffStatOptions {
                            compact_summary,
                            stat_count,
                            color: color_always,
                            quote_path_fully,
                        },
                        widths: Some(resolved_stat_widths),
                        config: None,
                    },
                    after_stat: Some(&mut render_dirstat),
                    prefix_already_written: false,
                },
                |entry| {
                    let lookup_entry = diff_relative_lookup_entry(entry, &relative_lookup_entries);
                    zero_all_worktree_oids
                        || (zero_worktree_oids
                            && lookup_entry.new_oid.as_ref().is_none_or(|oid| {
                                index_oids.get(&lookup_entry.path[..]) != Some(oid)
                            }))
                },
                |_, _| Ok(()),
            )?;
        }
        if !show_patch
            && !show_summary
            && (summary || (!show_stat && !show_shortstat))
            && !show_numstat
            && !show_raw
            && dirstat.is_none()
        {
            for entry in &entries {
                if z && (name_only || name_status) {
                    if name_only {
                        stdout.write_all(&entry.path)?;
                        stdout.write_all(b"\0")?;
                    } else {
                        stdout.write_all(entry.status.label().as_bytes())?;
                        stdout.write_all(b"\0")?;
                        if let Some(old_path) = &entry.old_path {
                            stdout.write_all(old_path)?;
                            stdout.write_all(b"\0")?;
                        }
                        stdout.write_all(&entry.path)?;
                        stdout.write_all(b"\0")?;
                    }
                } else if name_only {
                    let path = status_quote_path(&entry.path, false);
                    writeln!(stdout, "{path}")?;
                } else if !name_status && summary {
                    write_diff_summary_entry(&mut stdout, entry)?;
                } else {
                    write!(stdout, "{}", entry.status.label())?;
                    if let Some(old_path) = &entry.old_path {
                        let old_path = status_quote_path(old_path, false);
                        write!(stdout, "\t{old_path}")?;
                    }
                    let path = status_quote_path(&entry.path, false);
                    writeln!(stdout, "\t{path}")?;
                }
            }
        }
        // `--output=<file>` redirects the formatted diff to a file (git's
        // `diff_opt_output`); otherwise it goes to stdout.
        let mut file_output;
        let mut std_output;
        let output: &mut dyn Write = if let Some(path) = output.as_deref() {
            file_output = fs::File::create(path).map_err(|err| {
                eprintln!("fatal: cannot open '{path}': {err}");
                GitError::Exit(128)
            })?;
            &mut file_output
        } else {
            std_output = io::stdout();
            &mut std_output
        };
        if let Some(prefix) = line_prefix.as_deref() {
            write_line_prefixed(output, &stdout, prefix.as_bytes())?;
        } else {
            output.write_all(&stdout)?;
        }
    }
    if (quiet || exit_code) && has_differences {
        return Err(GitError::Exit(1));
    }
    Ok(())
}

fn reject_duplicate_tree_for_index_diff(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tree: &ObjectId,
) -> Result<()> {
    if sley_diff_merge::tree_has_duplicate_leaf_paths(db, format, tree)? {
        return Err(sley_diff_merge::corrupted_cache_tree_error());
    }
    Ok(())
}

struct CombinedDiffOptions<'a> {
    dense: bool,
    output_format: sley_rev::diff_options::DiffOutputFormat,
    patch_abbrev: usize,
    raw_abbrev: Option<usize>,
    z: bool,
    src_prefix: &'a str,
    dst_prefix: &'a str,
    context: usize,
    ws_ignore: sley_diff_merge::WsIgnore,
    diff_algorithm: sley_diff_merge::DiffAlgorithm,
    line_prefix: Option<&'a str>,
    /// `-O<orderfile>` / `diff.orderfile`: reorder the combined paths by the
    /// orderfile patterns (`diffcore_order` runs for combined diffs too).
    orderfile: Option<&'a str>,
    lazy_fetch: bool,
}

fn write_diff_combined_three_tree(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    trees: &[ObjectId],
    pathspec: &DiffPathspec,
    options: CombinedDiffOptions<'_>,
) -> Result<bool> {
    let parent_trees = [trees[1], trees[2]];
    let mut paths = commands::combined::combined_paths(db, format, &trees[0], &parent_trees)?;
    paths.retain(|path| pathspec.matches(&path.path));
    if paths.is_empty() {
        return Ok(false);
    }
    if let Some(orderfile) = options.orderfile {
        let patterns = commands::diff_order::read_orderfile(orderfile)?;
        commands::diff_order::order_by_path(&mut paths, &patterns, |path| &path.path);
    }

    let name_status = options
        .output_format
        .contains(sley_rev::diff_options::DiffOutputFormat::NAME_STATUS);
    let name_only = options
        .output_format
        .contains(sley_rev::diff_options::DiffOutputFormat::NAME_ONLY);
    let raw = options
        .output_format
        .contains(sley_rev::diff_options::DiffOutputFormat::RAW);
    let patch = options
        .output_format
        .contains(sley_rev::diff_options::DiffOutputFormat::PATCH);
    let no_output = options
        .output_format
        .contains(sley_rev::diff_options::DiffOutputFormat::NO_OUTPUT);
    let selection = sley_diff_merge::porcelain::select_render_formats(
        sley_diff_merge::porcelain::RenderSelectionOptions {
            default_output: sley_diff_merge::porcelain::DefaultDiffOutput::Patch,
            raw,
            patch,
            name_status,
            name_only,
            stat: false,
            numstat: false,
            shortstat: false,
            summary: false,
            auxiliary_format: false,
            suppress_output: no_output,
        },
    );
    let show_patch = selection.patch;

    let render_ctx = commands::combined::CombinedRenderCtx {
        db,
        format,
        dense: options.dense,
        all_paths: false,
        context: options.context,
        ws_ignore: options.ws_ignore,
        diff_algorithm: options.diff_algorithm,
        src_prefix: options.src_prefix,
        dst_prefix: options.dst_prefix,
        patch_abbrev: options.patch_abbrev,
        raw_abbrev: options.raw_abbrev,
        lazy_fetch: options.lazy_fetch,
    };
    let mut out = Vec::new();
    if selection.raw {
        for path in &paths {
            commands::combined::write_combined_raw(&mut out, &render_ctx, path, options.z)?;
        }
    }
    if selection.name_status {
        for path in &paths {
            commands::combined::write_combined_name_status(&mut out, path, options.z)?;
        }
    }
    if selection.name_only {
        for path in &paths {
            if options.z {
                out.write_all(&path.path)?;
                out.write_all(b"\0")?;
            } else {
                writeln!(out, "{}", status_quote_path(&path.path, false))?;
            }
        }
    }
    if show_patch {
        if selection.separates_patch_from_prefix() {
            writeln!(out)?;
        }
        for path in &paths {
            commands::combined::write_combined_patch(&mut out, &render_ctx, path)?;
        }
    }

    let mut stdout = io::stdout();
    if let Some(prefix) = options.line_prefix {
        write_line_prefixed(&mut stdout, &out, prefix.as_bytes())?;
    } else {
        stdout.write_all(&out)?;
    }
    Ok(!out.is_empty())
}

fn diff_unmerged_index_paths(git_dir: &Path, format: ObjectFormat) -> Result<BTreeSet<Vec<u8>>> {
    let index_path = sley_worktree::repository_index_path(git_dir);
    if !index_path.exists() {
        return Ok(BTreeSet::new());
    }
    let index = Index::parse(&fs::read(index_path)?, format)?;
    Ok(index
        .entries
        .into_iter()
        .filter(|entry| entry.stage() != sley_index::Stage::Normal)
        .map(|entry| entry.path.into_bytes())
        .collect())
}

struct UnmergedWorktreeCombinedPath {
    path: Vec<u8>,
    ours: ObjectId,
    theirs: ObjectId,
    worktree: Vec<u8>,
}

fn diff_unmerged_worktree_combined_paths(
    git_dir: &Path,
    worktree_root: Option<&Path>,
    format: ObjectFormat,
) -> Result<BTreeMap<Vec<u8>, UnmergedWorktreeCombinedPath>> {
    let Some(worktree_root) = worktree_root else {
        return Ok(BTreeMap::new());
    };
    let index_path = sley_worktree::repository_index_path(git_dir);
    if !index_path.exists() {
        return Ok(BTreeMap::new());
    }
    let index = Index::parse(&fs::read(index_path)?, format)?;
    let mut stages: BTreeMap<Vec<u8>, (Option<ObjectId>, Option<ObjectId>)> = BTreeMap::new();
    for entry in index.entries {
        let slot = stages
            .entry(entry.path.as_bytes().to_vec())
            .or_insert((None, None));
        match entry.stage() {
            sley_index::Stage::Ours => slot.0 = Some(entry.oid),
            sley_index::Stage::Theirs => slot.1 = Some(entry.oid),
            _ => {}
        }
    }
    let mut out = BTreeMap::new();
    for (path, (ours, theirs)) in stages {
        let (Some(ours), Some(theirs)) = (ours, theirs) else {
            continue;
        };
        let worktree_path = worktree_root.join(repo_path_to_path(&path));
        let Ok(worktree) = fs::read(worktree_path) else {
            continue;
        };
        out.insert(
            path.clone(),
            UnmergedWorktreeCombinedPath {
                path,
                ours,
                theirs,
                worktree,
            },
        );
    }
    Ok(out)
}

fn write_diff_unmerged_worktree_combined(
    stdout: &mut dyn Write,
    db: &FileObjectDatabase,
    path: &UnmergedWorktreeCombinedPath,
    abbrev: usize,
    src_prefix: &str,
    dst_prefix: &str,
    lazy_fetch: bool,
) -> Result<()> {
    let ours = read_blob(db, &path.ours, lazy_fetch)?;
    let theirs = read_blob(db, &path.theirs, lazy_fetch)?;
    let ours_lines = diff_split_lines(&ours);
    let theirs_lines = diff_split_lines(&theirs);
    let worktree_lines = diff_split_lines(&path.worktree);
    let ours_abbrev = diff_abbrev_oid(&path.ours, abbrev);
    let theirs_abbrev = diff_abbrev_oid(&path.theirs, abbrev);
    let quoted = status_quote_path(&path.path, false);
    writeln!(stdout, "diff --cc {quoted}")?;
    writeln!(stdout, "index {ours_abbrev},{theirs_abbrev}..0000000")?;
    writeln!(stdout, "--- {src_prefix}{quoted}")?;
    writeln!(stdout, "+++ {dst_prefix}{quoted}")?;
    writeln!(
        stdout,
        "@@@ -1,{} -1,{} +1,{} @@@",
        ours_lines.len().max(1),
        theirs_lines.len().max(1),
        worktree_lines.len().max(1)
    )?;
    for line in worktree_lines {
        let in_ours = ours_lines.contains(&line);
        let in_theirs = theirs_lines.contains(&line);
        let prefix = match (in_ours, in_theirs) {
            (true, true) => b"  ".as_slice(),
            (true, false) => b" +".as_slice(),
            (false, true) => b"+ ".as_slice(),
            (false, false) => b"++".as_slice(),
        };
        stdout.write_all(prefix)?;
        stdout.write_all(line)?;
        if !line.ends_with(b"\n") {
            stdout.write_all(b"\n")?;
        }
    }
    Ok(())
}

fn diff_split_lines(bytes: &[u8]) -> Vec<&[u8]> {
    if bytes.is_empty() {
        Vec::new()
    } else {
        bytes.split_inclusive(|byte| *byte == b'\n').collect()
    }
}

fn diff_abbrev_oid(oid: &ObjectId, width: usize) -> String {
    oid.to_hex().chars().take(width).collect()
}

fn write_line_prefixed(stdout: &mut dyn Write, bytes: &[u8], prefix: &[u8]) -> Result<()> {
    if bytes.is_empty() {
        return Ok(());
    }
    let mut at_line_start = true;
    for line in bytes.split_inclusive(|byte| *byte == b'\n') {
        if at_line_start {
            stdout.write_all(prefix)?;
        }
        stdout.write_all(line)?;
        at_line_start = line.ends_with(b"\n");
    }
    Ok(())
}

fn git_config_color_is_always(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "always" | "true" | "yes" | "on" | "1"
    )
}

#[derive(Clone, Copy)]
struct WsErrorHighlightKinds {
    old: bool,
    new: bool,
    context: bool,
    plain: bool,
}

fn parse_ws_error_highlight_kinds(value: Option<&str>) -> Option<WsErrorHighlightKinds> {
    let mut kinds = WsErrorHighlightKinds {
        old: false,
        new: true,
        context: false,
        plain: false,
    };
    for mode in value.unwrap_or("default").split(',') {
        match mode {
            "" | "default" => {
                kinds = WsErrorHighlightKinds {
                    old: false,
                    new: true,
                    context: false,
                    plain: false,
                };
            }
            "old" => kinds.old = true,
            "new" => kinds.new = true,
            "context" => kinds.context = true,
            "all" => {
                kinds = WsErrorHighlightKinds {
                    old: true,
                    new: true,
                    context: true,
                    plain: false,
                };
            }
            "none" => {
                kinds = WsErrorHighlightKinds {
                    old: true,
                    new: true,
                    context: true,
                    plain: true,
                };
            }
            _ => {}
        }
    }
    (kinds.old || kinds.new || kinds.context || kinds.plain).then_some(kinds)
}

pub(crate) fn apply_diff_pickaxe(
    entries: Vec<sley_diff_merge::NameStatusEntry>,
    needle: &[u8],
    pickaxe_all: bool,
    db: &FileObjectDatabase,
    worktree_root: Option<&Path>,
    use_worktree_new: bool,
    worktree_clean: Option<&DiffWorktreeCleanContext<'_>>,
    lazy_fetch: bool,
) -> Result<Vec<sley_diff_merge::NameStatusEntry>> {
    if needle.is_empty() {
        return Ok(Vec::new());
    }
    if pickaxe_all {
        for entry in &entries {
            if diff_entry_matches_pickaxe(
                entry,
                needle,
                db,
                worktree_root,
                use_worktree_new,
                worktree_clean,
                lazy_fetch,
            )? {
                return Ok(sort_diff_entries_by_path(entries));
            }
        }
        return Ok(Vec::new());
    }

    let mut matches = Vec::new();
    for entry in &entries {
        if diff_entry_matches_pickaxe(
            entry,
            needle,
            db,
            worktree_root,
            use_worktree_new,
            worktree_clean,
            lazy_fetch,
        )? {
            matches.push(entry.clone());
        }
    }
    Ok(sort_diff_entries_by_path(matches))
}

fn diff_entry_matches_pickaxe(
    entry: &sley_diff_merge::NameStatusEntry,
    needle: &[u8],
    db: &FileObjectDatabase,
    worktree_root: Option<&Path>,
    use_worktree_new: bool,
    worktree_clean: Option<&DiffWorktreeCleanContext<'_>>,
    lazy_fetch: bool,
) -> Result<bool> {
    let old_content = diff_entry_old_content(entry, db, lazy_fetch)?;
    let new_content = diff_entry_new_content(
        entry,
        db,
        worktree_root,
        use_worktree_new,
        worktree_clean,
        lazy_fetch,
    )?;
    Ok(
        count_non_overlapping_occurrences(old_content.as_deref().unwrap_or_default(), needle)
            != count_non_overlapping_occurrences(
                new_content.as_deref().unwrap_or_default(),
                needle,
            ),
    )
}

pub(crate) fn resolve_diff_find_objects(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    values: &[String],
) -> Result<Vec<ObjectId>> {
    values
        .iter()
        .map(|value| resolve_diff_find_object(git_dir, format, db, value))
        .collect()
}

fn resolve_diff_find_object(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    value: &str,
) -> Result<ObjectId> {
    if value.len() == format.hex_len() && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return ObjectId::from_hex(format, value)
            .map_err(|_| diff_find_object_unable_to_resolve_error(value));
    }
    sley_rev::RevisionResolver::new(git_dir, format, db)
        .resolve(value)
        .map_err(|_| diff_find_object_unable_to_resolve_error(value))
}

pub(crate) fn apply_diff_find_objects(
    entries: Vec<sley_diff_merge::NameStatusEntry>,
    targets: &[ObjectId],
) -> Vec<sley_diff_merge::NameStatusEntry> {
    if targets.is_empty() {
        return entries;
    }
    sort_diff_entries_by_path(
        entries
            .into_iter()
            .filter(|entry| {
                targets.iter().any(|target| {
                    entry.old_oid.as_ref() == Some(target) || entry.new_oid.as_ref() == Some(target)
                })
            })
            .collect(),
    )
}

fn count_non_overlapping_occurrences(haystack: &[u8], needle: &[u8]) -> usize {
    if needle.is_empty() {
        return 0;
    }
    let mut count = 0;
    let mut offset = 0;
    while offset + needle.len() <= haystack.len() {
        if &haystack[offset..offset + needle.len()] == needle {
            count += 1;
            offset += needle.len();
        } else {
            offset += 1;
        }
    }
    count
}

fn sort_diff_entries_by_path(
    mut entries: Vec<sley_diff_merge::NameStatusEntry>,
) -> Vec<sley_diff_merge::NameStatusEntry> {
    entries.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.old_path.cmp(&right.old_path))
            .then_with(|| left.status.code().cmp(&right.status.code()))
    });
    entries
}

fn diff_find_object_unable_to_resolve_error(value: &str) -> GitError {
    eprintln!("error: unable to resolve '{value}'");
    GitError::Exit(129)
}

fn diff_find_object_pickaxe_all_conflict_error() -> Result<()> {
    eprintln!(
        "fatal: options '--pickaxe-all' and '--find-object' cannot be used together, use '--pickaxe-all' with '-G' and '-S'"
    );
    Err(GitError::Exit(128))
}

fn diff_relative_prefix(
    cli_session: &crate::session::CliSession,
    mode: &sley_rev::diff_options::DiffRelativeMode,
    cwd: &Path,
    git_dir: &Path,
) -> Result<Vec<u8>> {
    match mode {
        sley_rev::diff_options::DiffRelativeMode::Off => Ok(Vec::new()),
        sley_rev::diff_options::DiffRelativeMode::Cwd => {
            Ok(worktree_prefix(cli_session, cwd, git_dir)?
                .trim_end_matches('/')
                .as_bytes()
                .to_vec())
        }
        sley_rev::diff_options::DiffRelativeMode::Prefix(prefix) => {
            Ok(diff_relative_prefix_arg(prefix).into_bytes())
        }
    }
}

fn diff_relative_prefix_arg(prefix: &str) -> String {
    if prefix.starts_with('/') {
        prefix.to_string()
    } else {
        prefix.trim_end_matches('/').to_string()
    }
}

type DiffRelativeLookupKey = (Vec<u8>, Option<Vec<u8>>, String);
type DiffRelativeLookupMap = BTreeMap<DiffRelativeLookupKey, sley_diff_merge::NameStatusEntry>;

fn apply_diff_relative(
    entries: Vec<sley_diff_merge::NameStatusEntry>,
    prefix: &[u8],
) -> (Vec<sley_diff_merge::NameStatusEntry>, DiffRelativeLookupMap) {
    if prefix.is_empty() {
        return (entries, DiffRelativeLookupMap::new());
    }
    let mut filtered = Vec::new();
    let mut lookup_entries = DiffRelativeLookupMap::new();
    for entry in entries {
        if let Some(old_path) = &entry.old_path {
            let old_display = diff_relative_display_path(old_path, prefix);
            let new_display = diff_relative_display_path(&entry.path, prefix);
            if matches!(entry.status, sley_diff_merge::NameStatus::Copied(_)) {
                match (old_display, new_display) {
                    (Some(old_path), Some(path)) => {
                        let lookup_entry = entry.clone();
                        let display_entry = sley_diff_merge::NameStatusEntry {
                            path: BString::from(path),
                            old_path: Some(BString::from(old_path)),
                            ..entry
                        };
                        diff_relative_push_entry(
                            &mut filtered,
                            &mut lookup_entries,
                            display_entry,
                            lookup_entry,
                        );
                    }
                    (None, Some(path)) => {
                        let display_entry = sley_diff_merge::NameStatusEntry {
                            status: sley_diff_merge::NameStatus::Added,
                            path: BString::from(path),
                            old_path: None,
                            old_mode: None,
                            new_mode: entry.new_mode,
                            old_oid: None,
                            new_oid: entry.new_oid,
                        };
                        let lookup_entry = sley_diff_merge::NameStatusEntry {
                            path: entry.path,
                            ..display_entry.clone()
                        };
                        diff_relative_push_entry(
                            &mut filtered,
                            &mut lookup_entries,
                            display_entry,
                            lookup_entry,
                        );
                    }
                    (Some(_), None) | (None, None) => {}
                }
            } else {
                match (old_display, new_display) {
                    (Some(old_path), Some(path)) => {
                        let lookup_entry = entry.clone();
                        let display_entry = sley_diff_merge::NameStatusEntry {
                            path: BString::from(path),
                            old_path: Some(BString::from(old_path)),
                            ..entry
                        };
                        diff_relative_push_entry(
                            &mut filtered,
                            &mut lookup_entries,
                            display_entry,
                            lookup_entry,
                        );
                    }
                    (Some(path), None) => {
                        let display_entry = sley_diff_merge::NameStatusEntry {
                            status: sley_diff_merge::NameStatus::Deleted,
                            path: BString::from(path),
                            old_path: None,
                            old_mode: entry.old_mode,
                            new_mode: None,
                            old_oid: entry.old_oid,
                            new_oid: None,
                        };
                        let lookup_entry = sley_diff_merge::NameStatusEntry {
                            path: old_path.clone(),
                            ..display_entry.clone()
                        };
                        diff_relative_push_entry(
                            &mut filtered,
                            &mut lookup_entries,
                            display_entry,
                            lookup_entry,
                        );
                    }
                    (None, Some(path)) => {
                        let display_entry = sley_diff_merge::NameStatusEntry {
                            status: sley_diff_merge::NameStatus::Added,
                            path: BString::from(path),
                            old_path: None,
                            old_mode: None,
                            new_mode: entry.new_mode,
                            old_oid: None,
                            new_oid: entry.new_oid,
                        };
                        let lookup_entry = sley_diff_merge::NameStatusEntry {
                            path: entry.path,
                            ..display_entry.clone()
                        };
                        diff_relative_push_entry(
                            &mut filtered,
                            &mut lookup_entries,
                            display_entry,
                            lookup_entry,
                        );
                    }
                    (None, None) => {}
                }
            }
        } else if let Some(path) = diff_relative_display_path(&entry.path, prefix) {
            let lookup_entry = entry.clone();
            let display_entry = sley_diff_merge::NameStatusEntry {
                path: BString::from(path),
                ..entry
            };
            diff_relative_push_entry(
                &mut filtered,
                &mut lookup_entries,
                display_entry,
                lookup_entry,
            );
        }
    }
    filtered.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.old_path.cmp(&right.old_path))
            .then_with(|| left.status.code().cmp(&right.status.code()))
    });
    (filtered, lookup_entries)
}

fn diff_relative_push_entry(
    filtered: &mut Vec<sley_diff_merge::NameStatusEntry>,
    lookup_entries: &mut DiffRelativeLookupMap,
    display_entry: sley_diff_merge::NameStatusEntry,
    lookup_entry: sley_diff_merge::NameStatusEntry,
) {
    lookup_entries.insert(diff_relative_lookup_key(&display_entry), lookup_entry);
    filtered.push(display_entry);
}

fn diff_relative_lookup_entry<'a>(
    entry: &'a sley_diff_merge::NameStatusEntry,
    lookup_entries: &'a DiffRelativeLookupMap,
) -> &'a sley_diff_merge::NameStatusEntry {
    lookup_entries
        .get(&diff_relative_lookup_key(entry))
        .unwrap_or(entry)
}

fn diff_relative_lookup_key(entry: &sley_diff_merge::NameStatusEntry) -> DiffRelativeLookupKey {
    (
        entry.path.as_bytes().to_vec(),
        entry.old_path.as_ref().map(|path| path.as_bytes().to_vec()),
        entry.status.label(),
    )
}

struct DiffStatIgnoreOptions<'a> {
    ws_ignore: sley_diff_merge::WsIgnore,
    ignore_blank_lines: bool,
    ignore_regexes: &'a [sley_grep::Regex],
    diff_algorithm: sley_diff_merge::DiffAlgorithm,
    indent_heuristic: bool,
}

fn diff_relative_display_path(path: &[u8], prefix: &[u8]) -> Option<Vec<u8>> {
    if prefix.is_empty() {
        return Some(path.to_vec());
    }
    if path == prefix {
        return Some(Vec::new());
    }
    // git matches the prefix as a plain string (`--relative=sub` turns
    // `subdir/file2` into `dir/file2`), swallowing one separating slash when
    // the prefix happens to end on a path-component boundary.
    path.strip_prefix(prefix)
        .map(|rest| rest.strip_prefix(b"/").unwrap_or(rest).to_vec())
}

fn collect_diff_stat_entries_with_ignore<'a>(
    entries: &'a [sley_diff_merge::NameStatusEntry],
    lookup_entries: &DiffRelativeLookupMap,
    db: &FileObjectDatabase,
    worktree_root: Option<&Path>,
    use_worktree_new: bool,
    worktree_clean: Option<&DiffWorktreeCleanContext<'_>>,
    ignore: DiffStatIgnoreOptions<'_>,
    lazy_fetch: bool,
) -> Result<Vec<DiffStatEntryData<'a>>> {
    let mut stat_entries = Vec::with_capacity(entries.len());
    for entry in entries {
        let lookup_entry = diff_relative_lookup_entry(entry, lookup_entries);
        let old_content = diff_entry_old_content(lookup_entry, db, lazy_fetch)?;
        let new_content = diff_entry_new_content(
            lookup_entry,
            db,
            worktree_root,
            use_worktree_new,
            worktree_clean,
            lazy_fetch,
        )?;
        let stats = if old_content.as_deref().is_some_and(is_binary_content)
            || new_content.as_deref().is_some_and(is_binary_content)
        {
            diff_line_stats(old_content.as_deref(), new_content.as_deref())
        } else {
            diff_line_stats_from_ignored_hunks(
                old_content.as_deref(),
                new_content.as_deref(),
                ignore.ws_ignore,
                ignore.ignore_blank_lines,
                ignore.ignore_regexes,
                ignore.diff_algorithm,
                ignore.indent_heuristic,
            )
        };
        stat_entries.push(DiffStatEntryData { entry, stats });
    }
    Ok(stat_entries)
}

fn collect_diff_stat_entries_with_lookup<'a>(
    entries: &'a [sley_diff_merge::NameStatusEntry],
    lookup_entries: &DiffRelativeLookupMap,
    db: &FileObjectDatabase,
    worktree_root: Option<&Path>,
    use_worktree_new: bool,
    worktree_clean: Option<&DiffWorktreeCleanContext<'_>>,
    lazy_fetch: bool,
) -> Result<Vec<DiffStatEntryData<'a>>> {
    let mut stat_entries = Vec::with_capacity(entries.len());
    for entry in entries {
        let lookup_entry = diff_relative_lookup_entry(entry, lookup_entries);
        let old_content = diff_entry_old_content(lookup_entry, db, lazy_fetch)?;
        let new_content = diff_entry_new_content(
            lookup_entry,
            db,
            worktree_root,
            use_worktree_new,
            worktree_clean,
            lazy_fetch,
        )?;
        let stats = diff_line_stats(old_content.as_deref(), new_content.as_deref());
        stat_entries.push(DiffStatEntryData { entry, stats });
    }
    Ok(stat_entries)
}

fn apply_diff_break_rewrite_stats(
    entries: &mut [DiffStatEntryData<'_>],
    lookup_entries: &DiffRelativeLookupMap,
    db: &FileObjectDatabase,
    worktree_root: Option<&Path>,
    use_worktree_new: bool,
    worktree_clean: Option<&DiffWorktreeCleanContext<'_>>,
    lazy_fetch: bool,
) -> Result<()> {
    for data in entries {
        if !matches!(data.entry.status, sley_diff_merge::NameStatus::Modified) {
            continue;
        }
        let lookup_entry = diff_relative_lookup_entry(data.entry, lookup_entries);
        let old_content = diff_entry_old_content(lookup_entry, db, lazy_fetch)?;
        let new_content = diff_entry_new_content(
            lookup_entry,
            db,
            worktree_root,
            use_worktree_new,
            worktree_clean,
            lazy_fetch,
        )?;
        let (Some(old), Some(new)) = (old_content.as_deref(), new_content.as_deref()) else {
            continue;
        };
        if sley_diff_merge::blob_similarity(old, new) >= 50 {
            continue;
        }
        let deletes = diff_line_stats(Some(old), None);
        let inserts = diff_line_stats(None, Some(new));
        if let (DiffLineStats::Text { deleted, .. }, DiffLineStats::Text { inserted, .. }) =
            (deletes, inserts)
        {
            data.stats = DiffLineStats::Text { inserted, deleted };
        }
    }
    Ok(())
}

fn diff_line_stats_from_ignored_hunks(
    old_content: Option<&[u8]>,
    new_content: Option<&[u8]>,
    ws_ignore: sley_diff_merge::WsIgnore,
    ignore_blank_lines: bool,
    ignore_regexes: &[sley_grep::Regex],
    diff_algorithm: sley_diff_merge::DiffAlgorithm,
    indent_heuristic: bool,
) -> DiffLineStats {
    let regex_match = (!ignore_regexes.is_empty()).then_some(move |line: &[u8]| {
        ignore_regexes
            .iter()
            .any(|re| re.is_match_with_case(line, false))
    });
    let change_ignore = (ignore_blank_lines || !ignore_regexes.is_empty()).then(|| {
        sley_diff_merge::render::ChangeIgnore {
            ignore_blank_lines,
            regex_match: regex_match.as_ref().map(|f| f as &dyn Fn(&[u8]) -> bool),
        }
    });
    let mut render_options = sley_diff_merge::render::HunkRenderOptions {
        context: 0,
        interhunk: 0,
        ws_ignore,
        algorithm: diff_algorithm,
        indent_heuristic,
        change_ignore: change_ignore.as_ref(),
        ..Default::default()
    };
    let mut hunks = Vec::new();
    sley_diff_merge::render::render_hunks(
        &mut hunks,
        old_content,
        new_content,
        &mut render_options,
    );
    let mut inserted = 0;
    let mut deleted = 0;
    for line in hunks.split_inclusive(|byte| *byte == b'\n') {
        match line.first() {
            Some(b'+') => inserted += 1,
            Some(b'-') => deleted += 1,
            _ => {}
        }
    }
    DiffLineStats::Text { inserted, deleted }
}

/// Parameters for `git diff --no-index`.
struct DiffNoIndexParams<'a> {
    context: usize,
    color: bool,
    color_moved_cli: Option<Option<sley_diff_merge::render::ColorMovedMode>>,
    color_moved_ws_cli: Option<sley_diff_merge::render::ColorMovedWs>,
    output_format: sley_rev::diff_options::DiffOutputFormat,
    raw_abbrev: Option<Option<usize>>,
    patch_abbrev: Option<usize>,
    patch_full_index: bool,
    patch_binary: bool,
    allow_external: bool,
    exit_code: bool,
    output: Option<&'a str>,
    reverse: bool,
    z: bool,
    word_diff_mode: Option<commands::diff_words::WordDiffMode>,
    word_diff_regex: Option<&'a str>,
    src_prefix: &'a str,
    dst_prefix: &'a str,
    cli_no_prefix: bool,
    cli_default_prefix: bool,
    cli_src_prefix: Option<&'a str>,
    cli_dst_prefix: Option<&'a str>,
    quiet: bool,
    interhunk: usize,
    ws_ignore: sley_diff_merge::WsIgnore,
    diff_algorithm: sley_diff_merge::DiffAlgorithm,
    ignore_blank_lines: bool,
    ignore_regexes: &'a [sley_grep::Regex],
    indent_heuristic: bool,
    anchored: &'a [Vec<u8>],
    lazy_fetch: bool,
}

struct NoIndexSide {
    path: Vec<u8>,
    content: Vec<u8>,
    mode: u32,
    oid: Option<ObjectId>,
}

struct NoIndexEntry {
    entry: sley_diff_merge::NameStatusEntry,
    old_content: Option<Vec<u8>>,
    new_content: Option<Vec<u8>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NoIndexPathKind {
    Stdin,
    Null,
    Directory,
    File,
    Fifo,
    Other,
}

/// `git diff --no-index <path> <path>`: compare two files outside (or beside)
/// the object database. Attributes and `diff.*` config still apply when the
/// command runs inside a repository. Exits 1 when the files differ.
fn cmd_diff_no_index(
    cwd: &Path,
    paths: &[String],
    params: DiffNoIndexParams<'_>,
    repository: Option<&RepositoryContext>,
) -> Result<()> {
    if paths.len() < 2 {
        eprintln!("usage: git diff --no-index [<options>] <path> <path>");
        return Err(GitError::Exit(129));
    }
    let format = repository
        .map(RepositoryContext::format)
        .unwrap_or(ObjectFormat::Sha1);
    let mut entries = no_index_entries(&paths[0], &paths[1], &paths[2..], format)?;
    if params.reverse {
        entries = reverse_no_index_entries(entries);
    }
    if entries.is_empty() {
        return Ok(());
    }
    // Repository context is optional: when present, .gitattributes drivers,
    // diff.<name>.* config, and color overrides all apply.
    let config = repository.map(|repository| repository.config().clone());
    let (mut src_prefix, mut dst_prefix) = no_index_resolve_prefixes(config.as_ref(), &params);
    if params.reverse {
        std::mem::swap(&mut src_prefix, &mut dst_prefix);
    }
    let worktree_root = repository.and_then(|repository| repository.worktree_root().ok());
    let colors = params
        .color
        .then(|| commands::diff_words::DiffColors::enabled(config.as_ref()));
    let word_request = params.word_diff_mode.map(|mode| WordDiffRequest {
        mode,
        cli_regex: params.word_diff_regex,
    });
    let color_moved_mode = match params.color_moved_cli {
        Some(mode) => mode,
        None => match config
            .as_ref()
            .and_then(|config| config.get("diff", None, "colormoved").map(str::to_owned))
        {
            Some(value) => sley_rev::diff_options::parse_color_moved_mode(&value)?,
            None => None,
        },
    };
    let color_moved_ws = match params.color_moved_ws_cli {
        Some(ws) => ws,
        None => match config
            .as_ref()
            .and_then(|config| config.get("diff", None, "colormovedws").map(str::to_owned))
        {
            Some(value) => sley_rev::diff_options::parse_color_moved_ws(&value)?,
            None => sley_diff_merge::render::ColorMovedWs::default(),
        },
    };
    let color_moved = color_moved_mode.map(|mode| sley_diff_merge::render::ColorMoved {
        mode,
        ws: color_moved_ws,
    });
    // A throwaway object database handle: content reads are overridden, so it
    // is never consulted.
    let scratch_db;
    let db = if let Some(repository) = repository {
        repository.objects()
    } else {
        // Outside a repository the patch renderer still requires an ODB
        // service, although no-index provides every content side explicitly.
        scratch_db = FileObjectDatabase::from_git_dir(cwd, format);
        &scratch_db
    };
    let raw = params
        .output_format
        .contains(sley_rev::diff_options::DiffOutputFormat::RAW);
    let name_status = params
        .output_format
        .contains(sley_rev::diff_options::DiffOutputFormat::NAME_STATUS);
    let name_only = params
        .output_format
        .contains(sley_rev::diff_options::DiffOutputFormat::NAME_ONLY);
    let numstat = params
        .output_format
        .contains(sley_rev::diff_options::DiffOutputFormat::NUMSTAT);
    let patch = params
        .output_format
        .contains(sley_rev::diff_options::DiffOutputFormat::PATCH);
    let no_output = params
        .output_format
        .contains(sley_rev::diff_options::DiffOutputFormat::NO_OUTPUT);
    let selection = sley_diff_merge::porcelain::select_render_formats(
        sley_diff_merge::porcelain::RenderSelectionOptions {
            default_output: sley_diff_merge::porcelain::DefaultDiffOutput::Patch,
            raw,
            patch,
            name_status,
            name_only,
            stat: false,
            numstat,
            shortstat: false,
            summary: false,
            auxiliary_format: false,
            suppress_output: no_output,
        },
    );
    let userdiff_attributes = worktree_root
        .as_ref()
        .map(|root| sley_worktree::StandardAttributeMatcher::from_worktree_root(root))
        .transpose()?;
    let userdiff =
        commands::userdiff::UserdiffResolver::with_attributes(userdiff_attributes, config.clone());
    let show_patch_for_external = selection.patch;
    if params.allow_external && show_patch_for_external {
        let global_external = global_external_diff_command(config.as_ref());
        if let Some(code) = run_external_diff_no_index_entries(
            &entries,
            &userdiff,
            global_external.as_ref(),
            ExternalDiffRunOptions {
                quiet: params.quiet,
                // `diff --no-index` reports differences with status 1 even
                // without an explicit `--exit-code`.
                exit_code: true,
                output: params.output,
                autocrlf: config
                    .as_ref()
                    .and_then(|config| config.get_bool("core", None, "autocrlf"))
                    .unwrap_or(false),
            },
        )? {
            if code != 0 {
                return Err(GitError::Exit(code));
            }
            return Ok(());
        }
    }
    if !params.quiet {
        let mut stdout = io::stdout();
        let raw_abbrev = match params.raw_abbrev {
            Some(width) => width.map(|width| width.min(format.hex_len())),
            None => Some(7),
        };
        let repository_abbrev = repository.map(RepositoryContext::abbrev).transpose()?;
        let patch_abbrev = if params.patch_full_index {
            format.hex_len()
        } else if let Some(width) = params.patch_abbrev {
            width.min(format.hex_len())
        } else {
            match repository_abbrev {
                Some(Some(width)) => width.min(format.hex_len()),
                Some(None) => format.hex_len(),
                None => 7.min(format.hex_len()),
            }
        };
        if selection.raw {
            for entry in &entries {
                write_diff_raw_entry(
                    &mut stdout,
                    &entry.entry,
                    params.z,
                    true,
                    raw_abbrev,
                    format,
                )?;
            }
        }
        if selection.name_status {
            for entry in &entries {
                if params.z {
                    stdout.write_all(entry.entry.status.label().as_bytes())?;
                    stdout.write_all(b"\0")?;
                    stdout.write_all(&entry.entry.path)?;
                    stdout.write_all(b"\0")?;
                } else {
                    writeln!(
                        stdout,
                        "{}\t{}",
                        entry.entry.status.label(),
                        status_quote_path(&entry.entry.path, false)
                    )?;
                }
            }
        }
        if selection.name_only {
            for entry in &entries {
                if params.z {
                    stdout.write_all(&entry.entry.path)?;
                    stdout.write_all(b"\0")?;
                } else {
                    writeln!(stdout, "{}", status_quote_path(&entry.entry.path, false))?;
                }
            }
        }
        if selection.numstat {
            for entry in &entries {
                let stats =
                    diff_line_stats(entry.old_content.as_deref(), entry.new_content.as_deref());
                write_diff_numstat_materialized_entry(&mut stdout, &entry.entry, stats, params.z)?;
            }
        }
        if selection.raw
            || selection.name_status
            || selection.name_only
            || selection.numstat
            || no_output
        {
            return Err(GitError::Exit(1));
        }
        if !selection.patch {
            return Err(GitError::Exit(1));
        }
        for entry in &entries {
            let options = DiffRenderOptions {
                line_indicators: sley_diff_merge::render::LineIndicators::default(),
                suppress_blank_empty: false,
                binary: params.patch_binary,
                anchors: params.anchored,
                allow_textconv: true,
                db,
                lazy_fetch: params.lazy_fetch,
                worktree_root: None,
                use_worktree_new: false,
                format,
                abbrev: patch_abbrev,
                src_prefix: &src_prefix,
                dst_prefix: &dst_prefix,
                context: params.context,
                userdiff: Some(&userdiff),
                funcname: None,
                colors: colors.as_ref(),
                word_diff: word_request.as_ref(),
                no_index_contents: Some((
                    entry.old_content.as_deref(),
                    entry.new_content.as_deref(),
                )),
                submodule_format: sley_rev::diff_options::SubmoduleDiffFormat::Short,
                submodule_dirt: None,
                // No-index has no attributes; the rule is core.whitespace (or the
                // default), used only when color is on.
                ws_error: (colors.is_some() && word_request.is_none()).then(|| {
                    let rule = config
                        .as_ref()
                        .and_then(|cfg| cfg.get("core", None, "whitespace"))
                        .and_then(sley_diff_merge::ws::parse_whitespace_rule)
                        .unwrap_or(sley_diff_merge::ws::WS_DEFAULT_RULE);
                    sley_diff_merge::render::WsErrorHighlight {
                        rule,
                        old: false,
                        new: true,
                        context: false,
                    }
                }),
                color_moved,
                interhunk: params.interhunk,
                ws_ignore: params.ws_ignore,
                diff_algorithm: params.diff_algorithm,
                ignore_blank_lines: params.ignore_blank_lines,
                ignore_regexes: params.ignore_regexes,
                line_ranges: None,
                indent_heuristic: params.indent_heuristic,
            };
            write_diff_patch_entry(&mut stdout, &entry.entry, options)?;
        }
    }
    Err(GitError::Exit(1))
}

fn no_index_entries(
    old_spec: &str,
    new_spec: &str,
    pathspecs: &[String],
    format: ObjectFormat,
) -> Result<Vec<NoIndexEntry>> {
    if old_spec == "-" && new_spec == "-" {
        let _ = no_index_read_stdin_side(format)?;
        return Ok(Vec::new());
    }
    let old_path = Path::new(old_spec);
    let new_path = Path::new(new_spec);
    let old_kind = no_index_path_kind(old_spec, old_path)?;
    let new_kind = no_index_path_kind(new_spec, new_path)?;
    no_index_reject_stream_directory_pair(old_kind, new_kind)?;
    if old_kind == NoIndexPathKind::Null && new_kind == NoIndexPathKind::Null {
        return Ok(Vec::new());
    }
    if old_kind == NoIndexPathKind::Null {
        let new = no_index_read_file(new_spec, new_path, format)?;
        return Ok(vec![no_index_entry_from_sides(None, Some(&new))]);
    }
    if new_kind == NoIndexPathKind::Null {
        let old = no_index_read_file(old_spec, old_path, format)?;
        return Ok(vec![no_index_entry_from_sides(Some(&old), None)]);
    }
    let old_is_dir = old_kind == NoIndexPathKind::Directory;
    let new_is_dir = new_kind == NoIndexPathKind::Directory;
    if !pathspecs.is_empty() {
        if pathspecs
            .iter()
            .any(|spec| no_index_pathspec_is_absolute(spec))
            || !(old_is_dir && new_is_dir)
        {
            eprintln!("usage: git diff --no-index [<options>] <path> <path>");
            return Err(GitError::Exit(129));
        }
    }
    if old_is_dir || new_is_dir {
        let old_files = no_index_collect_path(old_spec, old_path, old_is_dir, format)?;
        let new_files = no_index_collect_path(new_spec, new_path, new_is_dir, format)?;
        let mut keys = if old_is_dir == new_is_dir {
            let mut keys = old_files.keys().cloned().collect::<Vec<_>>();
            for key in new_files.keys() {
                if !old_files.contains_key(key) {
                    keys.push(key.clone());
                }
            }
            keys
        } else if old_is_dir {
            new_files.keys().cloned().collect()
        } else {
            old_files.keys().cloned().collect()
        };
        keys.sort();
        let mut entries = Vec::new();
        for key in keys {
            if !no_index_pathspec_matches(&key, pathspecs) {
                continue;
            }
            let old = old_files.get(&key);
            let new = new_files.get(&key);
            if old.map(|side| (&side.content, side.mode))
                == new.map(|side| (&side.content, side.mode))
            {
                continue;
            }
            entries.push(no_index_entry_from_sides(old, new));
        }
        return Ok(entries);
    }
    let old = no_index_read_file(old_spec, old_path, format)?;
    let new = no_index_read_file(new_spec, new_path, format)?;
    if old.content == new.content && old.mode == new.mode {
        return Ok(Vec::new());
    }
    Ok(vec![no_index_entry_from_sides(Some(&old), Some(&new))])
}

fn no_index_path_kind(spec: &str, path: &Path) -> Result<NoIndexPathKind> {
    if spec == "-" {
        return Ok(NoIndexPathKind::Stdin);
    }
    if spec == "/dev/null" {
        return Ok(NoIndexPathKind::Null);
    }
    let metadata = fs::symlink_metadata(path).map_err(|_| no_index_access_error(spec))?;
    let file_type = metadata.file_type();
    if file_type.is_dir() {
        return Ok(NoIndexPathKind::Directory);
    }
    if file_type.is_file() {
        return Ok(NoIndexPathKind::File);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;

        if file_type.is_fifo() {
            return Ok(NoIndexPathKind::Fifo);
        }
    }
    if file_type.is_symlink() {
        let metadata = fs::metadata(path).map_err(|_| no_index_access_error(spec))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileTypeExt;

            if metadata.file_type().is_fifo() {
                return Ok(NoIndexPathKind::Fifo);
            }
        }
        if metadata.is_dir() {
            return Ok(NoIndexPathKind::Directory);
        }
        if metadata.is_file() {
            return Ok(NoIndexPathKind::File);
        }
    }
    Ok(NoIndexPathKind::Other)
}

fn no_index_reject_stream_directory_pair(
    old_kind: NoIndexPathKind,
    new_kind: NoIndexPathKind,
) -> Result<()> {
    if (old_kind == NoIndexPathKind::Stdin && new_kind == NoIndexPathKind::Directory)
        || (old_kind == NoIndexPathKind::Directory && new_kind == NoIndexPathKind::Stdin)
    {
        eprintln!("fatal: cannot compare stdin to a directory");
        return Err(GitError::Exit(1));
    }
    if (old_kind == NoIndexPathKind::Fifo && new_kind == NoIndexPathKind::Directory)
        || (old_kind == NoIndexPathKind::Directory && new_kind == NoIndexPathKind::Fifo)
    {
        eprintln!("fatal: cannot compare a named pipe to a directory");
        return Err(GitError::Exit(1));
    }
    Ok(())
}

fn reverse_no_index_entries(entries: Vec<NoIndexEntry>) -> Vec<NoIndexEntry> {
    let mut reversed = entries
        .into_iter()
        .map(reverse_no_index_entry)
        .collect::<Vec<_>>();
    reversed.sort_by(|left, right| {
        left.entry
            .path
            .cmp(&right.entry.path)
            .then_with(|| left.entry.old_path.cmp(&right.entry.old_path))
            .then_with(|| left.entry.status.code().cmp(&right.entry.status.code()))
    });
    reversed
}

fn reverse_no_index_entry(entry: NoIndexEntry) -> NoIndexEntry {
    let mut reversed_entry = reverse_diff_entry(entry.entry);
    if matches!(
        reversed_entry.status,
        sley_diff_merge::NameStatus::Modified | sley_diff_merge::NameStatus::TypeChanged
    ) && let Some(old_path) = reversed_entry.old_path.take()
    {
        let new_path = reversed_entry.path;
        reversed_entry.path = old_path;
        reversed_entry.old_path = Some(new_path);
    }
    NoIndexEntry {
        entry: reversed_entry,
        old_content: entry.new_content,
        new_content: entry.old_content,
    }
}

fn no_index_collect_path(
    spec: &str,
    path: &Path,
    is_dir: bool,
    format: ObjectFormat,
) -> Result<std::collections::BTreeMap<Vec<u8>, NoIndexSide>> {
    let mut files = std::collections::BTreeMap::new();
    if is_dir {
        no_index_collect_dir(spec, path, path, &mut files, format)?;
    } else {
        files.insert(
            no_index_file_key(spec, path),
            no_index_read_file(spec, path, format)?,
        );
    }
    Ok(files)
}

fn no_index_collect_dir(
    spec: &str,
    root: &Path,
    dir: &Path,
    files: &mut std::collections::BTreeMap<Vec<u8>, NoIndexSide>,
    format: ObjectFormat,
) -> Result<()> {
    let mut children = fs::read_dir(dir)
        .map_err(|_| no_index_access_error(spec))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    children.sort_by_key(|entry| entry.file_name());
    for child in children {
        let path = child.path();
        if path.is_dir() {
            no_index_collect_dir(spec, root, &path, files, format)?;
        } else if path.is_file() {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/")
                .into_bytes();
            let display = no_index_join_display_path(spec, &rel);
            files.insert(rel, no_index_read_file(&display, &path, format)?);
        }
    }
    Ok(())
}

fn no_index_read_file(spec: &str, path: &Path, format: ObjectFormat) -> Result<NoIndexSide> {
    if spec == "-" {
        return no_index_read_stdin_side(format);
    }
    let symlink = fs::symlink_metadata(path)
        .map(|meta| meta.file_type().is_symlink())
        .unwrap_or(false);
    let symlink_to_fifo = symlink && no_index_path_is_fifo_target(path);
    let fifo = !symlink && no_index_path_is_fifo(path);
    let (content, mode) = if symlink && !symlink_to_fifo {
        (read_symlink_bytes_for_diff(path)?, 0o120000)
    } else {
        let content = fs::read(path).map_err(|_| no_index_access_error(spec))?;
        let mode = {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let permissions = fs::metadata(path).map(|meta| meta.permissions().mode());
                if permissions.is_ok_and(|bits| bits & 0o111 != 0) {
                    0o100755
                } else {
                    0o100644
                }
            }
            #[cfg(not(unix))]
            {
                0o100644
            }
        };
        (content, mode)
    };
    Ok(NoIndexSide {
        path: spec.as_bytes().to_vec(),
        content,
        mode,
        oid: (fifo || symlink_to_fifo).then(|| ObjectId::null(format)),
    })
}

#[cfg(unix)]
fn no_index_path_is_fifo(path: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt;

    fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_fifo())
}

#[cfg(not(unix))]
fn no_index_path_is_fifo(_path: &Path) -> bool {
    false
}

#[cfg(unix)]
fn no_index_path_is_fifo_target(path: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt;

    fs::metadata(path).is_ok_and(|meta| meta.file_type().is_fifo())
}

#[cfg(not(unix))]
fn no_index_path_is_fifo_target(_path: &Path) -> bool {
    false
}

fn no_index_file_key(spec: &str, path: &Path) -> Vec<u8> {
    path.file_name()
        .map(|name| name.to_string_lossy().replace('\\', "/").into_bytes())
        .unwrap_or_else(|| spec.as_bytes().to_vec())
}

fn no_index_join_display_path(spec: &str, rel: &[u8]) -> String {
    let rel = String::from_utf8_lossy(rel);
    if spec.ends_with('/') {
        format!("{spec}{rel}")
    } else {
        format!("{spec}/{rel}")
    }
}

fn no_index_pathspec_is_absolute(spec: &str) -> bool {
    no_index_pathspec_pattern(spec)
        .map(|(_, _, pattern)| pattern.starts_with('/'))
        .unwrap_or_else(|| spec.starts_with('/'))
}

fn no_index_pathspec_matches(key: &[u8], pathspecs: &[String]) -> bool {
    if pathspecs.is_empty() {
        return true;
    }
    let key = String::from_utf8_lossy(key);
    let mut saw_positive = false;
    let mut included = false;
    for spec in pathspecs {
        let Some((exclude, glob, pattern)) = no_index_pathspec_pattern(spec) else {
            continue;
        };
        if exclude {
            continue;
        }
        saw_positive = true;
        if no_index_pathspec_pattern_matches(&key, pattern, glob) {
            included = true;
        }
    }
    if !saw_positive {
        included = true;
    }
    for spec in pathspecs {
        let Some((exclude, glob, pattern)) = no_index_pathspec_pattern(spec) else {
            continue;
        };
        if exclude && no_index_pathspec_pattern_matches(&key, pattern, glob) {
            included = false;
        }
    }
    included
}

fn no_index_pathspec_pattern(spec: &str) -> Option<(bool, bool, &str)> {
    if let Some(pattern) = spec.strip_prefix(":!") {
        return Some((true, false, pattern));
    }
    if let Some(rest) = spec.strip_prefix(":(") {
        let (magic, pattern) = rest.split_once(')')?;
        let mut exclude = false;
        let mut glob = false;
        for token in magic.split(',') {
            match token {
                "exclude" | "!" => exclude = true,
                "glob" => glob = true,
                _ => {}
            }
        }
        return Some((exclude, glob, pattern));
    }
    Some((false, false, spec))
}

fn no_index_pathspec_pattern_matches(key: &str, pattern: &str, glob: bool) -> bool {
    if glob {
        return no_index_glob_matches(key.as_bytes(), pattern.as_bytes());
    }
    key == pattern
        || key
            .strip_prefix(pattern)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn no_index_glob_matches(key: &[u8], pattern: &[u8]) -> bool {
    if let Some(rest) = pattern.strip_prefix(b"**/") {
        return key == rest || key.ends_with(&[b"/".as_slice(), rest].concat());
    }
    no_index_glob_matches_inner(key, pattern)
}

fn no_index_glob_matches_inner(mut key: &[u8], mut pattern: &[u8]) -> bool {
    while let Some((&head, tail)) = pattern.split_first() {
        if head == b'*' {
            while pattern.first() == Some(&b'*') {
                pattern = &pattern[1..];
            }
            if pattern.is_empty() {
                return true;
            }
            for idx in 0..=key.len() {
                if no_index_glob_matches_inner(&key[idx..], pattern) {
                    return true;
                }
            }
            return false;
        }
        let Some((&key_head, key_tail)) = key.split_first() else {
            return false;
        };
        if head != key_head {
            return false;
        }
        key = key_tail;
        pattern = tail;
    }
    key.is_empty()
}

fn no_index_resolve_prefixes(
    config: Option<&GitConfig>,
    params: &DiffNoIndexParams<'_>,
) -> (String, String) {
    let mut src_prefix = params.src_prefix.to_string();
    let mut dst_prefix = params.dst_prefix.to_string();
    if let Some(config) = config {
        let cfg_no_prefix = config.get_bool("diff", None, "noprefix").unwrap_or(false);
        let cfg_mnemonic = config
            .get_bool("diff", None, "mnemonicprefix")
            .unwrap_or(false);
        if cfg_no_prefix {
            src_prefix.clear();
            dst_prefix.clear();
        } else if cfg_mnemonic {
            src_prefix = "1/".to_string();
            dst_prefix = "2/".to_string();
        } else {
            src_prefix = config
                .get("diff", None, "srcprefix")
                .map(str::to_owned)
                .unwrap_or_else(|| "a/".to_string());
            dst_prefix = config
                .get("diff", None, "dstprefix")
                .map(str::to_owned)
                .unwrap_or_else(|| "b/".to_string());
        }
        if params.cli_default_prefix {
            src_prefix = "a/".to_string();
            dst_prefix = "b/".to_string();
        }
        if params.cli_no_prefix {
            src_prefix.clear();
            dst_prefix.clear();
        }
        if let Some(prefix) = params.cli_src_prefix {
            src_prefix = prefix.to_string();
        }
        if let Some(prefix) = params.cli_dst_prefix {
            dst_prefix = prefix.to_string();
        }
    }
    (src_prefix, dst_prefix)
}

fn run_external_diff_no_index_entries(
    entries: &[NoIndexEntry],
    userdiff: &commands::userdiff::UserdiffResolver,
    global: Option<&ExternalDiffCommand>,
    options: ExternalDiffRunOptions<'_>,
) -> Result<Option<i32>> {
    let mut handled = false;
    let mut found_changes = false;
    let mut output_file = match options.output {
        Some(path) if !options.quiet => Some(
            fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(path)?,
        ),
        _ => None,
    };

    for (idx, entry) in entries.iter().enumerate() {
        let command = external_diff_for_entry(&entry.entry, userdiff, global)?;
        let Some(command) = command else {
            continue;
        };
        handled = true;
        if options.quiet && !command.trust_exit_code {
            found_changes = true;
            continue;
        }
        let rc = run_one_external_diff_no_index(
            entry,
            &command,
            idx + 1,
            entries.len(),
            options.autocrlf,
            options.quiet,
            output_file.as_mut(),
        )?;
        match (command.trust_exit_code, rc) {
            (false, 0) => found_changes = true,
            (true, 0) => {}
            (true, 1) => found_changes = true,
            _ => {
                let path = String::from_utf8_lossy(&entry.entry.path);
                eprintln!("fatal: external diff died, stopping at {path}");
                return Err(GitError::Exit(128));
            }
        }
    }

    if !handled {
        return Ok(None);
    }
    let code = if (options.quiet || options.exit_code) && found_changes {
        1
    } else {
        0
    };
    Ok(Some(code))
}

fn run_one_external_diff_no_index(
    entry: &NoIndexEntry,
    command: &ExternalDiffCommand,
    counter: usize,
    total: usize,
    autocrlf: bool,
    quiet: bool,
    output_file: Option<&mut fs::File>,
) -> Result<i32> {
    let old_path = entry.entry.old_path.as_ref().unwrap_or(&entry.entry.path);
    let old_path = String::from_utf8_lossy(old_path).into_owned();
    let new_path = String::from_utf8_lossy(&entry.entry.path).into_owned();
    let old_file = external_no_index_file(&old_path, entry.old_content.as_deref(), autocrlf)?;
    let new_file = external_no_index_file(&new_path, entry.new_content.as_deref(), autocrlf)?;
    let old_hex = ObjectId::null(ObjectFormat::Sha1).to_hex();
    let new_hex = ObjectId::null(ObjectFormat::Sha1).to_hex();
    let old_mode = external_diff_mode(entry.entry.old_mode);
    let new_mode = external_diff_mode(entry.entry.new_mode);
    let args = [
        old_path.clone(),
        old_file.path.to_string_lossy().into_owned(),
        old_hex,
        old_mode,
        new_file.path.to_string_lossy().into_owned(),
        new_hex,
        new_mode,
        new_path,
    ];
    let shell_command = format!("{} \"$@\"", command.command);
    let mut child = ProcessCommand::new("sh");
    child
        .arg("-c")
        .arg(shell_command)
        .arg(&command.command)
        .args(args)
        .env("GIT_DIFF_PATH_COUNTER", counter.to_string())
        .env("GIT_DIFF_PATH_TOTAL", total.to_string());
    if quiet {
        child.stdout(std::process::Stdio::null());
    } else if let Some(file) = output_file {
        child.stdout(file.try_clone()?);
    }
    let status = child.status()?;
    Ok(status.code().unwrap_or(128))
}

fn external_no_index_file(
    display_path: &str,
    content: Option<&[u8]>,
    autocrlf: bool,
) -> Result<ExternalDiffFile> {
    let Some(content) = content else {
        return Ok(ExternalDiffFile {
            path: PathBuf::from("/dev/null"),
            temp_dir: None,
        });
    };
    let path = PathBuf::from(display_path);
    if path.exists() {
        return Ok(ExternalDiffFile {
            path,
            temp_dir: None,
        });
    }
    let temp_dir = unique_external_diff_temp_dir()?;
    let path = temp_dir.join(repo_path_to_path(display_path.as_bytes()));
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let content = if autocrlf && !content.contains(&0) {
        lf_to_crlf(content)
    } else {
        content.to_vec()
    };
    fs::write(&path, content)?;
    Ok(ExternalDiffFile {
        path,
        temp_dir: Some(temp_dir),
    })
}

fn no_index_read_stdin_side(format: ObjectFormat) -> Result<NoIndexSide> {
    let mut content = Vec::new();
    io::stdin().read_to_end(&mut content)?;
    Ok(NoIndexSide {
        path: b"-".to_vec(),
        content,
        mode: 0o100644,
        oid: Some(ObjectId::null(format)),
    })
}

fn no_index_access_error(spec: &str) -> GitError {
    eprintln!("error: Could not access '{spec}'");
    GitError::Exit(1)
}

fn no_index_entry_from_sides(old: Option<&NoIndexSide>, new: Option<&NoIndexSide>) -> NoIndexEntry {
    let status = match (old, new) {
        (None, Some(_)) => sley_diff_merge::NameStatus::Added,
        (Some(_), None) => sley_diff_merge::NameStatus::Deleted,
        (Some(old), Some(new)) => sley_diff_merge::modify_or_type_change(old.mode, new.mode),
        (None, None) => sley_diff_merge::NameStatus::Modified,
    };
    let path = new
        .or(old)
        .map(|side| side.path.clone())
        .unwrap_or_default();
    NoIndexEntry {
        entry: sley_diff_merge::NameStatusEntry {
            status,
            path: BString::from(path),
            old_path: match (old, new) {
                (Some(old), Some(new)) if old.path != new.path => {
                    Some(BString::from(old.path.clone()))
                }
                _ => None,
            },
            old_mode: old.map(|side| side.mode),
            new_mode: new.map(|side| side.mode),
            old_oid: old.and_then(|side| side.oid),
            new_oid: new.and_then(|side| side.oid),
        },
        old_content: old.map(|side| side.content.clone()),
        new_content: new.map(|side| side.content.clone()),
    }
}
