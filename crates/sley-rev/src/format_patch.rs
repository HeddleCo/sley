//! Repository-level planning for `format-patch` patch series.
//!
//! This module owns revision setup, oldest-first non-merge selection,
//! path-limited history simplification, upstream patch-id de-duplication,
//! relative-prefix resolution, and base/prerequisite planning. It deliberately
//! does not render mail, choose filenames, write files, or print diagnostics.

use std::collections::HashSet;
use std::path::Path;

use sley_config::GitConfig;
use sley_core::{GitError, ObjectFormat, ObjectId, Result};
use sley_object::Commit;
use sley_odb::{FileObjectDatabase, ObjectReader};
use sley_refs::{FileRefStore, RefTarget};

use crate::revlist::{rev_list_date_order, rev_list_walk_commits};
use crate::{
    CommitRecord, Pathspec, PathspecMatchMagic, RevisionSetupContext, SimplifyOptions,
    ambiguous_argument_error, peel_to_commit, setup_revisions, simplify_history,
};

/// How patch-series base information is selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormatPatchBaseMode {
    /// Never emit base/prerequisite information.
    None,
    /// Resolve and validate this explicit base revision.
    Commit(String),
    /// Resolve the current branch's configured upstream when available.
    Auto,
    /// Honor `format.useAutoBase` from the supplied effective config.
    Config,
}

/// How diff paths are made relative to the invocation location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormatPatchRelativeMode {
    /// Honor `diff.relative` from the supplied effective config.
    Config,
    /// Keep repository-root-relative paths.
    Off,
    /// Strip this explicit repository path, or the invocation CWD when absent.
    On(Option<String>),
}

/// Diff semantics selected for every patch emitted by one `format-patch`
/// invocation, including cover-letter interdiffs.
///
/// Keeping this policy in the repository-level plan prevents secondary diff
/// renderers from reconstructing command-line state and silently falling back
/// to generic diff defaults.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatPatchDiffPolicy {
    /// Detect renames between the compared trees.
    pub detect_renames: bool,
    /// Detect copies between the compared trees.
    pub detect_copies: bool,
    /// Consider unmodified files as copy sources.
    pub find_copies_harder: bool,
    /// Rename similarity threshold, as a percentage.
    pub rename_threshold: u8,
    /// Copy similarity threshold, as a percentage.
    pub copy_threshold: u8,
    /// Unified-diff context lines.
    pub context_lines: usize,
    /// Prefix prepended to old-side paths.
    pub src_prefix: String,
    /// Prefix prepended to new-side paths.
    pub dst_prefix: String,
    /// Optional diff order file, resolved by the presentation layer.
    pub order_file: Option<String>,
    /// Emit applicable binary patch bodies.
    pub binary: bool,
}

impl Default for FormatPatchDiffPolicy {
    fn default() -> Self {
        Self {
            detect_renames: true,
            detect_copies: false,
            find_copies_harder: false,
            rename_threshold: sley_diff_merge::DEFAULT_RENAME_THRESHOLD,
            copy_threshold: sley_diff_merge::DEFAULT_RENAME_THRESHOLD,
            context_lines: 3,
            src_prefix: "a/".to_string(),
            dst_prefix: "b/".to_string(),
            order_file: None,
            binary: true,
        }
    }
}

/// Semantic options consumed by [`plan_format_patch_series`].
#[derive(Debug, Clone)]
pub struct FormatPatchPlanOptions {
    /// Revision setup arguments, including a possible `--` pathspec separator.
    pub setup_args: Vec<String>,
    /// Keep only the newest `count` selected commits before oldest-first output.
    pub count: Option<usize>,
    /// Treat one bare revision as an inclusive root range.
    pub root: bool,
    /// Drop commits whose patch-id already occurs in the excluded side.
    pub ignore_if_in_upstream: bool,
    /// Base selection policy.
    pub base: FormatPatchBaseMode,
    /// Relative path selection policy.
    pub relative: FormatPatchRelativeMode,
    /// Diff policy shared by patch and interdiff rendering.
    pub diff: FormatPatchDiffPolicy,
}

