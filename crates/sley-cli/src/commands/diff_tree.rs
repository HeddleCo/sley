//! `git diff-tree`: compare two tree-ish objects (or a commit against its
//! parent) and print the changed paths.
//!
//! This is the tree-vs-tree counterpart of `git diff`. The two commands share
//! almost all of their output machinery (raw `:mode mode oid oid STATUS` lines,
//! unified patches, `--stat`/`--numstat`/`--shortstat`/`--summary`, and
//! `--name-only`/`--name-status`), so this module reuses the crate-root helpers
//! that `cmd_diff` already relies on (`write_diff_raw_entry`,
//! `write_diff_patch_entry`, `write_diff_stat`, and friends) rather than
//! re-deriving them.
//!
//! The behaviours that are specific to `diff-tree` and therefore implemented
//! here are:
//!
//!   * Non-recursive output. Unlike `git diff`, the default `diff-tree` does not
//!     descend into changed subtrees: a modified directory is reported as a
//!     single `040000` entry (e.g. `M\tsub`). The recursive (`-r`) modes, and
//!     every file-content mode (`-p`, `--stat`, `--numstat`, `--shortstat`,
//!     `--summary`), implicitly descend and are produced via
//!     `sley_diff_merge::diff_name_status_trees_*`.
//!   * The commit-id header. When a single commit is given (or commits arrive on
//!     `--stdin`), git prints the commit's own object id on its own line before
//!     the diff, unless `--no-commit-id` is set.
//!   * Rename/copy detection is *off by default* (and `diff.renames` is ignored);
//!     it only runs when `-M`/`-C` is passed explicitly.
//!
//! A glob of the crate root brings every shared helper/type into scope via
//! descendant-privacy; see commands::stash for the rationale.
use crate::*;
use sley_object::TreeEntries;

/// Which family of output git should produce. `diff-tree` defaults to `Raw`
/// (the `:mode mode oid oid STATUS\tpath` form), which is *not* the default for
/// `git diff`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiffTreeOutput {
    Raw,
    Patch,
    Stat,
    Numstat,
    Shortstat,
    Summary,
    NameOnly,
    NameStatus,
    /// `-s`/`--no-patch`: compute the diff (for the exit code) but print nothing
    /// except, for a single commit, the commit-id header.
    Silent,
}

impl DiffTreeOutput {
    /// File-content output modes always operate at blob granularity, so they
    /// descend into changed subtrees regardless of `-r`.
    fn forces_recursion(self) -> bool {
        matches!(
            self,
            Self::Patch | Self::Stat | Self::Numstat | Self::Shortstat | Self::Summary
        )
    }
}

/// Parsed `diff-tree` invocation.
struct DiffTreeOptions {
    output: DiffTreeOutput,
    recursive: bool,
    /// `-t`: when recursing, also emit the intermediate tree (`040000`) entries.
    show_trees: bool,
    /// `--root`: for a single commit with no parent, diff against the empty tree
    /// instead of producing nothing.
    root: bool,
    /// `--no-commit-id`: suppress the per-commit object-id header line.
    no_commit_id: bool,
    /// `--stdin`: read tree-ish/commit specs (one diff request per line) from
    /// standard input instead of from the argument list.
    stdin: bool,
    z: bool,
    detect_renames: bool,
    detect_copies: bool,
    find_copies_harder: bool,
    rename_empty: bool,
    rename_threshold: u8,
    copy_threshold: u8,
    /// Raw-mode object-id abbreviation. `None` means full-length ids, matching
    /// git's `diff-tree` default (note this differs from `git diff`).
    raw_abbrev: Option<usize>,
    /// Patch/index-line abbreviation width.
    patch_abbrev: Option<usize>,
    patch_full_index: bool,
    src_prefix: String,
    dst_prefix: String,
    /// Positional tree-ish/commit arguments (everything that is not an option and
    /// not a trailing pathspec).
    revs: Vec<String>,
}

impl Default for DiffTreeOptions {
    fn default() -> Self {
        Self {
            output: DiffTreeOutput::Raw,
            recursive: false,
            show_trees: false,
            root: false,
            no_commit_id: false,
            stdin: false,
            z: false,
            detect_renames: false,
            detect_copies: false,
            find_copies_harder: false,
            rename_empty: true,
            rename_threshold: sley_diff_merge::DEFAULT_RENAME_THRESHOLD,
            copy_threshold: sley_diff_merge::DEFAULT_RENAME_THRESHOLD,
            raw_abbrev: None,
            patch_abbrev: None,
            patch_full_index: false,
            src_prefix: "a/".to_string(),
            dst_prefix: "b/".to_string(),
            revs: Vec::new(),
        }
    }
}

