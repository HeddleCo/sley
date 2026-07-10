//! `git merge-tree` — perform a merge without touching the index or working tree.
//!
//! Two modes are implemented, matching upstream `git`:
//!
//! * The modern `git merge-tree [--write-tree] <branch1> <branch2>` mode computes
//!   a full 3-way merge (reusing [`sley_diff_merge::merge_blobs`] for file content
//!   and [`sley_rev::merge_bases`] for ancestry), writes the resulting top-level
//!   tree to the object database, and prints its object id. On conflict it also
//!   prints the "Conflicted file info" stage list and informational messages, and
//!   exits with status 1. See `git merge-tree`'s OUTPUT section for the exact
//!   shape; this module reproduces it byte-for-byte for the common cases (clean
//!   merges, content / add-add / modify-delete conflicts) including the `-z`,
//!   `--name-only`, and `--[no-]messages` variants.
//!
//! * The deprecated `git merge-tree <base-tree> <branch1> <branch2>` "trivial
//!   merge" mode emits a semi-diff of the trivially merged entries that differ
//!   from `<branch1>`.
//!
//! Command modules pull their shared plumbing from the crate root; a glob import
//! works because a submodule can access its ancestor module's items (including
//! private ones), so every helper, type, and re-export visible at the crate root
//! is in scope here without re-listing it.
use crate::*;
use sley::plumbing::{sley_config, sley_core, sley_rev, sley_worktree};

/// Which top-level mode `git merge-tree` runs in. Selected explicitly via
/// `--write-tree` / `--trivial-merge`, otherwise inferred from the positional
/// argument count (2 → real merge, 3 → trivial merge), exactly like upstream.
#[derive(Clone, Copy, PartialEq, Eq)]
enum MergeTreeMode {
    /// Inferred from the number of positional arguments.
    Auto,
    /// Modern real merge (`--write-tree`).
    RealMerge,
    /// Deprecated trivial merge (`--trivial-merge`).
    TrivialMerge,
}

/// Parsed `git merge-tree` invocation.
struct MergeTreeOptions {
    mode: MergeTreeMode,
    /// NUL-terminate the conflicted-file-info "lines" and start the messages
    /// section with a NUL instead of a newline; also do not quote filenames.
    nul: bool,
    /// In the conflicted-file-info section, list only filenames (no mode/oid/stage)
    /// and never repeat a filename for multiple stages.
    name_only: bool,
    /// `Some(true)`/`Some(false)` force-enable / force-disable informational
    /// messages; `None` keeps the default (messages only when there are
    /// conflicts).
    messages: Option<bool>,
    /// Suppress all output and exit as early as possible.
    quiet: bool,
    /// Read a batch of merge requests from stdin.
    stdin: bool,
    /// Allow merging histories with no common ancestor.
    allow_unrelated_histories: bool,
    /// An explicit merge base (`--merge-base=<tree-ish>`); when set, the two
    /// positional arguments only need to be tree-ish, not commits.
    merge_base: Option<String>,
    /// Accumulated `-X` / `--strategy-option` values (e.g. `ours`, `theirs`).
    strategy_options: Vec<String>,
    /// Positional arguments (the branches / trees to merge).
    positionals: Vec<String>,
    /// Whether the invocation explicitly used any option other than
    /// `--trivial-merge`. Upstream rejects every such spelling in trivial mode,
    /// including negated options and the explicit `--` separator.
    trivial_incompatible_option: bool,
}

impl Default for MergeTreeOptions {
    fn default() -> Self {
        Self {
            mode: MergeTreeMode::Auto,
            nul: false,
            name_only: false,
            messages: None,
            quiet: false,
            stdin: false,
            allow_unrelated_histories: false,
            merge_base: None,
            strategy_options: Vec::new(),
            positionals: Vec::new(),
            trivial_incompatible_option: false,
        }
    }
}

/// The exact usage block `git merge-tree` prints (to stderr, exit 129) when the
/// arguments do not form a valid invocation.
const MERGE_TREE_USAGE: &str = "\
usage: git merge-tree [--write-tree] [<options>] <branch1> <branch2>
   or: git merge-tree [--trivial-merge] <base-tree> <branch1> <branch2>

    --write-tree          do a real merge instead of a trivial merge
    --trivial-merge       do a trivial merge only
    --[no-]messages       also show informational/conflict messages
    --quiet               suppress all output; only exit status wanted
    -z                    separate paths with the NUL character
    --name-only           list filenames without modes/oids/stages
    --allow-unrelated-histories
                          allow merging unrelated histories
    --stdin               perform multiple merges, one per line of input
    --[no-]merge-base <tree-ish>
                          specify a merge-base for the merge
    -X, --[no-]strategy-option <option=value>
                          option for selected merge strategy

";

/// Print the usage block to stderr and return the exit-129 sentinel used by the
/// argument parser in upstream `git`.
fn usage_error() -> GitError {
    eprint!("{MERGE_TREE_USAGE}");
    GitError::Exit(129)
}

/// `git merge-tree` entry point.
pub(crate) fn cmd_merge_tree(
    cli_session: &crate::session::CliSession,
    args: &[String],
) -> Result<()> {
    let options = parse_merge_tree_args(args)?;
    reject_trivial_incompatible_options(&options)?;
    if options.stdin {
        return run_stdin_merges(cli_session, &options);
    }
    let mode = resolve_merge_tree_mode(&options)?;
    match mode {
        MergeTreeMode::RealMerge => run_real_merge(cli_session, &options),
        MergeTreeMode::TrivialMerge => run_trivial_merge(cli_session, &options),
        MergeTreeMode::Auto => Err(usage_error()),
    }
}

fn reject_trivial_incompatible_options(options: &MergeTreeOptions) -> Result<()> {
    if options.mode == MergeTreeMode::TrivialMerge && options.trivial_incompatible_option {
        eprintln!("fatal: --trivial-merge is incompatible with all other options");
        return Err(GitError::Exit(128));
    }
    Ok(())
}