/// Existing repository handles and paths needed to plan a patch series.
pub struct FormatPatchPlanRequest<'a> {
    /// Repository `$GIT_DIR`.
    pub git_dir: &'a Path,
    /// Worktree root when the repository has one.
    pub worktree_root: Option<&'a Path>,
    /// Invocation working directory.
    pub cwd: &'a Path,
    /// Repository object format.
    pub format: ObjectFormat,
    /// Already-open object database.
    pub objects: &'a FileObjectDatabase,
    /// Already-open ref store.
    pub refs: &'a FileRefStore,
    /// Effective repository configuration, including invocation overrides.
    pub config: &'a GitConfig,
    /// Planning controls.
    pub options: &'a FormatPatchPlanOptions,
}

/// Revision resolution seam used to preserve caller-specific warnings and
/// disambiguation while the engine owns the surrounding plan.
pub trait FormatPatchRevisionResolver {
    /// Resolve one revision spelling to an object id.
    fn resolve_revision(&mut self, revision: &str) -> Result<ObjectId>;
}

/// Patch-id seam used for upstream de-duplication and base prerequisites.
///
/// The engine chooses which records and stability mode are needed. The caller
/// supplies the byte renderer/digest implementation used by its patch stack.
pub trait FormatPatchPatchId {
    /// Compute a patch id for `record`; `stable` requests order-independent
    /// patch-id folding. Empty commits return `None`.
    fn patch_id(&mut self, record: &CommitRecord, stable: bool) -> Result<Option<Vec<u8>>>;
}

/// Optional message predicate applied after revision ordering and merge removal.
pub trait FormatPatchCommitFilter {
    /// Return true when `record` should remain in the patch series.
    fn retain(&mut self, record: &CommitRecord) -> bool;
}

/// Injected services used during patch-series planning.
pub struct FormatPatchPlanServices<'a> {
    /// Caller-aware revision resolver.
    pub revisions: &'a mut dyn FormatPatchRevisionResolver,
    /// Patch-id provider.
    pub patch_ids: &'a mut dyn FormatPatchPatchId,
    /// Optional commit-message predicate.
    pub commit_filter: Option<&'a mut dyn FormatPatchCommitFilter>,
}

/// Base commit and stable prerequisite patch ids emitted in each patch footer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatPatchBaseInfo {
    /// Validated base commit.
    pub base: ObjectId,
    /// Stable patch ids between the base and the selected series.
    pub prerequisites: Vec<Vec<u8>>,
}

/// Structured patch-series plan returned to mail and filesystem presentation.
#[derive(Debug, Clone)]
pub struct FormatPatchPlanOutcome {
    /// Effective revision arguments after default-HEAD/bare-range normalization.
    pub revision_args: Vec<String>,
    /// Selected non-merge commits in oldest-first output order.
    pub commits: Vec<CommitRecord>,
    /// Revision-setup pathspec strings.
    pub pathspecs: Vec<String>,
    /// Repository-relative prefix to strip from rendered diff paths.
    pub relative_prefix: Option<Vec<u8>>,
    /// Diff policy captured from this invocation.
    pub diff: FormatPatchDiffPolicy,
    /// Optional validated base and prerequisite patch ids.
    pub base: Option<FormatPatchBaseInfo>,
}

/// Classified planning failure for caller-owned diagnostics.
#[derive(Debug)]
pub enum FormatPatchPlanError {
    /// Revision setup left an option it does not understand.
    UnsupportedSetupArgument { argument: String },
    /// The requested base is not a strict ancestor of the selected series.
    BaseNotAncestor { base: ObjectId },
    /// Any other repository, revision, object, or patch-id failure.
    Engine(GitError),
}

impl std::fmt::Display for FormatPatchPlanError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedSetupArgument { argument } => {
                write!(formatter, "unsupported format-patch option {argument}")
            }
            Self::BaseNotAncestor { .. } => {
                formatter.write_str("base commit should be the ancestor of revision list")
            }
            Self::Engine(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for FormatPatchPlanError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Engine(error) => Some(error),
            Self::UnsupportedSetupArgument { .. } | Self::BaseNotAncestor { .. } => None,
        }
    }
}

impl From<GitError> for FormatPatchPlanError {
    fn from(error: GitError) -> Self {
        Self::Engine(error)
    }
}

