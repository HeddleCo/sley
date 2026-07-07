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
use sley::plumbing::sley_object::TreeEntries;
use sley::plumbing::{sley_diff_merge, sley_rev, sley_worktree};

/// Which output formats to produce, mirroring git's `output_format` bitmask:
/// the explicit format options accumulate (`--stat --summary` prints both, and
/// `--patch-with-raw` is raw + patch); `diff-tree` defaults to raw when nothing
/// was requested, which is *not* the default for `git diff`.
#[derive(Debug, Clone, Copy, Default)]
struct DiffTreeOutput {
    raw: bool,
    patch: bool,
    stat: bool,
    compact_summary: bool,
    numstat: bool,
    shortstat: bool,
    summary: bool,
    name_only: bool,
    name_status: bool,
    /// `-s`/`--no-patch`: compute the diff (for the exit code) but print nothing
    /// except, for a single commit, the commit-id header.
    silent: bool,
}

impl DiffTreeOutput {
    /// File-content output modes always operate at blob granularity, so they
    /// descend into changed subtrees regardless of `-r`.
    fn forces_recursion(self) -> bool {
        self.patch || self.stat || self.numstat || self.shortstat || self.summary
    }

    /// Whether any explicit format was selected (otherwise raw is the default).
    fn any(&self) -> bool {
        self.raw
            || self.patch
            || self.stat
            || self.numstat
            || self.shortstat
            || self.summary
            || self.name_only
            || self.name_status
            || self.silent
    }
}

/// `--pretty` commit-header formats diff-tree supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiffTreePretty {
    Medium,
    Oneline,
    Subject,
    Notes,
}

/// Parsed `diff-tree` invocation.
struct DiffTreeOptions {
    output: DiffTreeOutput,
    recursive: bool,
    /// `-t`: when recursing, also emit the intermediate tree (`040000`) entries.
    show_trees: bool,
    /// `-R`: swap old and new sides before rendering.
    reverse: bool,
    /// `--root`: for a single commit with no parent, diff against the empty tree
    /// instead of producing nothing.
    root: bool,
    /// `--merge-base`: diff the merge base of two commits against the second
    /// commit.
    merge_base: bool,
    /// `--no-commit-id`: suppress the per-commit object-id header line.
    no_commit_id: bool,
    /// `--pretty[=medium|oneline]` / `-v`: print a commit-log header instead of
    /// the bare commit id.
    pretty: Option<DiffTreePretty>,
    /// `--notes` / `--show-notes`: append the standard notes block to the
    /// medium pretty header.
    show_notes: bool,
    /// `-m`: for a merge commit, emit one diff per parent (each preceded by the
    /// commit header). Without it a merge produces no output at all.
    merges_separate: bool,
    /// `-c` / `--cc`: combined merge diff. `Some(dense)` selects the renderer;
    /// `dense=true` is `--cc` (drop hunks the result shares with a parent),
    /// `dense=false` is `-c` (show every parent). `None` means no combined mode.
    combined: Option<bool>,
    /// `--combined-all-paths`: list each parent's path on a separate `---` line
    /// (only meaningful with `-c`/`--cc`).
    combined_all_paths: bool,
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
    /// `--binary`: emit `GIT binary patch` blocks (implies full index).
    patch_binary: bool,
    src_prefix: String,
    dst_prefix: String,
    /// `--check`: emit a whitespace-error report instead of the diff body.
    check: bool,
    /// `-S<string>`: keep filepairs whose old/new occurrence count differs.
    pickaxe: Option<String>,
    /// `--pickaxe-all`: if any filepair matches `-S`, show the whole changeset.
    pickaxe_all: bool,
    /// `--find-object=<oid>`: keep filepairs whose object occurrence changes.
    find_object_values: Vec<String>,
    /// `--exit-code` / `--quiet`: exit with status 1 when any difference is
    /// found (0 otherwise). `--quiet` additionally suppresses the diff output.
    exit_code: bool,
    /// Whitespace-ignore flags (`-w`, `-b`, `--ignore-space-at-eol`,
    /// `--ignore-cr-at-eol`).
    ws_ignore: sley_diff_merge::WsIgnore,
    /// The line-diff algorithm (`--patience` / `--histogram` / Myers default).
    diff_algorithm: sley_diff_merge::DiffAlgorithm,
    /// `--ignore-blank-lines`.
    ignore_blank_lines: bool,
    /// Compiled `-I<regex>` (`--ignore-matching-lines`) patterns.
    ignore_regexes: Vec<sley_grep::Regex>,
    /// `--max-depth=<n>`: recurse tree diffs only to this many directory
    /// levels below the matching pathspec and show changed subtrees at the
    /// boundary. `-1` means unlimited recursion.
    max_depth: Option<i64>,
    /// `--indent-heuristic` / `--no-indent-heuristic`: `None` falls back to
    /// `diff.indentHeuristic` config (default git-enabled).
    indent_heuristic: Option<bool>,
    /// Revision/pathspec arguments passed to the shared revision parser.
    setup_args: Vec<String>,
}

impl Default for DiffTreeOptions {
    fn default() -> Self {
        Self {
            output: DiffTreeOutput::default(),
            recursive: false,
            show_trees: false,
            reverse: false,
            root: false,
            merge_base: false,
            no_commit_id: false,
            pretty: None,
            show_notes: false,
            merges_separate: false,
            combined: None,
            combined_all_paths: false,
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
            patch_binary: false,
            src_prefix: "a/".to_string(),
            dst_prefix: "b/".to_string(),
            check: false,
            pickaxe: None,
            pickaxe_all: false,
            find_object_values: Vec::new(),
            exit_code: false,
            ws_ignore: sley_diff_merge::WsIgnore::default(),
            diff_algorithm: sley_diff_merge::DiffAlgorithm::Myers,
            ignore_blank_lines: false,
            ignore_regexes: Vec::new(),
            max_depth: None,
            indent_heuristic: None,
            setup_args: Vec::new(),
        }
    }
}