pub(crate) fn cmd_diff_tree(args: &[String]) -> Result<()> {
    let mut options = DiffTreeOptions::default();
    let mut pathspecs: Vec<String> = Vec::new();
    let mut positional_only = false;
    let mut idx = 0;
    while idx < args.len() {
        let arg = &args[idx];
        if positional_only {
            pathspecs.push(arg.clone());
            idx += 1;
            continue;
        }
        match arg.as_str() {
            "--" => positional_only = true,
            "-r" | "--recursive" => options.recursive = true,
            "-t" => {
                options.recursive = true;
                options.show_trees = true;
            }
            "--root" => options.root = true,
            "--no-commit-id" => options.no_commit_id = true,
            "--stdin" => options.stdin = true,
            "-z" => options.z = true,
            "-p" | "-u" | "--patch" => options.output = DiffTreeOutput::Patch,
            "--stat" => options.output = DiffTreeOutput::Stat,
            "--numstat" => options.output = DiffTreeOutput::Numstat,
            "--shortstat" => options.output = DiffTreeOutput::Shortstat,
            "--summary" => options.output = DiffTreeOutput::Summary,
            "--name-only" => options.output = DiffTreeOutput::NameOnly,
            "--name-status" => options.output = DiffTreeOutput::NameStatus,
            "-s" | "--no-patch" => options.output = DiffTreeOutput::Silent,
            "-a" | "--text" | "--no-ext-diff" | "--no-textconv" => {}
            // Rename / copy detection. diff-tree leaves these off unless asked.
            "-M" | "--find-renames" => options.detect_renames = true,
            "-C" | "--find-copies" => options.detect_copies = true,
            "--find-copies-harder" => {
                options.detect_copies = true;
                options.find_copies_harder = true;
            }
            "--no-find-copies-harder" => options.find_copies_harder = false,
            "--no-renames" => {
                options.detect_renames = false;
                options.detect_copies = false;
            }
            "--rename-empty" => options.rename_empty = true,
            "--no-rename-empty" => options.rename_empty = false,
            value if value.starts_with("-M") && value.len() > 2 => {
                log_validate_similarity_option(&value[2..], "find-renames")?;
                options.detect_renames = true;
                options.rename_threshold = parse_similarity_threshold(&value[2..]);
            }
            value if let Some(rest) = value.strip_prefix("--find-renames=") => {
                log_validate_similarity_option(rest, "find-renames")?;
                options.detect_renames = true;
                options.rename_threshold = parse_similarity_threshold(rest);
            }
            value if value.starts_with("-C") && value.len() > 2 => {
                log_validate_similarity_option(&value[2..], "find-copies")?;
                options.detect_copies = true;
                options.copy_threshold = parse_similarity_threshold(&value[2..]);
            }
            value if let Some(rest) = value.strip_prefix("--find-copies=") => {
                log_validate_similarity_option(rest, "find-copies")?;
                options.detect_copies = true;
                options.copy_threshold = parse_similarity_threshold(rest);
            }
            // Abbreviation controls. Raw mode shows full ids unless --abbrev is
            // given; --full-index forces full ids on patch index lines.
            "--abbrev" => {
                options.raw_abbrev = Some(7);
                options.patch_abbrev = Some(7);
            }
            "--no-abbrev" => {
                options.raw_abbrev = None;
                options.patch_abbrev = None;
            }
            value if let Some(rest) = value.strip_prefix("--abbrev=") => {
                let width = parse_abbrev(rest)?.max(4);
                options.raw_abbrev = Some(width);
                options.patch_abbrev = Some(width);
            }
            "--full-index" => options.patch_full_index = true,
            "--no-prefix" => {
                options.src_prefix.clear();
                options.dst_prefix.clear();
            }
            "--default-prefix" => {
                options.src_prefix = "a/".to_string();
                options.dst_prefix = "b/".to_string();
            }
            "--src-prefix" => {
                idx += 1;
                options.src_prefix = args
                    .get(idx)
                    .ok_or_else(|| GitError::Command("--src-prefix requires a value".into()))?
                    .clone();
            }
            value if let Some(rest) = value.strip_prefix("--src-prefix=") => {
                options.src_prefix = rest.to_string();
            }
            "--dst-prefix" => {
                idx += 1;
                options.dst_prefix = args
                    .get(idx)
                    .ok_or_else(|| GitError::Command("--dst-prefix requires a value".into()))?
                    .clone();
            }
            value if let Some(rest) = value.strip_prefix("--dst-prefix=") => {
                options.dst_prefix = rest.to_string();
            }
            // Combined-merge and pretty-printed log output are out of scope; be
            // explicit rather than emit something subtly wrong.
            "-c" | "--cc" | "--combined-all-paths" | "-m" => {
                return Err(GitError::Unsupported(
                    "diff-tree combined merge output is not supported".into(),
                ));
            }
            "--pretty" | "-v" => {
                return Err(GitError::Unsupported(
                    "diff-tree pretty/commit-log output is not supported".into(),
                ));
            }
            value if value.starts_with("--pretty=") || value.starts_with("--format=") => {
                return Err(GitError::Unsupported(
                    "diff-tree pretty/commit-log output is not supported".into(),
                ));
            }
            value if value.starts_with('-') && value != "-" => {
                return Err(GitError::Command(format!(
                    "unsupported diff-tree option {value}"
                )));
            }
            // First non-option token starts the positional (rev/pathspec) list.
            // A leading bare `-` is treated as a positional too.
            _ => {
                options.revs.push(arg.clone());
                // Any remaining tokens after we have collected the maximum of two
                // tree-ish operands are pathspecs. git treats trailing operands
                // that resolve to paths as pathspecs; we keep parsing options so
                // flags can still follow trees (git accepts e.g.
                // `diff-tree A B -- path`), and only `--` switches to pure
                // positional mode above.
            }
        }
        idx += 1;
    }

    // We do not implement pathspec filtering for diff-tree; reject it clearly so
    // we never silently ignore a path restriction.
    if !pathspecs.is_empty() || options.revs.len() > 2 {
        return Err(GitError::Unsupported(
            "diff-tree pathspec filtering is not supported".into(),
        ));
    }

    let repo = RepositoryContext::discover_current()?;
    let git_dir = repo.git_dir();
    let format = repo.format();
    let db = repo.objects();

    // Resolve the raw-mode abbreviation against core.abbrev only when the user
    // explicitly asked to abbreviate; otherwise diff-tree prints full ids.
    let repo_abbrev = repository_abbrev(git_dir, format)?;
    let raw_abbrev = options.raw_abbrev.map(|width| width.min(format.hex_len()));
    let patch_abbrev = if options.patch_full_index {
        format.hex_len()
    } else {
        options
            .patch_abbrev
            .or(repo_abbrev)
            .unwrap_or(7)
            .min(format.hex_len())
    };
    let request_context = DiffRequestContext {
        format,
        db,
        options: &options,
        raw_abbrev,
        patch_abbrev,
    };

    let mut has_differences = false;
    let mut stdout = io::stdout();

    if options.stdin {
        let mut input = String::new();
        io::stdin().read_to_string(&mut input)?;
        for line in input.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let request = parse_stdin_request(format, db, &options, line)?;
            if run_diff_request(&mut stdout, &request_context, &request)? {
                has_differences = true;
            }
        }
    } else {
        if options.revs.is_empty() {
            print_diff_tree_usage();
            return Err(GitError::Exit(129));
        }
        let request = resolve_arg_request(&repo, db, &options, &options.revs)?;
        if run_diff_request(&mut stdout, &request_context, &request)? {
            has_differences = true;
        }
    }

    let _ = has_differences;
    Ok(())
}