/// Build the semantic patch-series plan without rendering or filesystem I/O.
pub fn plan_format_patch_series(
    request: FormatPatchPlanRequest<'_>,
    mut services: FormatPatchPlanServices<'_>,
) -> std::result::Result<FormatPatchPlanOutcome, FormatPatchPlanError> {
    let setup_args = format_patch_setup_args(request.options);
    if let Some(revision) = format_patch_bare_exclude(request.options) {
        let oid = services
            .revisions
            .resolve_revision(revision)
            .map_err(|_| ambiguous_argument_error(revision))?;
        peel_to_commit(request.objects, request.format, &oid)
            .map_err(|_| ambiguous_argument_error(revision))?;
    }
    let setup = setup_revisions(
        &setup_args,
        &RevisionSetupContext {
            git_dir: request.git_dir,
            worktree_root: request.worktree_root,
            cwd: request.cwd,
            format: request.format,
            reader: request.objects,
            config: Some(request.config),
        },
    )?;
    if let Some(argument) = setup.leftovers.first() {
        return Err(FormatPatchPlanError::UnsupportedSetupArgument {
            argument: argument.clone(),
        });
    }
    let starts = setup
        .options
        .positives
        .iter()
        .map(|tip| peel_to_commit(request.objects, request.format, &tip.oid))
        .collect::<Result<Vec<_>>>()?;

    let mut excluded = HashSet::new();
    let mut excluded_records = Vec::new();
    for oid in setup.options.negatives {
        for record in rev_list_walk_commits(request.objects, request.format, [oid], false)? {
            excluded.insert(record.oid);
            excluded_records.push(record);
        }
    }
    let walked = rev_list_walk_commits(request.objects, request.format, starts, false)?;
    let upstream_patch_ids = if request.options.ignore_if_in_upstream {
        patch_ids_for_records(
            request.objects,
            request.format,
            &excluded_records,
            false,
            services.patch_ids,
        )?
    } else {
        HashSet::new()
    };

    let reachable = walked
        .iter()
        .filter(|record| !excluded.contains(&record.oid))
        .collect::<Vec<_>>();
    let ordered = rev_list_date_order(reachable)?;
    let mut selected = ordered
        .into_iter()
        .filter(|record| record.parents.len() <= 1)
        .cloned()
        .collect::<Vec<_>>();
    if let Some(filter) = services.commit_filter.as_deref_mut() {
        selected.retain(|record| filter.retain(record));
    }
    if !setup.pathspecs.is_empty() {
        let pathspec = Pathspec::parse(
            setup.pathspecs.iter().map(|spec| spec.as_bytes()),
            PathspecMatchMagic::default(),
        )
        .map_err(|error| GitError::Command(format!("bad pathspec: {error:?}")))?;
        selected = simplify_history(
            request.objects,
            request.format,
            selected,
            &pathspec,
            SimplifyOptions {
                full_history: false,
                first_parent: false,
                ..SimplifyOptions::default()
            },
        )?;
    }
    if let Some(count) = request.options.count {
        selected.truncate(count);
    }
    if !upstream_patch_ids.is_empty() {
        let mut kept = Vec::with_capacity(selected.len());
        for record in selected {
            let parent_tree = parent_tree(request.objects, request.format, &record)?;
            if record.commit.tree != parent_tree
                && let Some(id) = services.patch_ids.patch_id(&record, false)?
                && upstream_patch_ids.contains(&id)
            {
                continue;
            }
            kept.push(record);
        }
        selected = kept;
    }
    selected.reverse();

    let relative_prefix = resolve_relative_prefix(
        request.cwd,
        request.worktree_root,
        request.config,
        &request.options.relative,
    );
    let base = resolve_base_info(&request, &selected, services.revisions, services.patch_ids)?;
    Ok(FormatPatchPlanOutcome {
        revision_args: setup_args,
        commits: selected,
        pathspecs: setup.pathspecs,
        relative_prefix,
        diff: request.options.diff.clone(),
        base,
    })
}

fn format_patch_setup_args(options: &FormatPatchPlanOptions) -> Vec<String> {
    let mut args = options.setup_args.clone();
    if let Some(revision) = format_patch_bare_exclude(options) {
        args[0] = "HEAD".to_string();
        args.insert(1, format!("^{revision}"));
        return args;
    }
    let revision_end = args
        .iter()
        .position(|arg| arg == "--")
        .unwrap_or(args.len());
    if revision_end == 0 {
        args.insert(0, "HEAD".to_string());
        if options.count.is_none() && !options.root {
            args.insert(1, "^HEAD".to_string());
        }
    }
    args
}

fn format_patch_bare_exclude(options: &FormatPatchPlanOptions) -> Option<&str> {
    if options.count.is_some() || options.root {
        return None;
    }
    let revision_end = options
        .setup_args
        .iter()
        .position(|arg| arg == "--")
        .unwrap_or(options.setup_args.len());
    if revision_end != 1 {
        return None;
    }
    let revision = options.setup_args[0].as_str();
    (!revision.starts_with('^') && !revision.contains("..")).then_some(revision)
}