pub(crate) fn cmd_diff_tree(args: &[String]) -> Result<()> {
    // `diff-tree -s --pretty=tformat:%s <commit>` — the no-diff subject-only
    // form the sequencer suite uses; print the subject and stop.
    {
        let silent = args.iter().any(|arg| arg == "-s" || arg == "--no-patch");
        let pretty = args
            .iter()
            .find_map(|arg| arg.strip_prefix("--pretty="))
            .filter(|fmt| *fmt == "tformat:%s" || *fmt == "format:%s");
        if silent && let Some(_fmt) = pretty {
            let setup_args: Vec<String> = args
                .iter()
                .filter(|arg| !arg.starts_with('-') || arg.as_str() == "-")
                .cloned()
                .collect();
            if setup_args.len() == 1 {
                let cwd = env::current_dir()?;
                let git_dir = crate::session::cli_git_dir_from(&cwd)?;
                let format = repository_object_format(&git_dir)?;
                let config = read_repo_config(&git_dir)?;
                let db = FileObjectDatabase::from_git_dir(&git_dir, format);
                let worktree_root = worktree_root_for_git_dir(&git_dir).ok();
                let setup = sley_rev::setup_revisions(
                    &setup_args,
                    &sley_rev::RevisionSetupContext {
                        git_dir: &git_dir,
                        worktree_root: worktree_root.as_deref(),
                        cwd: &cwd,
                        format,
                        reader: &db,
                        config: Some(&config),
                    },
                )?;
                if setup.options.positives.len() != 1
                    || !setup.pathspecs.is_empty()
                    || !setup.options.negatives.is_empty()
                    || !setup.options.symmetric_ranges.is_empty()
                {
                    return Err(GitError::Unsupported(
                        "diff-tree pretty/commit-log output is not supported".into(),
                    ));
                }
                let commit_oid =
                    sley_rev::peel_to_commit(&db, format, &setup.options.positives[0].oid)?;
                let object = db.read_object(&commit_oid)?;
                let commit = Commit::parse(format, &object.body)?;
                println!("{}", commit_subject(&commit.message));
                return Ok(());
            }
        }
    }
    let mut options = DiffTreeOptions::default();
    let mut positional_only = false;
    let mut ignore_regex_patterns: Vec<String> = Vec::new();
    let mut idx = 0;
    while idx < args.len() {
        let arg = &args[idx];
        if positional_only {
            options.setup_args.push(arg.clone());
            idx += 1;
            continue;
        }
        match arg.as_str() {
            "--" => {
                options.setup_args.push(arg.clone());
                positional_only = true;
            }
            "-r" | "--recursive" => options.recursive = true,
            "-t" => {
                options.recursive = true;
                options.show_trees = true;
            }
            "-R" => options.reverse = true,
            "--root" => options.root = true,
            "--always" => {}
            "--merge-base" => options.merge_base = true,
            "--check" => options.check = true,
            "--no-commit-id" => options.no_commit_id = true,
            "--stdin" => options.stdin = true,
            "-z" => options.z = true,
            "-p" | "-u" | "--patch" => options.output.patch = true,
            "--raw" => options.output.raw = true,
            "--patch-with-stat" => {
                options.output.patch = true;
                options.output.stat = true;
            }
            "--patch-with-raw" => {
                options.output.patch = true;
                options.output.raw = true;
            }
            "--stat" => options.output.stat = true,
            "--compact-summary" => {
                options.output.stat = true;
                options.output.compact_summary = true;
            }
            "--numstat" => options.output.numstat = true,
            "--shortstat" => options.output.shortstat = true,
            "--summary" => options.output.summary = true,
            "--name-only" => options.output.name_only = true,
            "--name-status" => options.output.name_status = true,
            "-s" | "--no-patch" => {
                options.output = DiffTreeOutput {
                    silent: true,
                    ..DiffTreeOutput::default()
                };
            }
            "--exit-code" => options.exit_code = true,
            "--quiet" => {
                // `--quiet` implies `-s` (no diff body) plus exit-with-status.
                options.exit_code = true;
                options.output = DiffTreeOutput {
                    silent: true,
                    ..DiffTreeOutput::default()
                };
            }
            "-S" => {
                idx += 1;
                let value = args
                    .get(idx)
                    .ok_or_else(commands::diff_options::diff_pickaxe_requires_non_empty_error)?;
                if value.is_empty() {
                    return Err(commands::diff_options::diff_pickaxe_requires_non_empty_error());
                }
                options.pickaxe = Some(value.clone());
            }
            value if value.starts_with("-S") => {
                let value = &value[2..];
                if value.is_empty() {
                    return Err(commands::diff_options::diff_pickaxe_requires_non_empty_error());
                }
                options.pickaxe = Some(value.to_string());
            }
            "--pickaxe-all" => options.pickaxe_all = true,
            "--find-object" => {
                idx += 1;
                let value = args
                    .get(idx)
                    .ok_or_else(|| log_option_requires_value_error("find-object"))?;
                options.find_object_values.push(value.clone());
            }
            value if let Some(rest) = value.strip_prefix("--find-object=") => {
                options.find_object_values.push(rest.to_string());
            }
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
            // -l<n>: the rename/copy matrix limit. `0` is git's "big default"
            // (unlimited); diff-tree's detector is already unbounded, so we
            // validate the value for parity and otherwise ignore it.
            value
                if let Some(rest) = value.strip_prefix("-l")
                    && !rest.is_empty()
                    && rest.bytes().all(|b| b.is_ascii_digit()) =>
            {
                let _: u64 = rest
                    .parse()
                    .map_err(|_| GitError::Command(format!("invalid argument to -l: {rest}")))?;
            }
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
            "--binary" => {
                options.patch_binary = true;
                options.patch_full_index = true;
            }
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
            // Combined-merge output. `-c` selects (non-dense) combined and
            // leaves the default format raw (`::`); `--cc` selects dense
            // combined and implies patch output *only when no explicit output
            // format is given* (git's `merges_imply_patch`, applied below).
            "-c" => options.combined = Some(false),
            "--cc" => options.combined = Some(true),
            "--combined-all-paths" => options.combined_all_paths = true,
            "-m" => options.merges_separate = true,
            // Whitespace-ignore / algorithm / ignore-matching-lines: applied to
            // the patch hunk comparison (see the shared diff renderer).
            "--minimal" => options.diff_algorithm = sley_diff_merge::DiffAlgorithm::Minimal,
            "--patience" => options.diff_algorithm = sley_diff_merge::DiffAlgorithm::Patience,
            "--histogram" => options.diff_algorithm = sley_diff_merge::DiffAlgorithm::Histogram,
            "--indent-heuristic" => options.indent_heuristic = Some(true),
            "--no-indent-heuristic" => options.indent_heuristic = Some(false),
            "--diff-algorithm" => {
                idx += 1;
                let value = args
                    .get(idx)
                    .ok_or_else(|| GitError::Command("--diff-algorithm requires a value".into()))?;
                log_validate_diff_algorithm(value)?;
                options.diff_algorithm = crate::log_parse_diff_algorithm(value);
            }
            value if let Some(rest) = value.strip_prefix("--diff-algorithm=") => {
                log_validate_diff_algorithm(rest)?;
                options.diff_algorithm = crate::log_parse_diff_algorithm(rest);
            }
            "-w" | "--ignore-all-space" => options.ws_ignore.all_space = true,
            "-b" | "--ignore-space-change" => options.ws_ignore.space_change = true,
            "--ignore-space-at-eol" => options.ws_ignore.space_at_eol = true,
            "--ignore-cr-at-eol" => options.ws_ignore.cr_at_eol = true,
            "--ignore-blank-lines" => options.ignore_blank_lines = true,
            "--max-depth" => {
                idx += 1;
                let value = args
                    .get(idx)
                    .ok_or_else(|| GitError::Command("--max-depth requires a value".into()))?;
                options.max_depth = Some(parse_diff_tree_max_depth(value)?);
            }
            value if let Some(rest) = value.strip_prefix("--max-depth=") => {
                options.max_depth = Some(parse_diff_tree_max_depth(rest)?);
            }
            "-I" | "--ignore-matching-lines" => {
                idx += 1;
                let value = args.get(idx).ok_or_else(|| {
                    GitError::Command("--ignore-matching-lines requires a value".into())
                })?;
                ignore_regex_patterns.push(value.clone());
            }
            value if let Some(rest) = value.strip_prefix("--ignore-matching-lines=") => {
                ignore_regex_patterns.push(rest.to_string());
            }
            value if value.starts_with("-I") && value.len() > 2 => {
                ignore_regex_patterns.push(value[2..].to_string());
            }
            "--pretty" | "-v" | "--pretty=medium" => {
                options.pretty = Some(DiffTreePretty::Medium);
            }
            "--pretty=oneline" => options.pretty = Some(DiffTreePretty::Oneline),
            "--notes" | "--show-notes" => options.show_notes = true,
            "--no-notes" => options.show_notes = false,
            "--format=%s" | "--pretty=format:%s" | "--pretty=tformat:%s" => {
                options.pretty = Some(DiffTreePretty::Subject);
            }
            "--format=%N" | "--pretty=format:%N" | "--pretty=tformat:%N" => {
                options.pretty = Some(DiffTreePretty::Notes);
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
                options.setup_args.push(arg.clone());
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

    // `--combined-all-paths` is only meaningful alongside `-c`/`--cc`.
    if options.combined_all_paths && options.combined.is_none() {
        return Err(GitError::Command(
            "--combined-all-paths makes no sense without -c or --cc".into(),
        ));
    }

    if !options.output.any() {
        // `--cc` with no explicit format implies a combined patch (git's
        // `merges_imply_patch`); `-c` and the plain two-tree form default to
        // raw (`::` for combined, `:` otherwise).
        if options.combined == Some(true) {
            options.output.patch = true;
        } else {
            options.output.raw = true;
        }
    }
    options.ignore_regexes = crate::compile_ignore_matching_regexes(&ignore_regex_patterns)?;
    let repo = RepositoryContext::discover_current()?;
    let git_dir = repo.git_dir();
    let format = repo.format();
    let db = repo.objects();
    // git loads the display notes refs at revision setup; a valueless
    // `-c notes.displayRef` is a fatal parse error that must surface before any
    // output. Resolve them up front (and discard) when notes display is on.
    if options.show_notes {
        crate::commands::log::resolve_standard_notes_refs(git_dir, format)?;
    }
    let setup = sley_rev::setup_revisions(
        &options.setup_args,
        &sley_rev::RevisionSetupContext {
            git_dir,
            worktree_root: repo.worktree_root().ok(),
            cwd: repo.cwd(),
            format,
            reader: db,
            config: Some(repo.config()),
        },
    )?;
    if let Some(leftover) = setup.leftovers.first() {
        return Err(GitError::Command(format!(
            "unsupported diff-tree option {leftover}"
        )));
    }

    if setup.options.positives.len() > 2 {
        return Err(GitError::Unsupported(
            "diff-tree pathspec filtering is not supported".into(),
        ));
    }
    if options.max_depth.is_some() && diff_tree_has_wildcard_pathspec(&setup.pathspecs) {
        eprintln!("fatal: max-depth cannot be used with wildcard pathspecs");
        return Err(GitError::Exit(128));
    }
    if !setup.options.negatives.is_empty() || !setup.options.symmetric_ranges.is_empty() {
        return Err(GitError::Unsupported(
            "diff-tree revision ranges are not supported".into(),
        ));
    }

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
    let ws_resolver = if options.check {
        Some(commands::diff::WhitespaceRuleResolver::from_git_dir(
            git_dir,
        )?)
    } else {
        None
    };
    let find_objects =
        commands::diff::resolve_diff_find_objects(git_dir, format, &options.find_object_values)?;
    // `--indent-heuristic` / `--no-indent-heuristic` win over the
    // `diff.indentHeuristic` config (which defaults to git's enabled behavior).
    let indent_heuristic = options.indent_heuristic.unwrap_or_else(|| {
        repo.config()
            .get_bool("diff", None, "indentheuristic")
            .unwrap_or(true)
    });
    let diff_pathspec = if setup.pathspecs.is_empty() || options.max_depth.is_some() {
        None
    } else if let Ok(worktree_root) = repo.worktree_root() {
        Some(DiffPathspec::new(
            repo.cwd(),
            worktree_root,
            &setup.pathspecs,
        )?)
    } else {
        None
    };
    let request_context = DiffRequestContext {
        format,
        db,
        options: &options,
        pathspecs: &setup.pathspecs,
        diff_pathspec,
        raw_abbrev,
        patch_abbrev,
        find_objects: &find_objects,
        indent_heuristic,
        ws_resolver,
        check_failed: std::cell::Cell::new(false),
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
            for request in parse_stdin_request(git_dir, format, db, &options, line)? {
                if run_diff_request(&mut stdout, &request_context, &request)? {
                    has_differences = true;
                }
            }
        }
    } else {
        if options.merge_base && setup.options.positives.len() == 1 {
            eprintln!("fatal: --merge-base only works with two commits");
            return Err(GitError::Exit(128));
        }
        if setup.options.positives.is_empty() {
            print_diff_tree_usage();
            return Err(GitError::Exit(129));
        }
        let requests = if options.merge_base {
            resolve_merge_base_arg_request(git_dir, db, &setup.options.positives)?
        } else {
            resolve_arg_request(git_dir, db, &options, &setup.options.positives)?
        };
        for request in requests {
            if run_diff_request(&mut stdout, &request_context, &request)? {
                has_differences = true;
            }
        }
    }

    if options.check && request_context.check_failed.get() {
        return Err(GitError::Exit(2));
    }
    if options.exit_code && has_differences {
        return Err(GitError::Exit(1));
    }
    Ok(())
}

/// One resolved diff request: a left tree, a right tree, and an optional commit
/// id to print as the header (present only when the request came from a single
/// commit / a commit on `--stdin`).
#[derive(Default)]
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
    /// When `Some`, this request is a combined merge diff: render the result
    /// tree (`right`) against every parent tree at once. `left` is unused in
    /// this mode. Built only when `-c`/`--cc` selected a merge commit.
    combined: Option<CombinedRequest>,
}

/// A combined-diff request: the merge result and its parent commits.
struct CombinedRequest {
    /// The result (merge commit) tree.
    result_tree: ObjectId,
    /// Each parent's tree, in parent order.
    parent_trees: Vec<ObjectId>,
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
    git_dir: &Path,
    db: &FileObjectDatabase,
    options: &DiffTreeOptions,
    revs: &[sley_rev::RevisionTip],
) -> Result<Vec<DiffRequest>> {
    let format = db.object_format();
    if revs.len() == 1 {
        let oid = revs[0].oid;
        // The argument form prints the resolved commit id as its header.
        single_commit_request(git_dir, format, db, options, &oid, oid.to_hex())
    } else {
        // git only ever uses the first two operands as trees; anything further
        // would be a pathspec, which we reject earlier when it reaches us via
        // `--`. Here we defensively use the first two.
        let left = revs[0].oid;
        let right = revs[1].oid;
        let left_tree = sley_rev::peel_to_tree(db, format, &left)?;
        let right_tree = sley_rev::peel_to_tree(db, format, &right)?;
        Ok(vec![DiffRequest {
            left: Some(left_tree),
            right: Some(right_tree),
            ..Default::default()
        }])
    }
}

fn resolve_merge_base_arg_request(
    git_dir: &Path,
    db: &FileObjectDatabase,
    revs: &[sley_rev::RevisionTip],
) -> Result<Vec<DiffRequest>> {
    if revs.len() != 2 {
        print_diff_tree_usage();
        return Err(GitError::Exit(129));
    }
    let format = db.object_format();
    let left = commands::diff::diff_resolve_commit_arg(git_dir, format, db, &revs[0].rev)?;
    let right = commands::diff::diff_resolve_commit_arg(git_dir, format, db, &revs[1].rev)?;
    let base = commands::diff::diff_single_merge_base(git_dir, format, db, &left, &right)?;
    Ok(vec![DiffRequest {
        left: Some(sley_rev::peel_to_tree(db, format, &base)?),
        right: Some(sley_rev::peel_to_tree(db, format, &right)?),
        ..Default::default()
    }])
}

/// Build a single-commit diff request: commit tree vs first-parent tree.
///
/// `header_text` is the header line to print (the resolved commit id for the
/// argument form, or the verbatim input token for `--stdin`). A root commit is
/// skipped unless `--root` is set, exactly like git.
fn single_commit_request(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    options: &DiffTreeOptions,
    oid: &ObjectId,
    header_text: String,
) -> Result<Vec<DiffRequest>> {
    let object = db.read_object(oid)?;
    if object.object_type != ObjectType::Commit {
        // diff-tree's single-operand form insists on a commit; a bare tree there
        // is reported (to stderr) but is not a fatal error in git, and produces
        // no diff output.
        eprintln!(
            "error: object {oid} is a {}, not a commit",
            object.object_type.as_str()
        );
        return Ok(vec![DiffRequest {
            right: Some(*oid),
            skip: true,
            ..Default::default()
        }]);
    }
    let commit = Commit::parse(format, &object.body)?;
    let header_text = diff_tree_header_text(git_dir, format, options, oid, &commit, header_text)?;
    if commit.parents.len() > 1 {
        // A merge commit. Precedence matches git: `-c`/`--cc` (combined) wins
        // over `-m` (separate). Without any of them a merge produces no output.
        if options.combined.is_some() {
            let parent_trees = commit
                .parents
                .iter()
                .map(|parent| sley_rev::peel_to_tree(db, format, parent))
                .collect::<Result<Vec<_>>>()?;
            return Ok(vec![DiffRequest {
                right: Some(commit.tree.clone()),
                header: Some(DiffHeader {
                    text: header_text,
                    suppressible: true,
                }),
                combined: Some(CombinedRequest {
                    result_tree: commit.tree.clone(),
                    parent_trees,
                }),
                ..Default::default()
            }]);
        }
        if !options.merges_separate {
            return Ok(vec![DiffRequest {
                right: Some(commit.tree.clone()),
                skip: true,
                ..Default::default()
            }]);
        }
        let mut requests = Vec::with_capacity(commit.parents.len());
        for parent in &commit.parents {
            requests.push(DiffRequest {
                left: Some(sley_rev::peel_to_tree(db, format, parent)?),
                right: Some(commit.tree.clone()),
                header: Some(DiffHeader {
                    text: header_text.clone(),
                    suppressible: true,
                }),
                ..Default::default()
            });
        }
        return Ok(requests);
    }
    let left = match commit.parents.first() {
        Some(parent) => Some(sley_rev::peel_to_tree(db, format, parent)?),
        None => None,
    };
    // A root commit (no parent) is silently skipped unless --root says to diff it
    // against the empty tree.
    if left.is_none() && !options.root {
        return Ok(vec![DiffRequest {
            right: Some(commit.tree.clone()),
            skip: true,
            ..Default::default()
        }]);
    }
    Ok(vec![DiffRequest {
        left,
        right: Some(commit.tree.clone()),
        header: Some(DiffHeader {
            text: header_text,
            suppressible: true,
        }),
        ..Default::default()
    }])
}

/// The header block for a single-commit request: the bare id (default), or the
/// `--pretty` medium/oneline commit-log header. The medium form embeds a
/// trailing newline so the printed block ends with the blank line that
/// separates it from the diff.
fn diff_tree_header_text(
    git_dir: &Path,
    format: ObjectFormat,
    options: &DiffTreeOptions,
    oid: &ObjectId,
    commit: &Commit,
    plain_text: String,
) -> Result<String> {
    match options.pretty {
        None => Ok(plain_text),
        Some(DiffTreePretty::Oneline) => Ok(format!("{oid} {}", commit_subject(&commit.message))),
        Some(DiffTreePretty::Subject) if options.output.silent => {
            Ok(commit_subject(&commit.message))
        }
        Some(DiffTreePretty::Subject) => Ok(format!("{}\n", commit_subject(&commit.message))),
        Some(DiffTreePretty::Notes) => diff_tree_pretty_notes(git_dir, format, oid),
        Some(DiffTreePretty::Medium) => {
            let mut text = format!("commit {oid}\n");
            if commit.parents.len() > 1 {
                let merged: Vec<String> =
                    commit.parents.iter().map(format_log_abbrev_oid).collect();
                text.push_str(&format!("Merge: {}\n", merged.join(" ")));
            }
            text.push_str(&format!(
                "Author: {}\n",
                commit_author_identity(&commit.author)
            ));
            text.push_str(&format!(
                "Date:   {}\n",
                commit_identity_date(&commit.author, &DateMode::Default)
            ));
            text.push('\n');
            for line in String::from_utf8_lossy(&commit.message).lines() {
                if line.is_empty() {
                    text.push('\n');
                } else {
                    text.push_str(&format!("    {line}\n"));
                }
            }
            if options.show_notes {
                let notes = crate::commands::log::render_standard_notes(git_dir, format, oid)?;
                text.push_str(&String::from_utf8_lossy(&notes));
            }
            Ok(text)
        }
    }
}

fn diff_tree_pretty_notes(git_dir: &Path, format: ObjectFormat, oid: &ObjectId) -> Result<String> {
    let store = FileRefStore::new(git_dir, format);
    let handle = sley_notes::NotesRef::expand(sley_notes::DEFAULT_NOTES_REF);
    let mut notes =
        sley_notes::read_note_bytes(git_dir, format, &store, &handle, oid)?.unwrap_or_default();
    notes.push(b'\n');
    Ok(String::from_utf8_lossy(&notes).into_owned())
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
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    options: &DiffTreeOptions,
    line: &str,
) -> Result<Vec<DiffRequest>> {
    let mut parts = line.split_whitespace();
    let Some(first) = parts.next() else {
        // Blank lines are filtered by the caller; treat anything else as a skip.
        return Ok(vec![skip_echo(line)]);
    };
    let second = parts.next();
    if let Some(second) = second {
        let (Some(left), Some(right)) = (
            parse_full_oid(format, first),
            parse_full_oid(format, second),
        ) else {
            return Ok(vec![skip_echo(line)]);
        };
        let (Ok(left_tree), Ok(right_tree)) = (
            sley_rev::peel_to_tree(db, format, &left),
            sley_rev::peel_to_tree(db, format, &right),
        ) else {
            return Ok(vec![skip_echo(line)]);
        };
        // The two-tree stdin header echoes the input verbatim and is *not*
        // suppressed by --no-commit-id.
        Ok(vec![DiffRequest {
            left: Some(left_tree),
            right: Some(right_tree),
            header: Some(DiffHeader {
                text: line.to_string(),
                suppressible: false,
            }),
            ..Default::default()
        }])
    } else {
        let Some(oid) = parse_full_oid(format, first) else {
            return Ok(vec![skip_echo(line)]);
        };
        let Ok(object) = db.read_object(&oid) else {
            return Ok(vec![skip_echo(line)]);
        };
        if object.object_type != ObjectType::Commit {
            // A lone non-commit object id is not a valid single-token request:
            // git reports the error and prints no header for this line.
            eprintln!("error: Need exactly two trees, separated by a space");
            return Ok(vec![skip_silent()]);
        }
        single_commit_request(git_dir, format, db, options, &oid, first.to_string())
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
        header: Some(DiffHeader {
            text: line.to_string(),
            suppressible: false,
        }),
        skip: true,
        ..Default::default()
    }
}

/// A request that produces no output at all (no header, no diff). Used when git
/// reports an error for a line but does not echo it.
fn skip_silent() -> DiffRequest {
    DiffRequest {
        skip: true,
        ..Default::default()
    }
}

/// Execute and print one diff request. Returns `true` when there were
/// differences (so the caller can track an overall change flag).
struct DiffRequestContext<'a> {
    format: ObjectFormat,
    db: &'a FileObjectDatabase,
    options: &'a DiffTreeOptions,
    pathspecs: &'a [String],
    diff_pathspec: Option<DiffPathspec>,
    raw_abbrev: Option<usize>,
    patch_abbrev: usize,
    find_objects: &'a [ObjectId],
    /// Resolved `--indent-heuristic` / `diff.indentHeuristic`.
    indent_heuristic: bool,
    /// `--check` whitespace-rule resolver (only built in check mode).
    ws_resolver: Option<commands::diff::WhitespaceRuleResolver>,
    /// Accumulated `--check` failure status across all requests.
    check_failed: std::cell::Cell<bool>,
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
        if context.options.pretty == Some(DiffTreePretty::Medium)
            && context.options.output.patch
            && context.options.output.stat
        {
            write!(stdout, "{}", header.text)?;
        } else {
            writeln!(stdout, "{}", header.text)?;
        }
    }

    // git silently skips some requests (a root commit diffed without --root, an
    // unresolved stdin line): emit no diff after any header above.
    if request.skip {
        return Ok(false);
    }

    // Combined merge diff (`-c`/`--cc`): render the result tree against all
    // parents at once instead of the two-tree path below.
    if let Some(combined) = &request.combined {
        return run_combined_request(stdout, context, combined);
    }

    let Some(right) = request.right.clone() else {
        return Ok(false);
    };

    let recursive = context.options.recursive || context.options.output.forces_recursion();
    let entries = compute_entries(
        context.format,
        context.db,
        context.options,
        context.pathspecs,
        request.left.as_ref(),
        &right,
        recursive,
    )?;
    let entries = match context.diff_pathspec.as_ref() {
        Some(pathspec) => apply_diff_pathspec(entries, pathspec),
        None => entries,
    };
    let entries = if let Some(needle) = context.options.pickaxe.as_deref() {
        commands::diff::apply_diff_pickaxe(
            entries,
            needle.as_bytes(),
            context.options.pickaxe_all,
            context.db,
            None,
            false,
            None,
        )?
    } else {
        entries
    };
    let entries = commands::diff::apply_diff_find_objects(entries, context.find_objects);
    let has_differences = !entries.is_empty();

    // `--check`: report whitespace errors in place of the normal diff body.
    if context.options.check {
        if let Some(resolver) = &context.ws_resolver {
            let failed = commands::diff::run_diff_check(
                &entries, context.db, None, false, false, None, resolver,
            )?;
            if failed {
                context.check_failed.set(true);
            }
        }
        return Ok(has_differences);
    }

    let output = context.options.output;
    let mut wrote_block = false;
    if output.name_only {
        for entry in &entries {
            if context.options.z {
                stdout.write_all(&entry.path)?;
                stdout.write_all(b"\0")?;
            } else {
                let path = status_quote_path(&entry.path, false);
                writeln!(stdout, "{path}")?;
            }
        }
        wrote_block = true;
    }
    if output.name_status {
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
        wrote_block = true;
    }
    let stat_entries_for_render = if output.numstat || output.stat || output.shortstat {
        collect_diff_stat_entries(&entries, context.db, None, false)?
    } else {
        Vec::new()
    };

    if output.patch
        && output.stat
        && context.options.pretty == Some(DiffTreePretty::Medium)
        && !entries.is_empty()
    {
        writeln!(stdout, "---")?;
    }
    render_diff_entries(
        stdout,
        &entries,
        DiffEntryRenderModes {
            raw: output.raw,
            numstat: output.numstat,
            stat: output.stat,
            shortstat: output.shortstat,
            summary: output.summary,
            patch: output.patch && !entries.is_empty(),
        },
        DiffEntryRenderContext {
            raw: DiffEntryRawRenderOptions {
                z: context.options.z,
                abbrev: context.raw_abbrev,
                format: context.format,
            },
            stat: DiffEntryStatRenderOptions {
                source: Some(DiffEntryStatSource::Materialized(&stat_entries_for_render)),
                z: context.options.z,
                options: DiffStatOptions {
                    compact_summary: output.compact_summary,
                    stat_count: None,
                    color: false,
                    quote_path_fully: true,
                },
                // diff-tree is plumbing: fixed 80 columns, no config caps.
                widths: Some(DiffStatWidths::plumbing()),
            },
            after_stat: None,
            prefix_already_written: wrote_block,
        },
        |_| false,
        |stdout, entry| {
            let patch_options = DiffRenderOptions {
                binary: context.options.patch_binary,
                anchors: &[],
                allow_textconv: false,
                db: context.db,
                worktree_root: None,
                use_worktree_new: false,
                format: context.format,
                abbrev: context.patch_abbrev,
                src_prefix: &context.options.src_prefix,
                dst_prefix: &context.options.dst_prefix,
                context: 3,
                userdiff: None,
                funcname: None,
                colors: None,
                word_diff: None,
                no_index_contents: None,
                submodule_format: commands::diff_options::SubmoduleDiffFormat::Short,
                submodule_dirt: None,
                ws_error: None,
                color_moved: None,
                interhunk: 0,
                ws_ignore: context.options.ws_ignore,
                diff_algorithm: context.options.diff_algorithm,
                ignore_blank_lines: context.options.ignore_blank_lines,
                ignore_regexes: &context.options.ignore_regexes,
                line_ranges: None,
                indent_heuristic: context.indent_heuristic,
            };
            write_diff_patch_entry(stdout, entry, patch_options)
        },
    )?;

    Ok(has_differences)
}