/// One resolved diff request: a left tree, a right tree, and an optional commit
/// id to print as the header (present only when the request came from a single
/// commit / a commit on `--stdin`).
struct DiffRequest {
    /// Left/old tree. `None` selects the empty tree (root-commit-style add diff).
    left: Option<ObjectId>,
    /// Right/new tree. `None` only on skipped requests, whose `right` is never
    /// read because they print at most a header and no diff.
    right: Option<ObjectId>,
    /// Header line to print before the diff, and whether `--no-commit-id`
    /// suppresses it. For a single commit the header is the commit id (and is
    /// suppressible); for the `--stdin` two-tree form it is the verbatim input
    /// line (and is *not* suppressed by `--no-commit-id`, matching git).
    header: Option<DiffHeader>,
    /// When set, produce no diff output (git silently skips a root commit diffed
    /// without `--root`, and unresolved `--stdin` lines). A header, if present,
    /// is still printed.
    skip: bool,
}

/// A header line plus whether `--no-commit-id` suppresses it.
struct DiffHeader {
    text: String,
    suppressible: bool,
}

/// Resolve the positional argument list into a diff request.
///
///   * One operand: treat it as a commit and diff it against its first parent.
///     A root commit produces nothing unless `--root` is given. The commit id
///     becomes the (suppressible) header.
///   * Two operands: diff the two tree-ish objects directly; no header.
fn resolve_arg_request(
    repo: &RepositoryContext,
    db: &FileObjectDatabase,
    options: &DiffTreeOptions,
    revs: &[String],
) -> Result<DiffRequest> {
    let format = repo.format();
    if revs.len() == 1 {
        let oid = resolve_tree_ish_arg(repo, &revs[0])?;
        // The argument form prints the resolved commit id as its header.
        single_commit_request(format, db, options, &oid, oid.to_hex())
    } else {
        // git only ever uses the first two operands as trees; anything further
        // would be a pathspec, which we reject earlier when it reaches us via
        // `--`. Here we defensively use the first two.
        let left = resolve_tree_ish_arg(repo, &revs[0])?;
        let right = resolve_tree_ish_arg(repo, &revs[1])?;
        let left_tree = sley_rev::peel_to_tree(db, format, &left)?;
        let right_tree = sley_rev::peel_to_tree(db, format, &right)?;
        Ok(DiffRequest {
            left: Some(left_tree),
            right: Some(right_tree),
            header: None,
            skip: false,
        })
    }
}

/// Build a single-commit diff request: commit tree vs first-parent tree.
///
/// `header_text` is the header line to print (the resolved commit id for the
/// argument form, or the verbatim input token for `--stdin`). A root commit is
/// skipped unless `--root` is set, exactly like git.
fn single_commit_request(
    format: ObjectFormat,
    db: &FileObjectDatabase,
    options: &DiffTreeOptions,
    oid: &ObjectId,
    header_text: String,
) -> Result<DiffRequest> {
    let object = db.read_object(oid)?;
    if object.object_type != ObjectType::Commit {
        // diff-tree's single-operand form insists on a commit; a bare tree there
        // is reported (to stderr) but is not a fatal error in git, and produces
        // no diff output.
        eprintln!(
            "error: object {oid} is a {}, not a commit",
            object.object_type.as_str()
        );
        return Ok(DiffRequest {
            left: None,
            right: Some(oid.clone()),
            header: None,
            skip: true,
        });
    }
    let commit = Commit::parse_ref(format, &object.body)?;
    let left = match commit.parents.first() {
        Some(parent) => Some(sley_rev::peel_to_tree(db, format, parent)?),
        None => None,
    };
    // A root commit (no parent) is silently skipped unless --root says to diff it
    // against the empty tree.
    if left.is_none() && !options.root {
        return Ok(DiffRequest {
            left: None,
            right: Some(commit.tree.clone()),
            header: None,
            skip: true,
        });
    }
    Ok(DiffRequest {
        left,
        right: Some(commit.tree.clone()),
        header: Some(DiffHeader {
            text: header_text,
            suppressible: true,
        }),
        skip: false,
    })
}