fn patch_ids_for_records(
    objects: &FileObjectDatabase,
    format: ObjectFormat,
    records: &[CommitRecord],
    stable: bool,
    patch_ids: &mut dyn FormatPatchPatchId,
) -> std::result::Result<HashSet<Vec<u8>>, FormatPatchPlanError> {
    let mut ids = HashSet::new();
    for record in records {
        if record.parents.len() > 1 {
            continue;
        }
        let parent_tree = parent_tree(objects, format, record)?;
        if record.commit.tree != parent_tree
            && let Some(id) = patch_ids.patch_id(record, stable)?
        {
            ids.insert(id);
        }
    }
    Ok(ids)
}

fn parent_tree(
    objects: &FileObjectDatabase,
    format: ObjectFormat,
    record: &CommitRecord,
) -> Result<ObjectId> {
    match record.parents.first() {
        Some(parent) => commit_tree_oid(objects, format, parent),
        None => Ok(ObjectId::empty_tree(format)),
    }
}

fn commit_tree_oid(
    objects: &FileObjectDatabase,
    format: ObjectFormat,
    oid: &ObjectId,
) -> Result<ObjectId> {
    let object = objects.read_object(oid)?;
    Ok(Commit::parse_ref(format, &object.body)?.tree)
}

fn resolve_base_info(
    request: &FormatPatchPlanRequest<'_>,
    commits: &[CommitRecord],
    revisions: &mut dyn FormatPatchRevisionResolver,
    patch_ids: &mut dyn FormatPatchPatchId,
) -> std::result::Result<Option<FormatPatchBaseInfo>, FormatPatchPlanError> {
    if commits.is_empty() {
        return Ok(None);
    }
    let base = match &request.options.base {
        FormatPatchBaseMode::None => return Ok(None),
        FormatPatchBaseMode::Commit(revision) => {
            Some(resolve_base_commit(request, revisions, revision)?)
        }
        FormatPatchBaseMode::Auto => resolve_upstream_base(request, revisions)?,
        FormatPatchBaseMode::Config => match request.config.get("format", None, "useAutoBase") {
            Some(value)
                if value.eq_ignore_ascii_case("true") || value.eq_ignore_ascii_case("whenAble") =>
            {
                resolve_upstream_base(request, revisions)?
            }
            _ => None,
        },
    };
    let Some(base) = base else {
        return Ok(None);
    };
    validate_base_commit(
        request.git_dir,
        request.objects,
        request.format,
        &base,
        commits,
    )?;
    let prerequisites =
        prerequisite_patch_ids(request.objects, request.format, &base, commits, patch_ids)?;
    Ok(Some(FormatPatchBaseInfo {
        base,
        prerequisites,
    }))
}

fn resolve_base_commit(
    request: &FormatPatchPlanRequest<'_>,
    revisions: &mut dyn FormatPatchRevisionResolver,
    revision: &str,
) -> std::result::Result<ObjectId, FormatPatchPlanError> {
    let oid = revisions
        .resolve_revision(revision)
        .map_err(|_| ambiguous_argument_error(revision))?;
    peel_to_commit(request.objects, request.format, &oid)
        .map_err(|_| ambiguous_argument_error(revision).into())
}

fn resolve_upstream_base(
    request: &FormatPatchPlanRequest<'_>,
    revisions: &mut dyn FormatPatchRevisionResolver,
) -> std::result::Result<Option<ObjectId>, FormatPatchPlanError> {
    let Some(branch) = current_branch_name(request.refs)? else {
        return Ok(None);
    };
    let Some(merge) = request.config.get("branch", Some(&branch), "merge") else {
        return Ok(None);
    };
    let remote = request
        .config
        .get("branch", Some(&branch), "remote")
        .unwrap_or(".");
    let revision = if remote == "." {
        // git set_merge for remote `.`: expand short merge names to heads.
        if merge.starts_with("refs/") {
            merge.to_string()
        } else {
            format!("refs/heads/{merge}")
        }
    } else {
        let short = merge.strip_prefix("refs/heads/").unwrap_or(merge);
        format!("refs/remotes/{remote}/{short}")
    };
    resolve_base_commit(request, revisions, &revision).map(Some)
}