/// Parse the command-line arguments into a [`MergeTreeOptions`]. Mirrors upstream
/// option handling, including `--stdin` being unsupported here (it would imply a
/// multi-merge protocol we do not implement).
fn parse_merge_tree_args(args: &[String]) -> Result<MergeTreeOptions> {
    let mut options = MergeTreeOptions::default();
    let mut iter = args.iter();
    let mut positional_only = false;
    while let Some(arg) = iter.next() {
        if positional_only {
            options.positionals.push(arg.clone());
            continue;
        }
        match arg.as_str() {
            "--" | "--end-of-options" => {
                positional_only = true;
                options.trivial_incompatible_option = true;
            }
            "--write-tree" => options.mode = MergeTreeMode::RealMerge,
            "--trivial-merge" => options.mode = MergeTreeMode::TrivialMerge,
            "-z" => {
                options.nul = true;
                options.trivial_incompatible_option = true;
            }
            "--name-only" => {
                options.name_only = true;
                options.trivial_incompatible_option = true;
            }
            "--messages" => {
                options.messages = Some(true);
                options.trivial_incompatible_option = true;
            }
            "--no-messages" => {
                options.messages = Some(false);
                options.trivial_incompatible_option = true;
            }
            "--quiet" => {
                options.quiet = true;
                options.trivial_incompatible_option = true;
            }
            "--no-quiet" => {
                options.quiet = false;
                options.trivial_incompatible_option = true;
            }
            "--allow-unrelated-histories" => {
                options.allow_unrelated_histories = true;
                options.trivial_incompatible_option = true;
            }
            "--no-allow-unrelated-histories" => {
                options.allow_unrelated_histories = false;
                options.trivial_incompatible_option = true;
            }
            "--stdin" => {
                options.stdin = true;
                options.trivial_incompatible_option = true;
            }
            "--merge-base" => {
                let value = iter.next().ok_or_else(|| {
                    GitError::Command("merge-tree --merge-base requires a value".into())
                })?;
                options.merge_base = Some(value.clone());
                options.trivial_incompatible_option = true;
            }
            "--no-merge-base" => {
                options.merge_base = None;
                options.trivial_incompatible_option = true;
            }
            "-X" | "--strategy-option" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("merge-tree -X requires a value".into()))?;
                options.strategy_options.push(value.clone());
                options.trivial_incompatible_option = true;
            }
            "--no-strategy-option" => {
                options.strategy_options.clear();
                options.trivial_incompatible_option = true;
            }
            value if value.starts_with("--merge-base=") => {
                options.merge_base = value.strip_prefix("--merge-base=").map(str::to_string);
                options.trivial_incompatible_option = true;
            }
            value if value.starts_with("--strategy-option=") => {
                if let Some(value) = value.strip_prefix("--strategy-option=") {
                    options.strategy_options.push(value.to_string());
                }
                options.trivial_incompatible_option = true;
            }
            value if value.starts_with("-X") && value.len() > 2 => {
                options.strategy_options.push(value[2..].to_string());
                options.trivial_incompatible_option = true;
            }
            value if value.starts_with('-') && value != "-" => {
                if let Some(name) = value.strip_prefix("--") {
                    eprintln!("error: unknown option `{name}'");
                } else {
                    eprintln!("error: unknown switch `{}`", &value[1..]);
                }
                return Err(usage_error());
            }
            value => options.positionals.push(value.to_string()),
        }
    }
    Ok(options)
}

/// Resolve the effective mode, applying upstream's inference rules and rejecting
/// invalid argument counts for the selected mode.
fn resolve_merge_tree_mode(options: &MergeTreeOptions) -> Result<MergeTreeMode> {
    if options.stdin {
        if !options.positionals.is_empty() || options.merge_base.is_some() {
            return Err(usage_error());
        }
        return Ok(MergeTreeMode::RealMerge);
    }
    match options.mode {
        MergeTreeMode::RealMerge => {
            // `--merge-base` provides the base directly, so exactly two branches
            // are expected. Without it, also exactly two branches.
            if options.positionals.len() == 2 {
                Ok(MergeTreeMode::RealMerge)
            } else {
                Err(usage_error())
            }
        }
        MergeTreeMode::TrivialMerge => {
            // The trivial merge accepts no other options and needs base + 2 sides.
            if options.positionals.len() == 3
                && options.merge_base.is_none()
                && options.strategy_options.is_empty()
                && !options.nul
                && !options.name_only
                && options.messages.is_none()
            {
                Ok(MergeTreeMode::TrivialMerge)
            } else {
                Err(usage_error())
            }
        }
        MergeTreeMode::Auto => match options.positionals.len() {
            2 => Ok(MergeTreeMode::RealMerge),
            3 if options.merge_base.is_none()
                && options.strategy_options.is_empty()
                && !options.nul
                && !options.name_only
                && options.messages.is_none() =>
            {
                Ok(MergeTreeMode::TrivialMerge)
            }
            _ => Err(usage_error()),
        },
    }
}

// ===========================================================================
// Modern `--write-tree` real merge.
// ===========================================================================

/// One conflicted index entry (a higher-order stage) reported in the
/// "Conflicted file info" section.
struct ConflictedStage {
    mode: u32,
    oid: ObjectId,
    stage: u16,
    path: Vec<u8>,
}

/// One informational message, retaining both the free-form human string (used in
/// the default, newline-separated section) and the machine-stable `-z` form
/// (a path list, a stable conflict-type token, and the human message).
struct InfoMessage {
    /// Path used by merge-ort to sort this message. This is not always the
    /// first path in the machine-readable payload (notably rename/rename).
    sort_path: Vec<u8>,
    /// Stable order for multiple notices at the same logical path.
    order: u8,
    /// Paths/branches involved (used only in `-z` output).
    paths: Vec<Vec<u8>>,
    /// Stable short type, e.g. `Auto-merging` or `CONFLICT (contents)`.
    stable_type: String,
    /// Human-readable message, e.g. `Auto-merging foo` or
    /// `CONFLICT (content): Merge conflict in foo`.
    message: String,
}

/// Accumulated result of a 3-way merge ready to be rendered.
struct MergeOutcome {
    tree: ObjectId,
    /// Sorted higher-order stage entries for conflicted paths.
    conflicted: Vec<ConflictedStage>,
    /// Informational messages in upstream emission order.
    messages: Vec<InfoMessage>,
    /// Whether the merge had any conflict at all (drives the exit code and the
    /// default messages-on/off behaviour).
    clean: bool,
}

fn run_real_merge(
    cli_session: &crate::session::CliSession,
    options: &MergeTreeOptions,
) -> Result<()> {
    let _quiet_cleanup = if options.quiet {
        let git_dir = cli_session.git_dir()?;
        Some(QuietLooseObjectCleanup::new(git_dir)?)
    } else {
        None
    };
    let outcome = compute_real_merge(cli_session, options)?;

    if options.quiet {
        return if outcome.clean {
            Ok(())
        } else {
            Err(GitError::Exit(1))
        };
    }

    emit_real_merge(options, &outcome)?;
    if outcome.clean {
        Ok(())
    } else {
        Err(GitError::Exit(1))
    }
}

struct QuietLooseObjectCleanup {
    git_dir: PathBuf,
    before: BTreeSet<PathBuf>,
}

impl QuietLooseObjectCleanup {
    fn new(git_dir: PathBuf) -> Result<Self> {
        let before = loose_object_files(&git_dir)?;
        Ok(Self { git_dir, before })
    }
}

impl Drop for QuietLooseObjectCleanup {
    fn drop(&mut self) {
        let Ok(after) = loose_object_files(&self.git_dir) else {
            return;
        };
        for path in after {
            if self.before.contains(&path) {
                continue;
            }
            let full = self.git_dir.join("objects").join(&path);
            let parent = full.parent().map(Path::to_path_buf);
            let _ = fs::remove_file(&full);
            if let Some(parent) = parent {
                let _ = fs::remove_dir(parent);
            }
        }
    }
}

fn loose_object_files(git_dir: &Path) -> Result<BTreeSet<PathBuf>> {
    let mut files = BTreeSet::new();
    let objects = git_dir.join("objects");
    let Ok(dirs) = fs::read_dir(&objects) else {
        return Ok(files);
    };
    for dir in dirs {
        let dir = dir?;
        if !dir.file_type()?.is_dir() {
            continue;
        }
        let dir_name = dir.file_name();
        let dir_name = dir_name.to_string_lossy();
        if dir_name.len() != 2 || !dir_name.as_bytes().iter().all(u8::is_ascii_hexdigit) {
            continue;
        }
        for entry in fs::read_dir(dir.path())? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let file_name = entry.file_name();
            let file_name = file_name.to_string_lossy();
            if file_name.len() == 38 && file_name.as_bytes().iter().all(u8::is_ascii_hexdigit) {
                files.insert(PathBuf::from(dir_name.as_ref()).join(file_name.as_ref()));
            }
        }
    }
    Ok(files)
}