/// Parse one `--stdin` line.
///
/// Unlike the argument form, `--stdin` does **not** resolve refs or abbreviated
/// names: each token must be a full-length hex object id (this matches git, which
/// feeds `diff-tree --stdin` from `rev-list` output). A line whose tokens are not
/// valid full object ids is echoed as a header and otherwise skipped (no diff),
/// exactly like git.
///
///   * One token: a commit, diffed against its first parent (root-skip applies).
///     A single non-commit object id reports git's "Need exactly two trees"
///     error and is skipped.
///   * Two tokens: two tree-ish object ids, diffed directly. The header echoes
///     the verbatim input line and is not suppressed by `--no-commit-id`.
fn parse_stdin_request(
    format: ObjectFormat,
    db: &FileObjectDatabase,
    options: &DiffTreeOptions,
    line: &str,
) -> Result<DiffRequest> {
    let mut parts = line.split_whitespace();
    let Some(first) = parts.next() else {
        // Blank lines are filtered by the caller; treat anything else as a skip.
        return Ok(skip_echo(line));
    };
    let second = parts.next();
    if let Some(second) = second {
        let (Some(left), Some(right)) = (
            parse_full_oid(format, first),
            parse_full_oid(format, second),
        ) else {
            return Ok(skip_echo(line));
        };
        let (Ok(left_tree), Ok(right_tree)) = (
            sley_rev::peel_to_tree(db, format, &left),
            sley_rev::peel_to_tree(db, format, &right),
        ) else {
            return Ok(skip_echo(line));
        };
        // The two-tree stdin header echoes the input verbatim and is *not*
        // suppressed by --no-commit-id.
        Ok(DiffRequest {
            left: Some(left_tree),
            right: Some(right_tree),
            header: Some(DiffHeader {
                text: line.to_string(),
                suppressible: false,
            }),
            skip: false,
        })
    } else {
        let Some(oid) = parse_full_oid(format, first) else {
            return Ok(skip_echo(line));
        };
        let Ok(object) = db.read_object(&oid) else {
            return Ok(skip_echo(line));
        };
        if object.object_type != ObjectType::Commit {
            // A lone non-commit object id is not a valid single-token request:
            // git reports the error and prints no header for this line.
            eprintln!("error: Need exactly two trees, separated by a space");
            return Ok(skip_silent());
        }
        single_commit_request(format, db, options, &oid, first.to_string())
    }
}

/// Parse a token as a full-length hex object id, returning `None` when it is not
/// (wrong length or non-hex) so the caller can treat it as an unresolved stdin
/// line rather than an error.
fn parse_full_oid(format: ObjectFormat, token: &str) -> Option<ObjectId> {
    if token.len() != format.hex_len() || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    ObjectId::from_hex(format, token).ok()
}

/// A `--stdin` request that echoes `line` as its (non-suppressible) header but
/// produces no diff. git prints the input line for a `--stdin` entry whose tokens
/// it could not resolve to objects.
fn skip_echo(line: &str) -> DiffRequest {
    DiffRequest {
        left: None,
        right: None,
        header: Some(DiffHeader {
            text: line.to_string(),
            suppressible: false,
        }),
        skip: true,
    }
}

/// A request that produces no output at all (no header, no diff). Used when git
/// reports an error for a line but does not echo it.
fn skip_silent() -> DiffRequest {
    DiffRequest {
        left: None,
        right: None,
        header: None,
        skip: true,
    }
}

/// Resolve a single tree-ish/commit spec to an object id, emitting git's
/// `fatal: ambiguous argument ...` message (exit 128) when it does not name a
/// known revision.
fn resolve_tree_ish_arg(repo: &RepositoryContext, spec: &str) -> Result<ObjectId> {
    match repo.resolve_revision(spec) {
        Ok(oid) => Ok(oid),
        Err(_) => {
            eprintln!(
                "fatal: ambiguous argument '{spec}': unknown revision or path not in the working tree."
            );
            eprintln!(
                "Use '--' to separate paths from revisions, like this:\n'git <command> [<revision>...] -- [<file>...]'"
            );
            Err(GitError::Exit(128))
        }
    }
}

/// Execute and print one diff request. Returns `true` when there were
/// differences (so the caller can track an overall change flag).
struct DiffRequestContext<'a> {
    format: ObjectFormat,
    db: &'a FileObjectDatabase,
    options: &'a DiffTreeOptions,
    raw_abbrev: Option<usize>,
    patch_abbrev: usize,
}