fn current_branch_name(refs: &FileRefStore) -> Result<Option<String>> {
    match refs.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(name)) => Ok(name.strip_prefix("refs/heads/").map(str::to_string)),
        _ => Ok(None),
    }
}

fn validate_base_commit(
    git_dir: &Path,
    objects: &FileObjectDatabase,
    format: ObjectFormat,
    base: &ObjectId,
    commits: &[CommitRecord],
) -> std::result::Result<(), FormatPatchPlanError> {
    if commits.iter().any(|record| &record.oid == base) {
        return Err(FormatPatchPlanError::BaseNotAncestor { base: *base });
    }
    for record in commits {
        if !crate::is_ancestor(git_dir, format, objects, base, &record.oid)? {
            return Err(FormatPatchPlanError::BaseNotAncestor { base: *base });
        }
    }
    Ok(())
}

fn prerequisite_patch_ids(
    objects: &FileObjectDatabase,
    format: ObjectFormat,
    base: &ObjectId,
    commits: &[CommitRecord],
    patch_ids: &mut dyn FormatPatchPatchId,
) -> std::result::Result<Vec<Vec<u8>>, FormatPatchPlanError> {
    let Some(oldest_parent) = commits[0].parents.first() else {
        return Ok(Vec::new());
    };
    let selected = commits
        .iter()
        .map(|record| record.oid)
        .collect::<HashSet<_>>();
    let mut chain = Vec::new();
    let mut cursor = *oldest_parent;
    while &cursor != base {
        if selected.contains(&cursor) {
            break;
        }
        let record = read_commit_record(objects, format, cursor)?;
        let Some(parent) = record.parents.first().copied() else {
            break;
        };
        chain.push(record);
        cursor = parent;
    }
    chain.reverse();
    let mut ids = Vec::new();
    for record in &chain {
        if let Some(id) = patch_ids.patch_id(record, true)? {
            ids.push(id);
        }
    }
    Ok(ids)
}

fn read_commit_record(
    objects: &FileObjectDatabase,
    format: ObjectFormat,
    oid: ObjectId,
) -> Result<CommitRecord> {
    let object = objects.read_object(&oid)?;
    let commit: Commit = Commit::parse_ref(format, &object.body)?.into();
    Ok(CommitRecord {
        oid,
        parents: commit.parents.clone(),
        commit,
    })
}

fn resolve_relative_prefix(
    cwd: &Path,
    worktree_root: Option<&Path>,
    config: &GitConfig,
    mode: &FormatPatchRelativeMode,
) -> Option<Vec<u8>> {
    match mode {
        FormatPatchRelativeMode::Off => None,
        FormatPatchRelativeMode::On(Some(path)) => normalize_relative_prefix(path),
        FormatPatchRelativeMode::On(None) => cwd_relative_prefix(cwd, worktree_root),
        FormatPatchRelativeMode::Config => config
            .get_bool("diff", None, "relative")
            .unwrap_or(false)
            .then(|| cwd_relative_prefix(cwd, worktree_root))
            .flatten(),
    }
}

fn cwd_relative_prefix(cwd: &Path, worktree_root: Option<&Path>) -> Option<Vec<u8>> {
    let root = worktree_root?;
    let relative = cwd.strip_prefix(root).ok()?;
    normalize_relative_prefix(&relative.to_string_lossy().replace('\\', "/"))
}