fn compute_real_merge(
    cli_session: &crate::session::CliSession,
    options: &MergeTreeOptions,
) -> Result<MergeOutcome> {
    let cwd = cli_session.cwd().to_path_buf();
    let git_dir = cli_session.git_dir()?;
    let format = repository_object_format(&git_dir)?;
    let db = crate::repository::open_object_database(&git_dir, format)?;

    let branch1 = &options.positionals[0];
    let branch2 = &options.positionals[1];

    // Resolve the two sides plus the base tree. With an explicit --merge-base the
    // sides only need to be tree-ish; otherwise they must be commits so we can
    // compute the merge base from history.
    let (ours_tree, theirs_tree, base_tree) = match &options.merge_base {
        Some(base) => {
            let ours_tree = resolve_tree_ish(&git_dir, &db, format, branch1)?;
            let theirs_tree = resolve_tree_ish(&git_dir, &db, format, branch2)?;
            let base_tree = resolve_tree_ish(&git_dir, &db, format, base)?;
            (ours_tree, theirs_tree, Some(base_tree))
        }
        None => {
            let ours_commit = resolve_commit_ish(&git_dir, &db, format, branch1)?;
            let theirs_commit = resolve_commit_ish(&git_dir, &db, format, branch2)?;
            let bases = sley_rev::merge_bases(&git_dir, format, &db, &ours_commit, &theirs_commit)?;
            let base_tree = match bases.first() {
                Some(base) => Some(commit_tree_oid(&db, format, base)?),
                None => {
                    if !options.allow_unrelated_histories {
                        // This hard error is printed even under --quiet.
                        eprintln!("fatal: refusing to merge unrelated histories");
                        return Err(GitError::Exit(128));
                    }
                    None
                }
            };
            (
                commit_tree_oid(&db, format, &ours_commit)?,
                commit_tree_oid(&db, format, &theirs_commit)?,
                base_tree,
            )
        }
    };

    let strategy = parse_strategy_favor(&options.strategy_options)?;
    let detect_renames = merge_tree_detect_renames(&git_dir);
    let worktree_root = worktree_root_for_git_dir(&git_dir).ok();
    let explicit_attr_source = global_attr_source().or_else(|| env::var("GIT_ATTR_SOURCE").ok());
    let attr_source_tree = explicit_attr_source
        .as_deref()
        .map(|source| resolve_tree_ish(&git_dir, &db, format, source))
        .transpose()?;
    let worktree_attributes = if attr_source_tree.is_none() {
        worktree_root
            .as_ref()
            .and_then(|root| sley_worktree::StandardAttributeMatcher::from_worktree_root(root).ok())
    } else {
        None
    };
    let path_favor = |path: &[u8]| match merge_tree_merge_attribute(
        path,
        attr_source_tree.as_ref(),
        worktree_attributes.as_ref(),
        worktree_root.as_deref(),
        &git_dir,
        &db,
        format,
    ) {
        Some(sley_worktree::AttributeState::Value(value)) if value == b"union" => {
            sley_diff_merge::MergeFavor::Union
        }
        _ => sley_diff_merge::MergeFavor::None,
    };
    let path_is_binary = |path: &[u8]| {
        matches!(
            merge_tree_merge_attribute(
                path,
                attr_source_tree.as_ref(),
                worktree_attributes.as_ref(),
                worktree_root.as_deref(),
                &git_dir,
                &db,
                format,
            ),
            Some(sley_worktree::AttributeState::Unset)
        )
    };
    ensure_merge_tree_inputs_readable(&db, format, base_tree.as_ref(), &ours_tree, &theirs_tree)?;
    let mut write_db = db.clone();
    let merge = match sley_diff_merge::merge_trees(
        &mut write_db,
        format,
        base_tree.as_ref(),
        &ours_tree,
        &theirs_tree,
        &sley_diff_merge::MergeTreesOptions {
            ours_label: branch1,
            theirs_label: branch2,
            ancestor_label: "merged common ancestors",
            favor: strategy,
            path_favor: Some(&path_favor),
            path_marker_size: None,
            path_is_binary: Some(&path_is_binary),
            detect_renames,
            rename_threshold: sley_diff_merge::DEFAULT_RENAME_THRESHOLD,
            rename_limit: 0,
            directory_renames: sley_diff_merge::DirectoryRenames::Conflict,
            style: sley_diff_merge::ConflictStyle::Merge,
            ws_ignore: sley_diff_merge::WsIgnore::EMPTY,
        },
    ) {
        Ok(merge) => merge,
        Err(err) => return Err(merge_tree_merge_error(err)),
    };
    Ok(render_merge_outcome(&merge, branch1, branch2))
}

#[allow(clippy::too_many_arguments)]
fn merge_tree_merge_attribute(
    path: &[u8],
    attr_source_tree: Option<&ObjectId>,
    worktree_attributes: Option<&sley_worktree::StandardAttributeMatcher>,
    worktree_root: Option<&Path>,
    git_dir: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
) -> Option<sley_worktree::AttributeState> {
    let requested = [b"merge".to_vec()];
    let checks = if let (Some(tree), Some(root)) = (attr_source_tree, worktree_root) {
        sley_worktree::standard_attributes_for_path_from_tree(
            root, git_dir, db, format, tree, path, &requested, false,
        )
        .ok()?
    } else {
        worktree_attributes?.attributes_for_path(path, &requested, false)
    };
    checks.into_iter().next().and_then(|check| check.state)
}

fn ensure_merge_tree_inputs_readable(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    base_tree: Option<&ObjectId>,
    ours_tree: &ObjectId,
    theirs_tree: &ObjectId,
) -> Result<()> {
    for oid in [base_tree, Some(ours_tree), Some(theirs_tree)]
        .into_iter()
        .flatten()
    {
        if let Err(err) = db.read_object(oid) {
            if merge_tree_missing_oid(&err).is_some() {
                let base = match base_tree {
                    Some(base) => *base,
                    None => sley_core::object_id_for_bytes(format, "tree", b"")?,
                };
                eprintln!("error: Could not read {oid}");
                eprintln!(
                    "error: collecting merge info failed for trees {base}, {ours_tree}, {theirs_tree}"
                );
                eprintln!("fatal: failure to merge");
                return Err(GitError::Exit(128));
            }
            return Err(err);
        }
    }
    Ok(())
}

fn merge_tree_merge_error(err: GitError) -> GitError {
    if let Some(oid) = merge_tree_missing_oid(&err) {
        eprintln!("error: unable to read blob object {oid}");
        return GitError::Exit(128);
    }
    err
}

fn merge_tree_missing_oid(err: &GitError) -> Option<ObjectId> {
    match err {
        GitError::NotFound(kind) => kind.object_id(),
        _ => None,
    }
}

fn merge_tree_detect_renames(git_dir: &Path) -> bool {
    let params = effective_config_parameters_env();
    let Ok(config) = sley_config::read_repo_config(git_dir, params.as_deref()) else {
        return true;
    };
    config.get_bool("diff", None, "renames") != Some(false)
}