fn run_diff_request(
    stdout: &mut io::Stdout,
    context: &DiffRequestContext<'_>,
    request: &DiffRequest,
) -> Result<bool> {
    // The header (commit id, or the verbatim stdin line for the two-tree form)
    // prints before the diff, even for skipped `--stdin` lines. --no-commit-id
    // only suppresses suppressible headers (the single-commit form), not the
    // two-tree stdin echo.
    if let Some(header) = &request.header
        && !(header.suppressible && context.options.no_commit_id)
    {
        writeln!(stdout, "{}", header.text)?;
    }

    // git silently skips some requests (a root commit diffed without --root, an
    // unresolved stdin line): emit no diff after any header above.
    if request.skip {
        return Ok(false);
    }
    let Some(right) = request.right.clone() else {
        return Ok(false);
    };

    let recursive = context.options.recursive || context.options.output.forces_recursion();
    let entries = compute_entries(
        context.format,
        context.db,
        context.options,
        request.left.as_ref(),
        &right,
        recursive,
    )?;
    let has_differences = !entries.is_empty();

    match context.options.output {
        DiffTreeOutput::Silent => {}
        DiffTreeOutput::Raw => {
            for entry in &entries {
                write_diff_raw_entry(
                    stdout,
                    entry,
                    context.options.z,
                    false,
                    context.raw_abbrev,
                    context.format,
                )?;
            }
        }
        DiffTreeOutput::NameOnly => {
            for entry in &entries {
                if context.options.z {
                    stdout.write_all(&entry.path)?;
                    stdout.write_all(b"\0")?;
                } else {
                    let path = status_quote_path(&entry.path, false);
                    writeln!(stdout, "{path}")?;
                }
            }
        }
        DiffTreeOutput::NameStatus => {
            for entry in &entries {
                if context.options.z {
                    stdout.write_all(entry.status.label().as_bytes())?;
                    stdout.write_all(b"\0")?;
                    if let Some(old_path) = &entry.old_path {
                        stdout.write_all(old_path)?;
                        stdout.write_all(b"\0")?;
                    }
                    stdout.write_all(&entry.path)?;
                    stdout.write_all(b"\0")?;
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
        DiffTreeOutput::Numstat => {
            for entry in &entries {
                write_diff_numstat_entry(
                    stdout,
                    entry,
                    context.options.z,
                    context.db,
                    None,
                    false,
                )?;
            }
        }
        DiffTreeOutput::Shortstat => {
            write_diff_shortstat(stdout, &entries, context.db, None, false)?;
        }
        DiffTreeOutput::Stat => {
            write_diff_stat(
                stdout,
                &entries,
                context.db,
                None,
                false,
                DiffStatOptions {
                    compact_summary: false,
                    stat_count: None,
                    color: false,
                },
            )?;
        }
        DiffTreeOutput::Summary => {
            for entry in &entries {
                write_diff_summary_entry(stdout, entry)?;
            }
        }
        DiffTreeOutput::Patch => {
            for entry in &entries {
                let patch_options = DiffPatchOptions {
                    db: context.db,
                    worktree_root: None,
                    use_worktree_new: false,
                    format: context.format,
                    abbrev: context.patch_abbrev,
                    src_prefix: &context.options.src_prefix,
                    dst_prefix: &context.options.dst_prefix,
                };
                write_diff_patch_entry(stdout, entry, patch_options)?;
            }
        }
    }

    Ok(has_differences)
}

/// Build the change list for a request, honouring the recursion mode.
///
///   * Recursive: delegate to `sley_diff_merge`, which flattens subtrees into
///     full paths and runs (only the requested) rename/copy detection.
///   * Non-recursive: walk the two trees' top levels ourselves so changed
///     subtrees stay collapsed as `040000` entries; rename/copy detection, when
///     asked for, runs over the top-level blob entries only.
fn compute_entries(
    format: ObjectFormat,
    db: &FileObjectDatabase,
    options: &DiffTreeOptions,
    left: Option<&ObjectId>,
    right: &ObjectId,
    recursive: bool,
) -> Result<Vec<sley_diff_merge::NameStatusEntry>> {
    if recursive {
        let rename_options = sley_diff_merge::RenameDetectionOptions {
            base: sley_diff_merge::DiffNameStatusOptions {
                detect_renames: options.detect_renames,
                detect_copies: options.detect_copies,
                find_copies_harder: options.find_copies_harder,
                rename_empty: options.rename_empty,
            },
            detect_inexact: options.detect_renames || options.detect_copies,
            rename_threshold: options.rename_threshold,
            copy_threshold: options.copy_threshold,
        };
        let mut entries = match left {
            Some(left) => sley_diff_merge::diff_name_status_trees_with_rename_options(
                db,
                format,
                left,
                right,
                rename_options,
            )?,
            None => sley_diff_merge::diff_name_status_empty_tree_with_rename_options(
                db,
                format,
                right,
                rename_options,
            )?,
        };
        if options.show_trees {
            // `-t` additionally surfaces the intermediate tree nodes that changed
            // between the two sides; merge them in and re-sort like git.
            let tree_entries = changed_tree_nodes(format, db, left, right)?;
            entries.extend(tree_entries);
            sort_entries_by_path(&mut entries);
        }
        Ok(entries)
    } else {
        let left_map = match left {
            Some(left) => top_level_entries(format, db, left)?,
            None => BTreeMap::new(),
        };
        let right_map = top_level_entries(format, db, right)?;
        let mut entries = top_level_changes(&left_map, &right_map);
        if options.detect_renames || options.detect_copies {
            entries = detect_top_level_renames(entries, db, options);
        }
        sort_entries_by_path(&mut entries);
        Ok(entries)
    }
}

/// A single tree entry (mode + oid), keyed by name within its tree.
#[derive(Clone, PartialEq, Eq)]
struct TopEntry {
    mode: u32,
    oid: ObjectId,
}

/// Read the immediate children of `tree_oid` (no recursion) into a name->entry
/// map. Subtrees appear as `040000` entries whose oid is the subtree id.
fn top_level_entries(
    format: ObjectFormat,
    db: &FileObjectDatabase,
    tree_oid: &ObjectId,
) -> Result<BTreeMap<Vec<u8>, TopEntry>> {
    let object = db.read_object(tree_oid)?;
    if object.object_type != ObjectType::Tree {
        return Err(GitError::InvalidObject(format!(
            "expected tree {tree_oid}, found {}",
            object.object_type.as_str()
        )));
    }
    let mut map = BTreeMap::new();
    for entry in TreeEntries::new(format, &object.body) {
        let entry = entry?;
        map.insert(
            entry.name.to_vec(),
            TopEntry {
                mode: entry.mode,
                oid: entry.oid,
            },
        );
    }
    Ok(map)
}

/// Compute add/delete/modify entries between two single-level entry maps. A name
/// present on both sides with a different (mode, oid) is `Modified`; this covers
/// both blob edits and changed subtrees (reported as `040000` modifications).
fn top_level_changes(
    left: &BTreeMap<Vec<u8>, TopEntry>,
    right: &BTreeMap<Vec<u8>, TopEntry>,
) -> Vec<sley_diff_merge::NameStatusEntry> {
    let mut names: BTreeSet<Vec<u8>> = BTreeSet::new();
    names.extend(left.keys().cloned());
    names.extend(right.keys().cloned());
    let mut changes = Vec::new();
    for name in names {
        let l = left.get(&name);
        let r = right.get(&name);
        let status = match (l, r) {
            (None, Some(_)) => sley_diff_merge::NameStatus::Added,
            (Some(_), None) => sley_diff_merge::NameStatus::Deleted,
            (Some(l), Some(r)) if l != r => sley_diff_merge::NameStatus::Modified,
            _ => continue,
        };
        changes.push(sley_diff_merge::NameStatusEntry {
            status,
            path: name,
            old_path: None,
            old_mode: l.map(|entry| entry.mode),
            new_mode: r.map(|entry| entry.mode),
            old_oid: l.map(|entry| entry.oid.clone()),
            new_oid: r.map(|entry| entry.oid.clone()),
        });
    }
    changes
}

/// Top-level rename/copy detection over an already-computed change list.
///
/// This mirrors `sley_diff_merge`'s recursive detection (exact-OID first, then
/// content similarity via `blob_similarity`, greedy best-match assignment), but
/// restricted to the immediate children so non-recursive output keeps changed
/// subtrees collapsed. Only blob (non-`040000`) entries are eligible as rename
/// or copy candidates; directories never participate.
fn detect_top_level_renames(
    mut changes: Vec<sley_diff_merge::NameStatusEntry>,
    db: &FileObjectDatabase,
    options: &DiffTreeOptions,
) -> Vec<sley_diff_merge::NameStatusEntry> {
    if options.detect_renames {
        changes = detect_top_level_rename_pass(
            changes,
            db,
            options.rename_threshold,
            options.rename_empty,
        );
    }
    if options.detect_copies {
        changes = detect_top_level_copy_pass(
            changes,
            db,
            options.copy_threshold,
            options.find_copies_harder,
            options.rename_empty,
        );
    }
    changes
}

/// Is this change entry a regular-file (blob) side, i.e. eligible for rename/copy
/// pairing? Directory (`040000`) entries are excluded.
fn entry_is_blob_old(entry: &sley_diff_merge::NameStatusEntry) -> bool {
    entry.old_mode.is_some_and(|mode| mode != 0o040000)
}

fn entry_is_blob_new(entry: &sley_diff_merge::NameStatusEntry) -> bool {
    entry.new_mode.is_some_and(|mode| mode != 0o040000)
}

/// Replace matched delete/add pairs with `Renamed` entries. Exact-OID matches
/// score 100 and take priority; remaining pairs are scored by content
/// similarity and assigned greedily, best score first.
fn detect_top_level_rename_pass(
    changes: Vec<sley_diff_merge::NameStatusEntry>,
    db: &FileObjectDatabase,
    threshold: u8,
    rename_empty: bool,
) -> Vec<sley_diff_merge::NameStatusEntry> {
    let deleted: Vec<usize> = changes
        .iter()
        .enumerate()
        .filter(|(_, e)| e.status == sley_diff_merge::NameStatus::Deleted && entry_is_blob_old(e))
        .map(|(idx, _)| idx)
        .collect();
    let added: Vec<usize> = changes
        .iter()
        .enumerate()
        .filter(|(_, e)| e.status == sley_diff_merge::NameStatus::Added && entry_is_blob_new(e))
        .map(|(idx, _)| idx)
        .collect();
    if deleted.is_empty() || added.is_empty() {
        return changes;
    }

    let mut src_used = vec![false; deleted.len()];
    let mut dst_used = vec![false; added.len()];
    // dst change-index -> (src change-index, score)
    let mut assigned: BTreeMap<usize, (usize, u8)> = BTreeMap::new();

    // Exact-OID renames first (score 100), in source order then dest order.
    for (si, &src_idx) in deleted.iter().enumerate() {
        let Some(src_oid) = changes[src_idx].old_oid.clone() else {
            continue;
        };
        if !rename_empty && is_empty_blob_oid(&src_oid) {
            continue;
        }
        for (di, &dst_idx) in added.iter().enumerate() {
            if dst_used[di] {
                continue;
            }
            if changes[dst_idx].new_oid.as_ref() == Some(&src_oid) {
                src_used[si] = true;
                dst_used[di] = true;
                assigned.insert(dst_idx, (src_idx, 100));
                break;
            }
        }
    }

    // Inexact renames over the remaining, threshold permitting.
    if threshold <= 100 {
        let mut pairs: Vec<(usize, usize, u8)> = Vec::new();
        for (si, &src_idx) in deleted.iter().enumerate() {
            if src_used[si] {
                continue;
            }
            let Some(src_oid) = changes[src_idx].old_oid.as_ref() else {
                continue;
            };
            if !rename_empty && is_empty_blob_oid(src_oid) {
                continue;
            }
            let Some(src_bytes) = read_blob_for_similarity(db, src_oid) else {
                continue;
            };
            for (di, &dst_idx) in added.iter().enumerate() {
                if dst_used[di] {
                    continue;
                }
                let Some(dst_oid) = changes[dst_idx].new_oid.as_ref() else {
                    continue;
                };
                if !rename_empty && is_empty_blob_oid(dst_oid) {
                    continue;
                }
                let Some(dst_bytes) = read_blob_for_similarity(db, dst_oid) else {
                    continue;
                };
                let score = sley_diff_merge::blob_similarity(&src_bytes, &dst_bytes);
                if score >= threshold {
                    pairs.push((si, di, score));
                }
            }
        }
        pairs.sort_by(|a, b| {
            b.2.cmp(&a.2)
                .then_with(|| a.0.cmp(&b.0))
                .then_with(|| a.1.cmp(&b.1))
        });
        for (si, di, score) in pairs {
            if src_used[si] || dst_used[di] {
                continue;
            }
            src_used[si] = true;
            dst_used[di] = true;
            assigned.insert(added[di], (deleted[si], score));
        }
    }

    apply_rename_assignments(changes, &assigned)
}

/// Rewrite `changes` so each assigned destination becomes a `Renamed` entry that
/// carries its source's old path/mode/oid, and the consumed source deletes are
/// dropped.
fn apply_rename_assignments(
    changes: Vec<sley_diff_merge::NameStatusEntry>,
    assigned: &BTreeMap<usize, (usize, u8)>,
) -> Vec<sley_diff_merge::NameStatusEntry> {
    if assigned.is_empty() {
        return changes;
    }
    let consumed_sources: BTreeSet<usize> = assigned.values().map(|(src, _)| *src).collect();
    // Snapshot source metadata before the sources are dropped.
    let mut source_meta: BTreeMap<usize, RenameSourceMeta> = BTreeMap::new();
    for &src in &consumed_sources {
        let src_entry = &changes[src];
        source_meta.insert(
            src,
            RenameSourceMeta {
                path: src_entry.path.clone(),
                mode: src_entry.old_mode,
                oid: src_entry.old_oid.clone(),
            },
        );
    }

    let mut result = Vec::with_capacity(changes.len());
    for (idx, entry) in changes.into_iter().enumerate() {
        if consumed_sources.contains(&idx) {
            continue;
        }
        if let Some((src_idx, score)) = assigned.get(&idx) {
            let meta = source_meta.get(src_idx).cloned().unwrap_or_default();
            result.push(sley_diff_merge::NameStatusEntry {
                status: sley_diff_merge::NameStatus::Renamed(*score),
                path: entry.path,
                old_path: Some(meta.path),
                old_mode: meta.mode,
                new_mode: entry.new_mode,
                old_oid: meta.oid,
                new_oid: entry.new_oid,
            });
            continue;
        }
        result.push(entry);
    }
    result
}

/// Old-side metadata of a rename source, snapshotted before the source delete
/// entry is consumed so it can be attached to the renamed destination.
#[derive(Clone, Default)]
struct RenameSourceMeta {
    path: Vec<u8>,
    mode: Option<u32>,
    oid: Option<ObjectId>,
}

/// Detect copies among the still-`Added` top-level entries. With
/// `find_copies_harder`, every left-side blob is a candidate source; otherwise
/// only blobs that themselves changed (deleted/modified) on this diff. Copies do
/// not consume their source.
fn detect_top_level_copy_pass(
    mut changes: Vec<sley_diff_merge::NameStatusEntry>,
    db: &FileObjectDatabase,
    threshold: u8,
    find_copies_harder: bool,
    rename_empty: bool,
) -> Vec<sley_diff_merge::NameStatusEntry> {
    if threshold > 100 {
        return changes;
    }
    let sources: Vec<CopySource> = changes
        .iter()
        .filter(|entry| match entry.status {
            sley_diff_merge::NameStatus::Deleted | sley_diff_merge::NameStatus::Modified => true,
            _ => find_copies_harder,
        })
        .filter_map(|entry| match (entry.old_mode, entry.old_oid.as_ref()) {
            (Some(mode), Some(oid)) if mode != 0o040000 => Some(CopySource {
                path: entry.path.clone(),
                mode,
                oid: oid.clone(),
            }),
            _ => None,
        })
        .collect();
    if sources.is_empty() {
        return changes;
    }

    for entry in changes.iter_mut() {
        if entry.status != sley_diff_merge::NameStatus::Added || !entry_is_blob_new(entry) {
            continue;
        }
        let Some(dst_oid) = entry.new_oid.clone() else {
            continue;
        };
        if !rename_empty && is_empty_blob_oid(&dst_oid) {
            continue;
        }
        // Exact-oid copy first (score 100).
        if let Some(source) = sources.iter().find(|source| source.oid == dst_oid) {
            entry.status = sley_diff_merge::NameStatus::Copied(100);
            entry.old_path = Some(source.path.clone());
            entry.old_mode = Some(source.mode);
            entry.old_oid = Some(source.oid.clone());
            continue;
        }
        let Some(dst_bytes) = read_blob_for_similarity(db, &dst_oid) else {
            continue;
        };
        let mut best: Option<(u8, &CopySource)> = None;
        for source in &sources {
            if !rename_empty && is_empty_blob_oid(&source.oid) {
                continue;
            }
            let Some(src_bytes) = read_blob_for_similarity(db, &source.oid) else {
                continue;
            };
            let score = sley_diff_merge::blob_similarity(&src_bytes, &dst_bytes);
            if score >= threshold && best.as_ref().is_none_or(|(b, _)| score > *b) {
                best = Some((score, source));
            }
        }
        if let Some((score, source)) = best {
            entry.status = sley_diff_merge::NameStatus::Copied(score);
            entry.old_path = Some(source.path.clone());
            entry.old_mode = Some(source.mode);
            entry.old_oid = Some(source.oid.clone());
        }
    }
    changes
}

/// A candidate copy source: the old-side path/mode/oid of a left-side blob.
struct CopySource {
    path: Vec<u8>,
    mode: u32,
    oid: ObjectId,
}

/// Read a blob's bytes for similarity scoring, returning `None` when the object
/// is missing or is not a blob (so a bad candidate just fails to match).
fn read_blob_for_similarity(db: &FileObjectDatabase, oid: &ObjectId) -> Option<Vec<u8>> {
    match db.read_object(oid) {
        Ok(object) if object.object_type == ObjectType::Blob => Some(object.body.clone()),
        _ => None,
    }
}

/// The well-known empty-blob object id for the repository's hash format.
fn is_empty_blob_oid(oid: &ObjectId) -> bool {
    EncodedObject::new(ObjectType::Blob, Vec::new())
        .object_id(oid.format())
        .map(|empty| &empty == oid)
        .unwrap_or(false)
}

/// Sort the change list by destination path, matching git's ordering for the
/// non-rename entries we produce here (raw/name modes and `-t` tree nodes never
/// involve a rename whose old path would sort differently).
fn sort_entries_by_path(entries: &mut [sley_diff_merge::NameStatusEntry]) {
    entries.sort_by(|a, b| a.path.cmp(&b.path));
}

/// Collect the intermediate-tree (`040000`) change entries for `-t`, recursing
/// in lockstep over both sides so every changed subtree node is reported.
fn changed_tree_nodes(
    format: ObjectFormat,
    db: &FileObjectDatabase,
    left: Option<&ObjectId>,
    right: &ObjectId,
) -> Result<Vec<sley_diff_merge::NameStatusEntry>> {
    let mut out = Vec::new();
    collect_changed_tree_nodes(format, db, left, Some(right), Vec::new(), &mut out)?;
    Ok(out)
}

/// Recursive worker for `-t`: at each level, compare the subtree children of the
/// two sides; for every subtree name that differs (added, removed, or changed
/// id), emit a `040000` entry and descend into the changed ones.
fn collect_changed_tree_nodes(
    format: ObjectFormat,
    db: &FileObjectDatabase,
    left_tree: Option<&ObjectId>,
    right_tree: Option<&ObjectId>,
    prefix: Vec<u8>,
    out: &mut Vec<sley_diff_merge::NameStatusEntry>,
) -> Result<()> {
    let left_children = match left_tree {
        Some(oid) => subtree_children(format, db, oid)?,
        None => BTreeMap::new(),
    };
    let right_children = match right_tree {
        Some(oid) => subtree_children(format, db, oid)?,
        None => BTreeMap::new(),
    };
    let mut names: BTreeSet<Vec<u8>> = BTreeSet::new();
    names.extend(left_children.keys().cloned());
    names.extend(right_children.keys().cloned());
    for name in names {
        let l = left_children.get(&name);
        let r = right_children.get(&name);
        if l == r {
            continue;
        }
        let mut path = prefix.clone();
        if !path.is_empty() {
            path.push(b'/');
        }
        path.extend_from_slice(&name);
        let status = match (l, r) {
            (None, Some(_)) => sley_diff_merge::NameStatus::Added,
            (Some(_), None) => sley_diff_merge::NameStatus::Deleted,
            _ => sley_diff_merge::NameStatus::Modified,
        };
        out.push(sley_diff_merge::NameStatusEntry {
            status,
            path: path.clone(),
            old_path: None,
            old_mode: l.map(|_| 0o040000),
            new_mode: r.map(|_| 0o040000),
            old_oid: l.cloned(),
            new_oid: r.cloned(),
        });
        // Descend into modified subtrees (both sides present) so deeper changed
        // trees are reported too.
        if l.is_some() && r.is_some() {
            collect_changed_tree_nodes(format, db, l, r, path, out)?;
        }
    }
    Ok(())
}

/// The immediate subtree (`040000`) children of `tree_oid`, keyed by name.
fn subtree_children(
    format: ObjectFormat,
    db: &FileObjectDatabase,
    tree_oid: &ObjectId,
) -> Result<BTreeMap<Vec<u8>, ObjectId>> {
    let object = db.read_object(tree_oid)?;
    if object.object_type != ObjectType::Tree {
        return Err(GitError::InvalidObject(format!(
            "expected tree {tree_oid}, found {}",
            object.object_type.as_str()
        )));
    }
    let mut map = BTreeMap::new();
    for entry in TreeEntries::new(format, &object.body) {
        let entry = entry?;
        if entry.mode == 0o040000 {
            map.insert(entry.name.to_vec(), entry.oid);
        }
    }
    Ok(map)
}

/// `diff-tree`'s usage block, printed to stderr when no operands are supplied or
/// an unknown bare option is seen. Matches git's wording (exit 129).
fn print_diff_tree_usage() {
    eprintln!("usage: git diff-tree [--stdin] [-m] [-s] [-v] [--no-commit-id] [--pretty]");
    eprintln!("              [-t] [-r] [-c | --cc] [--combined-all-paths] [--root] [--merge-base]");
    eprintln!("              [<common-diff-options>] <tree-ish> [<tree-ish>] [<path>...]");
}