/// Execute a combined merge diff (`-c`/`--cc`): the merge result diffed against
/// every parent simultaneously, delegating to the shared `commands::combined`
/// module (the same code `show`/`log` use). The stat/summary family is computed
/// solely against the first parent (git's STAT_FORMAT_MASK).
fn run_combined_request(
    stdout: &mut io::Stdout,
    context: &DiffRequestContext<'_>,
    combined: &CombinedRequest,
) -> Result<bool> {
    let format = context.format;
    let db = context.db;
    let mut paths = commands::combined::combined_paths(
        db,
        format,
        &combined.result_tree,
        &combined.parent_trees,
    )?;
    if !context.find_objects.is_empty() {
        paths.retain(|path| {
            commands::combined::combined_path_matches_find_objects(path, context.find_objects)
        });
    }

    let output = context.options.output;
    let mut has_differences = !paths.is_empty();
    let mut wrote_block = false;

    let render_ctx = commands::combined::CombinedRenderCtx {
        db,
        format,
        dense: context.options.combined.unwrap_or(true),
        all_paths: context.options.combined_all_paths,
        context: 3,
        ws_ignore: context.options.ws_ignore,
        diff_algorithm: context.options.diff_algorithm,
        src_prefix: &context.options.src_prefix,
        dst_prefix: &context.options.dst_prefix,
        patch_abbrev: context.patch_abbrev,
        raw_abbrev: context.raw_abbrev,
    };

    if output.name_only {
        for path in &paths {
            if context.options.z {
                stdout.write_all(&path.path)?;
                stdout.write_all(b"\0")?;
            } else {
                writeln!(stdout, "{}", status_quote_path(&path.path, false))?;
            }
        }
        wrote_block |= !paths.is_empty();
    }
    if output.name_status {
        for path in &paths {
            commands::combined::write_combined_name_status(stdout, path, context.options.z)?;
        }
        wrote_block |= !paths.is_empty();
    }
    if output.raw {
        for path in &paths {
            commands::combined::write_combined_raw(stdout, &render_ctx, path, context.options.z)?;
        }
        wrote_block |= !paths.is_empty();
    }

    // The stat / summary / numstat / shortstat family is computed against the
    // FIRST parent only (git's STAT_FORMAT_MASK).
    let stat_active = output.stat || output.numstat || output.shortstat || output.summary;
    if stat_active {
        let first_parent_entries = compute_entries(
            format,
            db,
            context.options,
            context.pathspecs,
            combined.parent_trees.first(),
            &combined.result_tree,
            true,
        )?;
        has_differences |= !first_parent_entries.is_empty();
        let stat_entries = if output.numstat || output.stat || output.shortstat {
            collect_diff_stat_entries(&first_parent_entries, db, None, false)?
        } else {
            Vec::new()
        };
        if output.numstat {
            for entry in &stat_entries {
                write_diff_numstat_materialized_entry(
                    stdout,
                    entry.entry,
                    entry.stats,
                    context.options.z,
                )?;
            }
        }
        if output.stat {
            write_diff_stat_materialized_with_widths(
                stdout,
                &stat_entries,
                DiffStatOptions {
                    compact_summary: output.compact_summary,
                    stat_count: None,
                    color: false,
                    quote_path_fully: true,
                },
                DiffStatWidths::plumbing(),
            )?;
        }
        if output.shortstat {
            write_diff_shortstat_materialized(stdout, &stat_entries)?;
        }
        if output.summary {
            for entry in &first_parent_entries {
                write_diff_summary_entry(stdout, entry)?;
            }
        }
        wrote_block |= !first_parent_entries.is_empty();
    }

    if output.patch && !paths.is_empty() {
        if wrote_block {
            writeln!(stdout)?;
        }
        for path in &paths {
            commands::combined::write_combined_patch(stdout, &render_ctx, path)?;
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
    pathspecs: &[String],
    left: Option<&ObjectId>,
    right: &ObjectId,
    recursive: bool,
) -> Result<Vec<sley_diff_merge::NameStatusEntry>> {
    if let Some(max_depth) = options.max_depth {
        let specs = DiffTreeDepthPathspecs::new(pathspecs);
        let mut entries =
            depth_limited_tree_changes(format, db, left, Some(right), &specs, max_depth)?;
        sort_entries_by_path(&mut entries);
        if options.reverse {
            entries = reverse_diff_entries(entries);
        }
        return Ok(entries);
    }
    if recursive {
        let name_status_options = sley_diff_merge::DiffNameStatusOptions {
            detect_renames: options.detect_renames,
            detect_copies: options.detect_copies,
            find_copies_harder: options.find_copies_harder,
            rename_empty: options.rename_empty,
            detect_inexact: options.detect_renames || options.detect_copies,
            rename_threshold: options.rename_threshold,
            copy_threshold: options.copy_threshold,
            rename_limit: 0,
            ..Default::default()
        };
        let mut entries = match left {
            Some(left) => sley_diff_merge::diff_name_status_trees_with_options(
                db,
                format,
                left,
                right,
                name_status_options,
            )?,
            None => sley_diff_merge::diff_name_status_empty_tree_with_options(
                db,
                format,
                right,
                name_status_options,
            )?,
        };
        if options.show_trees {
            // `-t` additionally surfaces the intermediate tree nodes that changed
            // between the two sides; merge them in and re-sort like git.
            let tree_entries = changed_tree_nodes(format, db, left, right)?;
            entries.extend(tree_entries);
            sort_entries_by_path(&mut entries);
        }
        if options.reverse {
            Ok(reverse_diff_entries(entries))
        } else {
            Ok(entries)
        }
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
        if options.reverse {
            entries = reverse_top_level_entries(entries);
        }
        Ok(entries)
    }
}

fn parse_diff_tree_max_depth(value: &str) -> Result<i64> {
    value.parse::<i64>().map_err(|_| {
        eprintln!("error: option `max-depth' expects a numerical value");
        GitError::Exit(129)
    })
}

fn diff_tree_has_wildcard_pathspec(pathspecs: &[String]) -> bool {
    pathspecs
        .iter()
        .any(|path| sley_worktree::pathspec_is_glob(path.as_bytes()))
}

#[derive(Debug)]
struct DiffTreeDepthPathspecs {
    specs: Vec<Vec<u8>>,
}

impl DiffTreeDepthPathspecs {
    fn new(pathspecs: &[String]) -> Self {
        let specs = pathspecs
            .iter()
            .map(|path| {
                let trimmed = path.trim_end_matches('/');
                if trimmed == "." {
                    Vec::new()
                } else {
                    trimmed.as_bytes().to_vec()
                }
            })
            .collect();
        Self { specs }
    }

    fn relative_depth(&self, path: &[u8]) -> Option<i64> {
        if self.specs.is_empty() {
            return Some(path_slash_depth(path));
        }
        self.specs
            .iter()
            .filter_map(|spec| relative_depth_from_spec(spec, path))
            .min()
    }

    fn is_ancestor_of_spec(&self, path: &[u8]) -> bool {
        self.specs.iter().any(|spec| {
            !path.is_empty()
                && spec.len() > path.len()
                && spec.starts_with(path)
                && spec.get(path.len()) == Some(&b'/')
        })
    }
}

fn relative_depth_from_spec(spec: &[u8], path: &[u8]) -> Option<i64> {
    if spec.is_empty() {
        return Some(path_slash_depth(path));
    }
    if path == spec {
        return Some(0);
    }
    if path.len() > spec.len() && path.starts_with(spec) && path.get(spec.len()) == Some(&b'/') {
        return Some(path_component_count(&path[spec.len() + 1..]));
    }
    None
}

fn path_slash_depth(path: &[u8]) -> i64 {
    path.iter().filter(|byte| **byte == b'/').count() as i64
}

fn path_component_count(path: &[u8]) -> i64 {
    if path.is_empty() {
        0
    } else {
        path.iter().filter(|byte| **byte == b'/').count() as i64 + 1
    }
}

fn depth_limited_tree_changes(
    format: ObjectFormat,
    db: &FileObjectDatabase,
    left: Option<&ObjectId>,
    right: Option<&ObjectId>,
    pathspecs: &DiffTreeDepthPathspecs,
    max_depth: i64,
) -> Result<Vec<sley_diff_merge::NameStatusEntry>> {
    let mut out = Vec::new();
    let context = DepthLimitedTreeContext {
        format,
        db,
        pathspecs,
        max_depth,
    };
    collect_depth_limited_tree_changes(&context, left, right, Vec::new(), &mut out)?;
    Ok(out)
}

struct DepthLimitedTreeContext<'a> {
    format: ObjectFormat,
    db: &'a FileObjectDatabase,
    pathspecs: &'a DiffTreeDepthPathspecs,
    max_depth: i64,
}

fn collect_depth_limited_tree_changes(
    context: &DepthLimitedTreeContext<'_>,
    left_tree: Option<&ObjectId>,
    right_tree: Option<&ObjectId>,
    prefix: Vec<u8>,
    out: &mut Vec<sley_diff_merge::NameStatusEntry>,
) -> Result<()> {
    let left_map = match left_tree {
        Some(oid) => top_level_entries(context.format, context.db, oid)?,
        None => BTreeMap::new(),
    };
    let right_map = match right_tree {
        Some(oid) => top_level_entries(context.format, context.db, oid)?,
        None => BTreeMap::new(),
    };
    let mut names = BTreeSet::new();
    names.extend(left_map.keys().cloned());
    names.extend(right_map.keys().cloned());

    for name in names {
        let left = left_map.get(&name);
        let right = right_map.get(&name);
        if left == right {
            continue;
        }
        let path = join_diff_tree_path(&prefix, &name);
        let left_is_tree = left.is_some_and(|entry| entry.mode == 0o040000);
        let right_is_tree = right.is_some_and(|entry| entry.mode == 0o040000);

        match (left_is_tree, right_is_tree) {
            (true, true) => {
                handle_depth_limited_tree_pair(context, left, right, path, out)?;
            }
            (true, false) => {
                handle_depth_limited_tree_side(context, left, None, path.clone(), out)?;
                if let Some(right) = right {
                    maybe_push_depth_limited_blob_change(
                        out,
                        path,
                        None,
                        Some(right),
                        context.pathspecs,
                        context.max_depth,
                    );
                }
            }
            (false, true) => {
                if let Some(left) = left {
                    maybe_push_depth_limited_blob_change(
                        out,
                        path.clone(),
                        Some(left),
                        None,
                        context.pathspecs,
                        context.max_depth,
                    );
                }
                handle_depth_limited_tree_side(context, None, right, path, out)?;
            }
            (false, false) => {
                maybe_push_depth_limited_blob_change(
                    out,
                    path,
                    left,
                    right,
                    context.pathspecs,
                    context.max_depth,
                );
            }
        }
    }
    Ok(())
}

fn handle_depth_limited_tree_pair(
    context: &DepthLimitedTreeContext<'_>,
    left: Option<&TopEntry>,
    right: Option<&TopEntry>,
    path: Vec<u8>,
    out: &mut Vec<sley_diff_merge::NameStatusEntry>,
) -> Result<()> {
    let left_oid = left.map(|entry| entry.oid);
    let right_oid = right.map(|entry| entry.oid);
    if context.max_depth < 0 || context.pathspecs.is_ancestor_of_spec(&path) {
        return collect_depth_limited_tree_changes(
            context,
            left_oid.as_ref(),
            right_oid.as_ref(),
            path,
            out,
        );
    }
    let Some(depth) = context.pathspecs.relative_depth(&path) else {
        return Ok(());
    };
    if depth >= context.max_depth {
        push_depth_limited_tree_change(out, path, left, right);
    } else {
        collect_depth_limited_tree_changes(
            context,
            left_oid.as_ref(),
            right_oid.as_ref(),
            path,
            out,
        )?;
    }
    Ok(())
}

fn handle_depth_limited_tree_side(
    context: &DepthLimitedTreeContext<'_>,
    left: Option<&TopEntry>,
    right: Option<&TopEntry>,
    path: Vec<u8>,
    out: &mut Vec<sley_diff_merge::NameStatusEntry>,
) -> Result<()> {
    if context.max_depth < 0 || context.pathspecs.is_ancestor_of_spec(&path) {
        return collect_depth_limited_tree_changes(
            context,
            left.map(|entry| &entry.oid),
            right.map(|entry| &entry.oid),
            path,
            out,
        );
    }
    let Some(depth) = context.pathspecs.relative_depth(&path) else {
        return Ok(());
    };
    if depth >= context.max_depth {
        push_depth_limited_tree_change(out, path, left, right);
    } else {
        collect_depth_limited_tree_changes(
            context,
            left.map(|entry| &entry.oid),
            right.map(|entry| &entry.oid),
            path,
            out,
        )?;
    }
    Ok(())
}

fn maybe_push_depth_limited_blob_change(
    out: &mut Vec<sley_diff_merge::NameStatusEntry>,
    path: Vec<u8>,
    left: Option<&TopEntry>,
    right: Option<&TopEntry>,
    pathspecs: &DiffTreeDepthPathspecs,
    max_depth: i64,
) {
    let Some(depth) = pathspecs.relative_depth(&path) else {
        return;
    };
    if max_depth >= 0 && depth > max_depth {
        return;
    }
    let status = match (left, right) {
        (None, Some(_)) => sley_diff_merge::NameStatus::Added,
        (Some(_), None) => sley_diff_merge::NameStatus::Deleted,
        (Some(left), Some(right)) => sley_diff_merge::modify_or_type_change(left.mode, right.mode),
        (None, None) => return,
    };
    out.push(sley_diff_merge::NameStatusEntry {
        status,
        path: BString::from(path),
        old_path: None,
        old_mode: left.map(|entry| entry.mode),
        new_mode: right.map(|entry| entry.mode),
        old_oid: left.map(|entry| entry.oid),
        new_oid: right.map(|entry| entry.oid),
    });
}

fn push_depth_limited_tree_change(
    out: &mut Vec<sley_diff_merge::NameStatusEntry>,
    path: Vec<u8>,
    left: Option<&TopEntry>,
    right: Option<&TopEntry>,
) {
    let status = match (left, right) {
        (None, Some(_)) => sley_diff_merge::NameStatus::Added,
        (Some(_), None) => sley_diff_merge::NameStatus::Deleted,
        (Some(left), Some(right)) => sley_diff_merge::modify_or_type_change(left.mode, right.mode),
        (None, None) => return,
    };
    out.push(sley_diff_merge::NameStatusEntry {
        status,
        path: BString::from(path),
        old_path: None,
        old_mode: left.map(|entry| entry.mode),
        new_mode: right.map(|entry| entry.mode),
        old_oid: left.map(|entry| entry.oid),
        new_oid: right.map(|entry| entry.oid),
    });
}

fn join_diff_tree_path(prefix: &[u8], name: &[u8]) -> Vec<u8> {
    let mut path = Vec::with_capacity(prefix.len() + 1 + name.len());
    path.extend_from_slice(prefix);
    if !path.is_empty() {
        path.push(b'/');
    }
    path.extend_from_slice(name);
    path
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
/// present on both sides with a different (mode, oid) is `Modified`, except
/// file/tree replacements: non-recursive diff-tree reports those as a delete and
/// an add at the same path rather than as a single typechange.
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
        if let (Some(l), Some(r)) = (l, r)
            && l != r
            && ((l.mode == 0o040000) != (r.mode == 0o040000))
        {
            if l.mode == 0o040000 {
                changes.push(top_level_added_entry(name.clone(), r));
                changes.push(top_level_deleted_entry(name, l));
            } else {
                changes.push(top_level_deleted_entry(name.clone(), l));
                changes.push(top_level_added_entry(name, r));
            }
            continue;
        }
        let status = match (l, r) {
            (None, Some(_)) => sley_diff_merge::NameStatus::Added,
            (Some(_), None) => sley_diff_merge::NameStatus::Deleted,
            (Some(l), Some(r)) if l != r => sley_diff_merge::NameStatus::Modified,
            _ => continue,
        };
        changes.push(sley_diff_merge::NameStatusEntry {
            status,
            path: BString::from(name),
            old_path: None,
            old_mode: l.map(|entry| entry.mode),
            new_mode: r.map(|entry| entry.mode),
            old_oid: l.map(|entry| entry.oid),
            new_oid: r.map(|entry| entry.oid),
        });
    }
    changes
}