fn run_stdin_merges(
    cli_session: &crate::session::CliSession,
    options: &MergeTreeOptions,
) -> Result<()> {
    if options.merge_base.is_some() {
        eprintln!("fatal: --merge-base and --stdin cannot be used together");
        return Err(GitError::Exit(128));
    }
    if !options.positionals.is_empty() {
        return Err(usage_error());
    }

    let mut input = Vec::new();
    io::stdin().read_to_end(&mut input)?;
    let records: Vec<&[u8]> = if options.nul {
        input.split(|b| *b == b'\0').collect()
    } else {
        input.split(|b| *b == b'\n').collect()
    };

    let stdout = io::stdout();
    let mut out = stdout.lock();
    for record in records {
        if record.is_empty() {
            continue;
        }
        let Some(mut batch) = stdin_record_options(options, record)? else {
            continue;
        };
        batch.mode = MergeTreeMode::RealMerge;
        batch.nul = true;
        batch.quiet = false;
        batch.stdin = false;

        let outcome = compute_real_merge(cli_session, &batch)?;
        out.write_all(if outcome.clean { b"1\0" } else { b"0\0" })?;
        emit_real_merge_to(&mut out, &batch, &outcome)?;
        out.write_all(b"\0")?;
    }
    out.flush()?;
    Ok(())
}

fn stdin_record_options(
    options: &MergeTreeOptions,
    record: &[u8],
) -> Result<Option<MergeTreeOptions>> {
    let text = std::str::from_utf8(record)
        .map_err(|_| GitError::Command("merge-tree --stdin input is not UTF-8".into()))?;
    let tokens: Vec<&str> = text.split_whitespace().collect();
    if tokens.is_empty() {
        return Ok(None);
    }

    let mut batch = MergeTreeOptions {
        mode: MergeTreeMode::RealMerge,
        nul: true,
        name_only: options.name_only,
        messages: options.messages,
        quiet: false,
        stdin: false,
        allow_unrelated_histories: options.allow_unrelated_histories,
        merge_base: None,
        strategy_options: options.strategy_options.clone(),
        positionals: Vec::new(),
        trivial_incompatible_option: false,
    };

    match tokens.as_slice() {
        [left, right] => {
            batch.positionals.push((*left).to_string());
            batch.positionals.push((*right).to_string());
        }
        [base, "--", left, right] => {
            batch.merge_base = Some((*base).to_string());
            batch.positionals.push((*left).to_string());
            batch.positionals.push((*right).to_string());
        }
        _ => {
            eprintln!(
                "fatal: malformed input line: {}",
                String::from_utf8_lossy(record)
            );
            return Err(GitError::Exit(128));
        }
    }
    Ok(Some(batch))
}

/// Interpret recognised `-X` strategy options. Only the conflict-resolution
/// favouring options affect merge-tree output; everything else is ignored, as
/// upstream tolerates (and largely ignores) most strategy options here.
fn parse_strategy_favor(options: &[String]) -> Result<sley_diff_merge::MergeFavor> {
    use sley::plumbing::sley_diff_merge::MergeFavor;
    let mut favor = MergeFavor::None;
    for option in options {
        match option.as_str() {
            "ours" => favor = MergeFavor::Ours,
            "theirs" => favor = MergeFavor::Theirs,
            // Whitespace / diff-algorithm knobs do not change which bytes win for
            // the cases we model; accept and ignore them.
            "ignore-space-change"
            | "ignore-all-space"
            | "ignore-space-at-eol"
            | "ignore-cr-at-eol"
            | "renormalize"
            | "no-renormalize"
            | "find-renames"
            | "no-renames" => {}
            other => {
                if other.starts_with("find-renames=")
                    || other.starts_with("rename-threshold=")
                    || other.starts_with("diff-algorithm=")
                    || other.starts_with("subtree")
                {
                    continue;
                }
                return Err(GitError::Unsupported(format!(
                    "merge-tree strategy option {other} is not supported yet"
                )));
            }
        }
    }
    Ok(favor)
}

/// Resolve `rev` to a commit object id, peeling tags. On failure, print the same
/// pair of error lines upstream `git merge-tree` prints and return exit 1.
fn resolve_commit_ish(
    git_dir: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    rev: &str,
) -> Result<ObjectId> {
    let oid = match resolve_revision(git_dir, format, rev) {
        Ok(oid) => oid,
        Err(_) => return Err(not_something_we_can_merge(rev)),
    };
    match sley_rev::peel_to_commit(db, format, &oid) {
        Ok(commit) => Ok(commit),
        Err(_) => {
            eprintln!(
                "error: {rev}: expected commit type, but the object dereferences to {} type",
                dereferenced_type(db, &oid)
            );
            Err(not_something_we_can_merge(rev))
        }
    }
}

/// Resolve `rev` to a tree object id, peeling commits/tags as needed.
fn resolve_tree_ish(
    git_dir: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    rev: &str,
) -> Result<ObjectId> {
    let oid = match resolve_revision(git_dir, format, rev) {
        Ok(oid) => oid,
        Err(_) => return Err(not_something_we_can_merge(rev)),
    };
    match sley_rev::peel_to_tree(db, format, &oid) {
        Ok(tree) => Ok(tree),
        Err(_) => Err(not_something_we_can_merge(rev)),
    }
}

/// Best-effort name of the type an object dereferences to, for the
/// "expected commit type" diagnostic.
fn dereferenced_type(db: &FileObjectDatabase, oid: &ObjectId) -> &'static str {
    match db.read_object(oid) {
        Ok(object) => object.object_type.as_str(),
        Err(_) => "unknown",
    }
}

/// Emit the standard "<rev> - not something we can merge" error and the exit-1
/// sentinel.
fn not_something_we_can_merge(rev: &str) -> GitError {
    eprintln!("merge-tree: {rev} - not something we can merge");
    GitError::Exit(1)
}