fn normalize_relative_prefix(path: &str) -> Option<Vec<u8>> {
    let trimmed = path.trim_matches('/');
    if trimmed.is_empty() || trimmed == "." {
        return None;
    }
    let mut bytes = trimmed.as_bytes().to_vec();
    bytes.push(b'/');
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sley_object::{EncodedObject, ObjectType};
    use sley_odb::ObjectWriter;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct Resolver<'a> {
        git_dir: &'a Path,
        objects: &'a FileObjectDatabase,
        config: &'a GitConfig,
    }

    impl FormatPatchRevisionResolver for Resolver<'_> {
        fn resolve_revision(&mut self, revision: &str) -> Result<ObjectId> {
            crate::resolve_revision_with_config(
                self.git_dir,
                ObjectFormat::Sha1,
                self.objects,
                revision,
                self.config,
            )
        }
    }

    struct OidPatchIds;

    impl FormatPatchPatchId for OidPatchIds {
        fn patch_id(&mut self, record: &CommitRecord, _stable: bool) -> Result<Option<Vec<u8>>> {
            Ok(Some(record.oid.to_hex().into_bytes()))
        }
    }

    #[test]
    fn no_argument_series_is_empty_unless_count_or_root_requests_head() {
        let options = |count, root| FormatPatchPlanOptions {
            setup_args: Vec::new(),
            count,
            root,
            ignore_if_in_upstream: false,
            base: FormatPatchBaseMode::None,
            relative: FormatPatchRelativeMode::Off,
            diff: FormatPatchDiffPolicy::default(),
        };
        assert_eq!(
            format_patch_setup_args(&options(None, false)),
            ["HEAD", "^HEAD"]
        );
        assert_eq!(format_patch_setup_args(&options(Some(1), false)), ["HEAD"]);
        assert_eq!(format_patch_setup_args(&options(None, true)), ["HEAD"]);
    }

    #[test]
    fn plans_oldest_first_range_base_and_relative_prefix() {
        let root = std::env::temp_dir().join(format!(
            "sley-format-patch-plan-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let git_dir = root.join(".git");
        let worktree = root.join("worktree");
        let cwd = worktree.join("subdir");
        fs::create_dir_all(git_dir.join("objects")).expect("objects");
        fs::create_dir_all(&cwd).expect("worktree");
        let objects = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let tree = objects
            .write_object(EncodedObject::new(ObjectType::Tree, Vec::new()))
            .expect("tree");
        let base = write_commit(&objects, tree, Vec::new(), b"base\n", 1);
        let middle = write_commit(&objects, tree, vec![base], b"middle\n", 2);
        let tip = write_commit(&objects, tree, vec![middle], b"tip\n", 3);
        fs::write(git_dir.join("HEAD"), format!("{tip}\n")).expect("HEAD");

        let config = GitConfig::default();
        let refs = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let diff = FormatPatchDiffPolicy {
            context_lines: 7,
            src_prefix: "old/".into(),
            dst_prefix: "new/".into(),
            ..FormatPatchDiffPolicy::default()
        };
        let options = FormatPatchPlanOptions {
            setup_args: vec![base.to_hex()],
            count: None,
            root: false,
            ignore_if_in_upstream: false,
            base: FormatPatchBaseMode::Commit(base.to_hex()),
            relative: FormatPatchRelativeMode::On(None),
            diff: diff.clone(),
        };
        let mut revisions = Resolver {
            git_dir: &git_dir,
            objects: &objects,
            config: &config,
        };
        let mut patch_ids = OidPatchIds;
        let outcome = plan_format_patch_series(
            FormatPatchPlanRequest {
                git_dir: &git_dir,
                worktree_root: Some(&worktree),
                cwd: &cwd,
                format: ObjectFormat::Sha1,
                objects: &objects,
                refs: &refs,
                config: &config,
                options: &options,
            },
            FormatPatchPlanServices {
                revisions: &mut revisions,
                patch_ids: &mut patch_ids,
                commit_filter: None,
            },
        )
        .expect("plan");

        assert_eq!(
            outcome
                .commits
                .iter()
                .map(|record| record.oid)
                .collect::<Vec<_>>(),
            [middle, tip]
        );
        assert_eq!(
            outcome.relative_prefix.as_deref(),
            Some(b"subdir/".as_slice())
        );
        assert_eq!(outcome.diff, diff);
        assert_eq!(
            outcome.base,
            Some(FormatPatchBaseInfo {
                base,
                prerequisites: Vec::new(),
            })
        );
        assert_eq!(
            outcome.revision_args,
            vec!["HEAD".to_string(), format!("^{base}")]
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn classifies_planning_errors_for_cli_rendering() {
        let base = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("oid");
        assert_eq!(
            FormatPatchPlanError::UnsupportedSetupArgument {
                argument: "--future".into(),
            }
            .to_string(),
            "unsupported format-patch option --future"
        );
        assert_eq!(
            FormatPatchPlanError::BaseNotAncestor { base }.to_string(),
            "base commit should be the ancestor of revision list"
        );
    }

    fn write_commit(
        objects: &FileObjectDatabase,
        tree: ObjectId,
        parents: Vec<ObjectId>,
        message: &[u8],
        timestamp: i64,
    ) -> ObjectId {
        let identity = format!("Example <example.invalid> {timestamp} +0000").into_bytes();
        let commit = Commit {
            tree,
            parents,
            author: identity.clone(),
            committer: identity,
            encoding: None,
            message: message.to_vec(),
        };
        objects
            .write_object(EncodedObject::new(ObjectType::Commit, commit.write()))
            .expect("commit")
    }
}