fn top_level_added_entry(path: Vec<u8>, entry: &TopEntry) -> sley_diff_merge::NameStatusEntry {
    sley_diff_merge::NameStatusEntry {
        status: sley_diff_merge::NameStatus::Added,
        path: BString::from(path),
        old_path: None,
        old_mode: None,
        new_mode: Some(entry.mode),
        old_oid: None,
        new_oid: Some(entry.oid),
    }
}

fn top_level_deleted_entry(path: Vec<u8>, entry: &TopEntry) -> sley_diff_merge::NameStatusEntry {
    sley_diff_merge::NameStatusEntry {
        status: sley_diff_merge::NameStatus::Deleted,
        path: BString::from(path),
        old_path: None,
        old_mode: Some(entry.mode),
        new_mode: None,
        old_oid: Some(entry.oid),
        new_oid: None,
    }
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
            options.detect_copies,
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
    want_copies: bool,
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
        let Some(src_oid) = changes[src_idx].old_oid else {
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

    // Basename pre-pass (git's `find_basename_matches`): before the global
    // matrix, pair unique-basename src/dst at the stricter basename score so a
    // same-basename rename wins over a globally-more-similar different basename.
    // git skips this when copies are also wanted (`!want_copies`).
    if threshold <= 100 && !want_copies {
        let src_paths: Vec<&[u8]> = deleted.iter().map(|&i| &changes[i].path[..]).collect();
        let dst_paths: Vec<&[u8]> = added.iter().map(|&i| &changes[i].path[..]).collect();
        let basename_pairs = sley_diff_merge::basename_rename_matches(
            &src_paths,
            &dst_paths,
            &src_used,
            &dst_used,
            threshold,
            |si, di| {
                let src_oid = changes[deleted[si]].old_oid.as_ref()?;
                let dst_oid = changes[added[di]].new_oid.as_ref()?;
                if !rename_empty && (is_empty_blob_oid(src_oid) || is_empty_blob_oid(dst_oid)) {
                    return None;
                }
                let src_bytes = read_blob_for_similarity(db, src_oid)?;
                let dst_bytes = read_blob_for_similarity(db, dst_oid)?;
                Some(sley_diff_merge::blob_similarity(&src_bytes, &dst_bytes))
            },
        );
        for (si, di, score) in basename_pairs {
            src_used[si] = true;
            dst_used[di] = true;
            assigned.insert(added[di], (deleted[si], score));
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
                oid: src_entry.old_oid,
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
    path: BString,
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
                oid: *oid,
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
        let Some(dst_oid) = entry.new_oid else {
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
            entry.old_oid = Some(source.oid);
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
            entry.old_oid = Some(source.oid);
        }
    }
    changes
}

/// A candidate copy source: the old-side path/mode/oid of a left-side blob.
struct CopySource {
    path: BString,
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
    entries.sort_by(|a, b| {
        sley_object::tree_entry_cmp(
            a.path.as_bytes(),
            diff_tree_entry_sort_mode(a),
            b.path.as_bytes(),
            diff_tree_entry_sort_mode(b),
        )
        .then_with(|| a.path.len().cmp(&b.path.len()))
        .then_with(|| diff_tree_entry_tree_rank(a).cmp(&diff_tree_entry_tree_rank(b)))
        .then_with(|| a.status.code().cmp(&b.status.code()))
    });
}

fn reverse_top_level_entries(
    entries: Vec<sley_diff_merge::NameStatusEntry>,
) -> Vec<sley_diff_merge::NameStatusEntry> {
    let mut reversed = entries
        .into_iter()
        .map(reverse_diff_entry)
        .collect::<Vec<_>>();
    sort_entries_by_path(&mut reversed);
    reversed
}

fn diff_tree_entry_sort_mode(entry: &sley_diff_merge::NameStatusEntry) -> u32 {
    entry.new_mode.or(entry.old_mode).unwrap_or(0)
}

fn diff_tree_entry_tree_rank(entry: &sley_diff_merge::NameStatusEntry) -> u8 {
    match diff_tree_entry_primary_mode(entry) {
        0o040000 => 1,
        _ => 0,
    }
}

fn diff_tree_entry_primary_mode(entry: &sley_diff_merge::NameStatusEntry) -> u32 {
    match entry.status {
        sley_diff_merge::NameStatus::Added => entry.new_mode.or(entry.old_mode),
        sley_diff_merge::NameStatus::Deleted => entry.old_mode.or(entry.new_mode),
        _ => entry.new_mode.or(entry.old_mode),
    }
    .unwrap_or(0)
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
            path: BString::from(path.clone()),
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