/// Render a [`sley_diff_merge::MergeTreesResult`] into the `merge-tree
/// --write-tree` [`MergeOutcome`] (tree + sorted conflict stages + ordered
/// messages). The library computes the merge; this function is purely the
/// merge-tree-specific *presentation* (message text, message ordering, stage
/// sorting), kept byte-identical to the historical inline implementation.
fn render_merge_outcome(
    merge: &sley_diff_merge::MergeTreesResult,
    ours_label: &str,
    theirs_label: &str,
) -> MergeOutcome {
    let mut conflicted: Vec<ConflictedStage> = Vec::new();
    // Upstream's `display_messages` sorts the per-path message lists by path and
    // prints each path's messages in insertion order ("Auto-merging" before any
    // "CONFLICT" for the same path) — NOT all auto lines then all conflict lines.
    // We replicate that by emitting into one list in path order, auto-then-
    // conflict per path. `merge.paths` is path-sorted except for the `path~side`
    // leaves a distinct-types/dir-file rename inserts mid-iteration, so sort a
    // view of it first.
    let mut messages: Vec<InfoMessage> = Vec::new();
    let mut ordered: Vec<&sley_diff_merge::MergedPath> = merge.paths.iter().collect();
    ordered.sort_by(|left, right| left.path.cmp(&right.path));

    for entry in ordered {
        let path = &entry.path;
        if entry.auto_merged {
            messages.push(InfoMessage {
                sort_path: path.clone(),
                order: 10,
                paths: vec![path.clone()],
                stable_type: "Auto-merging".to_string(),
                message: format!("Auto-merging {}", String::from_utf8_lossy(path)),
            });
        }
        let Some(kind) = &entry.conflict else {
            continue;
        };
        match kind {
            sley_diff_merge::MergeConflictKind::Content { add_add } => {
                let conflict_kind = if *add_add { "add/add" } else { "content" };
                messages.push(InfoMessage {
                    sort_path: path.clone(),
                    order: 20,
                    paths: vec![path.clone()],
                    stable_type: "CONFLICT (contents)".to_string(),
                    message: format!(
                        "CONFLICT ({conflict_kind}): Merge conflict in {}",
                        String::from_utf8_lossy(path)
                    ),
                });
            }
            sley_diff_merge::MergeConflictKind::RenameContent { .. } => {
                messages.push(InfoMessage {
                    sort_path: path.clone(),
                    order: 20,
                    paths: vec![path.clone()],
                    stable_type: "CONFLICT (contents)".to_string(),
                    message: format!(
                        "CONFLICT (content): Merge conflict in {}",
                        String::from_utf8_lossy(path)
                    ),
                });
            }
            sley_diff_merge::MergeConflictKind::RenameRenameTwoToOne {
                ours_path,
                theirs_path,
            } => {
                messages.push(InfoMessage {
                    sort_path: path.clone(),
                    order: 20,
                    paths: vec![ours_path.clone(), theirs_path.clone(), path.clone()],
                    stable_type: "CONFLICT (rename/rename)".to_string(),
                    message: format!(
                        "CONFLICT (rename/rename): {ours} and {theirs} renamed to {path}, respectively.",
                        ours = String::from_utf8_lossy(ours_path),
                        theirs = String::from_utf8_lossy(theirs_path),
                        path = String::from_utf8_lossy(path),
                    ),
                });
            }
            sley_diff_merge::MergeConflictKind::RenameRenameOneToTwo {
                old_path,
                ours_path,
                theirs_path,
                ours_label,
                theirs_label,
            } => {
                messages.push(InfoMessage {
                    sort_path: old_path.clone(),
                    order: 20,
                    paths: vec![old_path.clone(), ours_path.clone(), theirs_path.clone()],
                    stable_type: "CONFLICT (rename/rename)".to_string(),
                    message: format!(
                        "CONFLICT (rename/rename): {old} renamed to {ours} in {ours_label} and to {theirs} in {theirs_label}.",
                        old = String::from_utf8_lossy(old_path),
                        ours = String::from_utf8_lossy(ours_path),
                        theirs = String::from_utf8_lossy(theirs_path),
                    ),
                });
            }
            sley_diff_merge::MergeConflictKind::RenameRenameOneToTwoStage => {}
            sley_diff_merge::MergeConflictKind::DirRenameSplit { source_dir } => {
                messages.push(InfoMessage {
                    sort_path: source_dir.clone(),
                    order: 20,
                    paths: vec![source_dir.clone()],
                    stable_type: "CONFLICT (directory rename split)".to_string(),
                    message: format!(
                        "CONFLICT (directory rename split): Unclear where to rename {source} to; it was renamed to multiple other directories, with no destination getting a majority of the files.",
                        source = String::from_utf8_lossy(source_dir),
                    ),
                });
            }
            sley_diff_merge::MergeConflictKind::ModifyDelete {
                deleted_in,
                modified_in,
            } => {
                messages.push(InfoMessage {
                    sort_path: path.clone(),
                    order: 20,
                    paths: vec![path.clone()],
                    stable_type: "CONFLICT (modify/delete)".to_string(),
                    message: format!(
                        "CONFLICT (modify/delete): {path} deleted in {deleted_in} and modified in {modified_in}.  Version {modified_in} of {path} left in tree.",
                        path = String::from_utf8_lossy(path),
                    ),
                });
            }
            sley_diff_merge::MergeConflictKind::RenameDelete {
                old_path,
                renamed_in,
                deleted_in,
            } => {
                messages.push(InfoMessage {
                    sort_path: path.clone(),
                    order: 20,
                    paths: vec![path.clone(), old_path.clone()],
                    stable_type: "CONFLICT (rename/delete)".to_string(),
                    message: format!(
                        "CONFLICT (rename/delete): {old} renamed to {new} in {renamed_in}, but deleted in {deleted_in}.",
                        old = String::from_utf8_lossy(old_path),
                        new = String::from_utf8_lossy(path),
                    ),
                });
            }
            sley_diff_merge::MergeConflictKind::FileDirectory {
                original_path,
                moved_from,
            } => {
                messages.push(InfoMessage {
                    sort_path: path.clone(),
                    order: 20,
                    paths: vec![path.clone(), original_path.clone()],
                    stable_type: "CONFLICT (file/directory)".to_string(),
                    message: format!(
                        "CONFLICT (file/directory): directory in the way of {old} from {moved_from}; moving it to {new} instead.",
                        old = String::from_utf8_lossy(original_path),
                        new = String::from_utf8_lossy(path),
                    ),
                });
            }
            sley_diff_merge::MergeConflictKind::DirRenameLocation {
                old_path,
                renamed_from,
                added_in,
                dir_renamed_in,
                back_to_self: _,
            } => {
                let new_path = String::from_utf8_lossy(path);
                let message = match renamed_from {
                    Some(source) => format!(
                        "CONFLICT (file location): {src} renamed to {old} in {added_in}, inside a directory that was renamed in {dir_renamed_in}, suggesting it should perhaps be moved to {new_path}.",
                        src = String::from_utf8_lossy(source),
                        old = String::from_utf8_lossy(old_path),
                    ),
                    None => format!(
                        "CONFLICT (file location): {old} added in {added_in} inside a directory that was renamed in {dir_renamed_in}, suggesting it should perhaps be moved to {new_path}.",
                        old = String::from_utf8_lossy(old_path),
                    ),
                };
                messages.push(InfoMessage {
                    sort_path: path.clone(),
                    order: 0,
                    paths: vec![path.clone(), old_path.clone()],
                    stable_type: "CONFLICT (directory rename suggested)".to_string(),
                    message,
                });
            }
            sley_diff_merge::MergeConflictKind::DirRenameImplicitCollision { sources } => {
                let source_list = sources
                    .iter()
                    .map(|s| String::from_utf8_lossy(s).into_owned())
                    .collect::<Vec<_>>()
                    .join(", ");
                messages.push(InfoMessage {
                    sort_path: path.clone(),
                    order: 20,
                    paths: vec![path.clone()],
                    stable_type: "CONFLICT (implicit dir rename)".to_string(),
                    message: format!(
                        "CONFLICT (implicit dir rename): Existing file/dir at {new} in the way of implicit directory rename(s) putting the following path(s) there: {source_list}.",
                        new = String::from_utf8_lossy(path),
                    ),
                });
            }
            sley_diff_merge::MergeConflictKind::DistinctTypes {
                original_path,
                ours_renamed,
                theirs_renamed,
            } => {
                let mut msg_paths = vec![original_path.clone()];
                msg_paths.extend(ours_renamed.clone());
                msg_paths.extend(theirs_renamed.clone());
                let renamed_both = ours_renamed.is_some() && theirs_renamed.is_some();
                let which = if renamed_both { "both" } else { "one" };
                messages.push(InfoMessage {
                    sort_path: original_path.clone(),
                    order: 20,
                    paths: msg_paths,
                    // Upstream's `type_short_descriptions[CONFLICT_DISTINCT_MODES]`
                    // is "CONFLICT (distinct modes)" even though the human line
                    // reads "distinct types".
                    stable_type: "CONFLICT (distinct modes)".to_string(),
                    message: format!(
                        "CONFLICT (distinct types): {orig} had different types on each side; renamed {which} of them so each can be recorded somewhere.",
                        orig = String::from_utf8_lossy(original_path),
                    ),
                });
            }
            sley_diff_merge::MergeConflictKind::DistinctTypesStage => {}
        }
        push_conflicted_stages(&mut conflicted, path, &entry.stages);
    }
    for event in &merge.info_messages {
        match event {
            sley_diff_merge::MergeInfoMessage::AutoMerge { path } => {
                messages.push(InfoMessage {
                    sort_path: path.clone(),
                    order: 10,
                    paths: vec![path.clone()],
                    stable_type: "Auto-merging".to_string(),
                    message: format!("Auto-merging {}", String::from_utf8_lossy(path)),
                });
            }
            sley_diff_merge::MergeInfoMessage::DirRenameLocationConflict {
                old_path,
                new_path,
                renamed_from,
                added_in,
                dir_renamed_in,
            } => {
                let message = match renamed_from {
                    Some(source) => format!(
                        "CONFLICT (file location): {source} renamed to {old} in {added_in}, inside a directory that was renamed in {dir_renamed_in}, suggesting it should perhaps be moved to {new}.",
                        source = String::from_utf8_lossy(source),
                        old = String::from_utf8_lossy(old_path),
                        new = String::from_utf8_lossy(new_path),
                    ),
                    None => format!(
                        "CONFLICT (file location): {old} added in {added_in} inside a directory that was renamed in {dir_renamed_in}, suggesting it should perhaps be moved to {new}.",
                        old = String::from_utf8_lossy(old_path),
                        new = String::from_utf8_lossy(new_path),
                    ),
                };
                messages.push(InfoMessage {
                    sort_path: new_path.clone(),
                    order: 0,
                    paths: vec![new_path.clone(), old_path.clone()],
                    stable_type: "CONFLICT (directory rename suggested)".to_string(),
                    message,
                });
            }
            sley_diff_merge::MergeInfoMessage::RenameDeleteConflict {
                old_path,
                new_path,
                renamed_in,
                deleted_in,
            } => {
                messages.push(InfoMessage {
                    sort_path: new_path.clone(),
                    order: 1,
                    paths: vec![new_path.clone(), old_path.clone()],
                    stable_type: "CONFLICT (rename/delete)".to_string(),
                    message: format!(
                        "CONFLICT (rename/delete): {old} renamed to {new} in {renamed_in}, but deleted in {deleted_in}.",
                        old = String::from_utf8_lossy(old_path),
                        new = String::from_utf8_lossy(new_path),
                    ),
                });
            }
            sley_diff_merge::MergeInfoMessage::ModifyDeleteConflict {
                path,
                deleted_in,
                modified_in,
            } => {
                messages.push(InfoMessage {
                    sort_path: path.clone(),
                    order: 30,
                    paths: vec![path.clone()],
                    stable_type: "CONFLICT (modify/delete)".to_string(),
                    message: format!(
                        "CONFLICT (modify/delete): {path} deleted in {deleted_in} and modified in {modified_in}.  Version {modified_in} of {path} left in tree.",
                        path = String::from_utf8_lossy(path),
                    ),
                });
            }
            // These are merge-porcelain progress messages, not merge-tree's
            // structured conflict records. The directory-location conflict
            // variant above carries the merge-tree-applicable form.
            sley_diff_merge::MergeInfoMessage::DirRenameSkippedDueToRerename { .. }
            | sley_diff_merge::MergeInfoMessage::DirRenameApplied { .. } => {}
        }
    }
    messages.sort_by(|left, right| {
        left.sort_path
            .cmp(&right.sort_path)
            .then_with(|| left.order.cmp(&right.order))
            .then_with(|| left.paths.cmp(&right.paths))
    });
    let _ = (ours_label, theirs_label);

    conflicted.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.stage.cmp(&right.stage))
    });

    MergeOutcome {
        tree: merge.tree,
        conflicted,
        messages,
        clean: merge.clean,
    }
}

/// Append the present higher-order stages (1=base, 2=ours, 3=theirs) for a
/// conflicted `path` to `out`.
fn push_conflicted_stages(
    out: &mut Vec<ConflictedStage>,
    path: &[u8],
    stages: &sley_diff_merge::MergeStages,
) {
    for (stage, entry) in [(1u16, &stages.base), (2, &stages.ours), (3, &stages.theirs)] {
        if let Some((mode, oid)) = entry {
            out.push(ConflictedStage {
                mode: *mode,
                oid: *oid,
                stage,
                path: path.to_vec(),
            });
        }
    }
}

/// Render a [`MergeOutcome`] to stdout in the modern output format.
fn emit_real_merge(options: &MergeTreeOptions, outcome: &MergeOutcome) -> Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    emit_real_merge_to(&mut out, options, outcome)?;
    out.flush()?;
    Ok(())
}

fn emit_real_merge_to(
    out: &mut impl Write,
    options: &MergeTreeOptions,
    outcome: &MergeOutcome,
) -> Result<()> {
    let oid_terminator: &[u8] = if options.nul { b"\0" } else { b"\n" };
    out.write_all(outcome.tree.to_hex().as_bytes())?;
    out.write_all(oid_terminator)?;

    if outcome.clean {
        // Conflicted-file-info section is empty on a clean merge.
        emit_messages(out, options, outcome, /* clean */ true)?;
        return Ok(());
    }

    // Conflicted file info.
    if options.name_only {
        let mut seen: BTreeSet<&[u8]> = BTreeSet::new();
        for entry in &outcome.conflicted {
            if seen.insert(entry.path.as_slice()) {
                write_conflicted_path(out, options.nul, &entry.path)?;
            }
        }
    } else {
        for entry in &outcome.conflicted {
            // merge-tree always prints the full-length oid here (no abbreviation).
            let prefix = format!(
                "{:06o} {} {}\t",
                entry.mode,
                entry.oid.to_hex(),
                entry.stage
            );
            out.write_all(prefix.as_bytes())?;
            write_conflicted_path(out, options.nul, &entry.path)?;
        }
    }

    emit_messages(out, options, outcome, /* clean */ false)?;
    Ok(())
}

/// Write a single conflicted-file-info path, NUL- or newline-terminated. In the
/// default (non-`-z`) mode the path is quoted per `core.quotePath`.
fn write_conflicted_path(out: &mut impl Write, nul: bool, path: &[u8]) -> Result<()> {
    if nul {
        out.write_all(path)?;
        out.write_all(b"\0")?;
    } else {
        let quoted = status_quote_path(path, false);
        out.write_all(quoted.as_bytes())?;
        out.write_all(b"\n")?;
    }
    Ok(())
}

/// Emit the informational-messages section, honouring `--[no-]messages`, `-z`,
/// and the default (messages only when conflicted).
fn emit_messages(
    out: &mut impl Write,
    options: &MergeTreeOptions,
    outcome: &MergeOutcome,
    clean: bool,
) -> Result<()> {
    let show = options.messages.unwrap_or(!clean);
    if !show {
        return Ok(());
    }

    // The messages section is always emitted when shown, even when it carries no
    // records: in that case it is just its single leading separator (a blank line
    // for the default format, or a NUL for `-z`).
    if options.nul {
        // The section "begins with a NUL", then zero or more records of the form
        // <n>\0<path1>\0..<pathN>\0<stable-type>\0<message>\n\0
        out.write_all(b"\0")?;
        for message in &outcome.messages {
            out.write_all(message.paths.len().to_string().as_bytes())?;
            out.write_all(b"\0")?;
            for path in &message.paths {
                out.write_all(path)?;
                out.write_all(b"\0")?;
            }
            out.write_all(message.stable_type.as_bytes())?;
            out.write_all(b"\0")?;
            out.write_all(message.message.as_bytes())?;
            out.write_all(b"\n")?;
            out.write_all(b"\0")?;
        }
    } else {
        // A blank line separates the section, then one human message per line.
        out.write_all(b"\n")?;
        for message in &outcome.messages {
            out.write_all(message.message.as_bytes())?;
            out.write_all(b"\n")?;
        }
    }
    Ok(())
}

// ===========================================================================
// Deprecated `--trivial-merge`.
// ===========================================================================

/// One side's view of a path in the trivial merge: present (mode, oid) or absent.
type TrivialEntry = Option<(u32, ObjectId)>;

fn run_trivial_merge(
    cli_session: &crate::session::CliSession,
    options: &MergeTreeOptions,
) -> Result<()> {
    let git_dir = cli_session.git_dir()?;
    let format = repository_object_format(&git_dir)?;
    let db = crate::repository::open_object_database(&git_dir, format)?;

    let base_tree = resolve_tree_ish(&git_dir, &db, format, &options.positionals[0])?;
    let ours_tree = resolve_tree_ish(&git_dir, &db, format, &options.positionals[1])?;
    let theirs_tree = resolve_tree_ish(&git_dir, &db, format, &options.positionals[2])?;

    let base_map = sley_diff_merge::flatten_tree(&db, format, &base_tree)?;
    let ours_map = sley_diff_merge::flatten_tree(&db, format, &ours_tree)?;
    let theirs_map = sley_diff_merge::flatten_tree(&db, format, &theirs_tree)?;

    let mut all_paths = BTreeSet::new();
    all_paths.extend(base_map.keys().cloned());
    all_paths.extend(ours_map.keys().cloned());
    all_paths.extend(theirs_map.keys().cloned());
    let mut all_paths: Vec<Vec<u8>> = all_paths.into_iter().collect();
    all_paths.sort_by(|left, right| trivial_path_cmp(left, right));

    let stdout = io::stdout();
    let mut out = stdout.lock();

    for path in all_paths {
        let base: TrivialEntry = base_map.get(&path).cloned();
        let ours: TrivialEntry = ours_map.get(&path).cloned();
        let theirs: TrivialEntry = theirs_map.get(&path).cloned();

        // The trivial merge omits any path whose merged result equals <branch1>
        // (ours): if neither side changed relative to ours, or only ours changed,
        // there is nothing to report.
        if ours == theirs || theirs == base {
            continue;
        }

        emit_trivial_path(&mut out, &db, &path, &base, &ours, &theirs)?;
    }

    out.flush()?;
    Ok(())
}

fn trivial_path_cmp(left: &[u8], right: &[u8]) -> std::cmp::Ordering {
    if right
        .strip_prefix(left)
        .is_some_and(|suffix| suffix.starts_with(b"/"))
    {
        return std::cmp::Ordering::Greater;
    }
    if left
        .strip_prefix(right)
        .is_some_and(|suffix| suffix.starts_with(b"/"))
    {
        return std::cmp::Ordering::Less;
    }
    left.cmp(right)
}

/// One stage line of a trivial-merge record.
struct TrivialStageLine {
    label: &'static str,
    mode: u32,
    oid: ObjectId,
}

/// Emit one path's trivial-merge record: a section header, the relevant stage
/// lines, and a unified diff of ours -> the merged result.
fn emit_trivial_path(
    out: &mut impl Write,
    db: &FileObjectDatabase,
    path: &[u8],
    base: &TrivialEntry,
    ours: &TrivialEntry,
    theirs: &TrivialEntry,
) -> Result<()> {
    let (header, lines, result_bytes) = trivial_resolution(db, base, ours, theirs)?;

    writeln!(out, "{header}")?;
    for line in &lines {
        writeln!(
            out,
            "  {:<6} {:06o} {} {}",
            line.label,
            line.mode,
            line.oid.to_hex(),
            String::from_utf8_lossy(path)
        )?;
    }

    // The diff is from ours (branch1) to the merged result.
    if let Some(result_bytes) = result_bytes {
        let ours_bytes = blob_bytes(db, ours)?;
        write_unified_hunks(out, &ours_bytes, &result_bytes)?;
    }
    Ok(())
}

/// Classify a path for the trivial merge, returning its section header, the stage
/// lines to print, and the resulting (merged) blob bytes used for the diff. This
/// mirrors `git merge-tree`'s deprecated trivial resolver, which only reports
/// paths whose result differs from `ours`.
fn trivial_resolution(
    db: &FileObjectDatabase,
    base: &TrivialEntry,
    ours: &TrivialEntry,
    theirs: &TrivialEntry,
) -> Result<(&'static str, Vec<TrivialStageLine>, Option<Vec<u8>>)> {
    // Helper to build a stage line from a present entry.
    let line = |label: &'static str, entry: &TrivialEntry| -> Option<TrivialStageLine> {
        entry.as_ref().map(|(mode, oid)| TrivialStageLine {
            label,
            mode: *mode,
            oid: *oid,
        })
    };

    match (base, ours, theirs) {
        // Removed on the remote side (and unchanged locally): result is removed.
        (Some(_), Some(_), None) => {
            let lines = [line("base", base), line("our", ours)]
                .into_iter()
                .flatten()
                .collect();
            Ok(("removed in remote", lines, Some(Vec::new())))
        }
        // Added only on the remote side: result is their version.
        (None, None, Some((_, their_oid))) => {
            let lines = [line("their", theirs)].into_iter().flatten().collect();
            Ok((
                "added in remote",
                lines,
                Some(merge_read_blob(db, their_oid)?),
            ))
        }
        // Both sides added the file (with differing content): a content merge with
        // `.our` / `.their` markers.
        (None, Some(_), Some(_)) => {
            let result = trivial_content_merge(db, &Vec::new(), ours, theirs)?;
            let lines = [line("our", ours), line("their", theirs)]
                .into_iter()
                .flatten()
                .collect();
            Ok(("added in both", lines, Some(result)))
        }
        (Some((_, base_oid)), our_entry, their_entry) => {
            let base_bytes = merge_read_blob(db, base_oid)?;
            match (our_entry, their_entry) {
                // Changed only on the remote side: auto-resolves to their version,
                // reported as a clean `merged` with a `result` line.
                (Some((our_mode, _)), Some((_, their_oid))) if ours == base => {
                    let their_bytes = merge_read_blob(db, their_oid)?;
                    let lines = vec![
                        TrivialStageLine {
                            label: "result",
                            mode: *our_mode,
                            oid: *their_oid,
                        },
                        line("our", ours).unwrap_or(TrivialStageLine {
                            label: "our",
                            mode: *our_mode,
                            oid: *their_oid,
                        }),
                    ];
                    Ok(("merged", lines, Some(their_bytes)))
                }
                // Changed on both sides: a content merge with `.our` / `.their`.
                (Some(_), Some(_)) => {
                    let result = trivial_content_merge(db, &base_bytes, ours, theirs)?;
                    let lines = [line("base", base), line("our", ours), line("their", theirs)]
                        .into_iter()
                        .flatten()
                        .collect();
                    Ok(("changed in both", lines, Some(result)))
                }
                // Removed locally but changed remotely.
                (None, Some(_)) => {
                    let lines = [line("base", base), line("their", theirs)]
                        .into_iter()
                        .flatten()
                        .collect();
                    Ok(("removed in local", lines, None))
                }
                // Any remaining shape resolves to ours and is filtered out before
                // reaching here; emit nothing meaningful.
                _ => Ok(("merged", Vec::new(), Some(blob_bytes(db, ours)?))),
            }
        }
        // Cases that resolve to `ours` (added locally only, or no entry at all)
        // are filtered out before reaching here.
        _ => Ok(("merged", Vec::new(), Some(blob_bytes(db, ours)?))),
    }
}

/// Run a 3-way content merge for the trivial mode, using git's `.our` / `.their`
/// conflict-marker labels.
fn trivial_content_merge(
    db: &FileObjectDatabase,
    base_bytes: &[u8],
    ours: &TrivialEntry,
    theirs: &TrivialEntry,
) -> Result<Vec<u8>> {
    let ours_bytes = blob_bytes(db, ours)?;
    let theirs_bytes = blob_bytes(db, theirs)?;
    let result = sley_diff_merge::merge_blobs(
        base_bytes,
        &ours_bytes,
        &theirs_bytes,
        &sley_diff_merge::MergeBlobOptions {
            ours_label: ".our",
            theirs_label: ".their",
            base_label: ".base",
            style: sley_diff_merge::ConflictStyle::Merge,
            favor: sley_diff_merge::MergeFavor::None,
            ws_ignore: sley_diff_merge::WsIgnore::EMPTY,
            marker_size: 7,
        },
    );
    Ok(result.content)
}

/// Read the blob bytes for a present entry, or the empty slice for an absent one.
fn blob_bytes(db: &FileObjectDatabase, entry: &TrivialEntry) -> Result<Vec<u8>> {
    match entry {
        Some((_, oid)) => merge_read_blob(db, oid),
        None => Ok(Vec::new()),
    }
}

/// Render a unified diff (grouped hunks with three lines of context) of
/// `old` -> `new`, using the shared Myers diff. This drives the diff portion of
/// the trivial-merge output.
fn write_unified_hunks(out: &mut impl Write, old: &[u8], new: &[u8]) -> Result<()> {
    let old_lines = sley_diff_merge::split_lines(old);
    let new_lines = sley_diff_merge::split_lines(new);
    let ops = sley_diff_merge::myers_diff_lines(&old_lines, &new_lines);

    // Flatten ops into a per-line tag stream: ' ', '-', or '+'.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Tag {
        Ctx,
        Del,
        Add,
    }
    struct Row<'a> {
        tag: Tag,
        line: &'a [u8],
    }
    // `DiffOp` is run-length encoded over consecutive lines; expand each run to a
    // per-line row, advancing independent cursors into `old`/`new`.
    let mut rows: Vec<Row<'_>> = Vec::new();
    let mut old_cursor = 0usize;
    let mut new_cursor = 0usize;
    for op in &ops {
        match op {
            sley_diff_merge::DiffOp::Equal(count) => {
                for _ in 0..*count {
                    rows.push(Row {
                        tag: Tag::Ctx,
                        line: old_lines[old_cursor].bytes_without_newline(),
                    });
                    old_cursor += 1;
                    new_cursor += 1;
                }
            }
            sley_diff_merge::DiffOp::Delete(count) => {
                for _ in 0..*count {
                    rows.push(Row {
                        tag: Tag::Del,
                        line: old_lines[old_cursor].bytes_without_newline(),
                    });
                    old_cursor += 1;
                }
            }
            sley_diff_merge::DiffOp::Insert(count) => {
                for _ in 0..*count {
                    rows.push(Row {
                        tag: Tag::Add,
                        line: new_lines[new_cursor].bytes_without_newline(),
                    });
                    new_cursor += 1;
                }
            }
        }
    }

    const CONTEXT: usize = 3;
    let mut idx = 0;
    while idx < rows.len() {
        if rows[idx].tag == Tag::Ctx {
            idx += 1;
            continue;
        }
        // Found a change; expand a hunk with surrounding context, merging
        // changes that are within 2*CONTEXT context lines of each other.
        let hunk_start = idx.saturating_sub(CONTEXT);
        let mut hunk_end = idx;
        loop {
            // Advance hunk_end past the current change run.
            while hunk_end < rows.len() && rows[hunk_end].tag != Tag::Ctx {
                hunk_end += 1;
            }
            // Look ahead: if another change starts within CONTEXT*2 context lines,
            // absorb the intervening context into this hunk.
            let mut lookahead = hunk_end;
            let mut ctx_run = 0;
            while lookahead < rows.len() && rows[lookahead].tag == Tag::Ctx && ctx_run < CONTEXT * 2
            {
                lookahead += 1;
                ctx_run += 1;
            }
            if lookahead < rows.len() && rows[lookahead].tag != Tag::Ctx && ctx_run <= CONTEXT {
                hunk_end = lookahead;
                continue;
            }
            break;
        }
        let hunk_end_with_ctx = (hunk_end + CONTEXT).min(rows.len());

        // Compute the old/new line ranges for the header.
        let mut old_start = 0usize;
        let mut new_start = 0usize;
        for row in &rows[..hunk_start] {
            match row.tag {
                Tag::Ctx => {
                    old_start += 1;
                    new_start += 1;
                }
                Tag::Del => old_start += 1,
                Tag::Add => new_start += 1,
            }
        }
        let mut old_count = 0usize;
        let mut new_count = 0usize;
        for row in &rows[hunk_start..hunk_end_with_ctx] {
            match row.tag {
                Tag::Ctx => {
                    old_count += 1;
                    new_count += 1;
                }
                Tag::Del => old_count += 1,
                Tag::Add => new_count += 1,
            }
        }

        writeln!(
            out,
            "@@ -{} +{} @@",
            hunk_range(old_start + 1, old_count),
            hunk_range(new_start + 1, new_count)
        )?;
        for row in &rows[hunk_start..hunk_end_with_ctx] {
            let marker = match row.tag {
                Tag::Ctx => b' ',
                Tag::Del => b'-',
                Tag::Add => b'+',
            };
            out.write_all(&[marker])?;
            out.write_all(row.line)?;
            out.write_all(b"\n")?;
        }

        idx = hunk_end_with_ctx;
    }
    Ok(())
}

/// Format one side of a unified hunk header (`start,count`, with git's
/// collapsing rules for empty and single-line ranges).
fn hunk_range(start: usize, count: usize) -> String {
    match count {
        0 => format!("{},0", start.saturating_sub(1)),
        1 => format!("{start}"),
        _ => format!("{start},{count}"),
    }
}
