//! `git diff-files` — compare files in the working tree against the index.
//!
//! This is the plumbing counterpart to porcelain `git diff` (with neither
//! `--cached` nor a tree argument): it always diffs the *index* against the
//! *working tree*. The two visible behavioural differences from `git diff` are
//! the plumbing defaults — output is the diff-raw format unless an output mode
//! such as `-p`/`--stat` is requested, and raw object names are printed in full
//! (porcelain `git diff --raw` abbreviates them) unless `--abbrev` is given.
//!
//! The actual rendering (raw, patch, stat, numstat, shortstat, summary,
//! name-only, name-status) and the index-vs-worktree name-status engine call
//! are shared with the crate-root `diff` implementation; this module only owns
//! the `diff-files`-specific argument parser and dispatch. Shared plumbing is
//! pulled in via the crate-root glob; see `commands::stash` for the rationale.
use crate::*;

/// Usage text emitted for `-h` (to stdout) and on a parse error (to stderr),
/// matching `git diff-files`'s built-in usage. Kept as a single block so both
/// paths stay byte-for-byte identical to upstream.
const DIFF_FILES_USAGE: &str =
    "usage: git diff-files [-q] [-0 | -1 | -2 | -3 | -c | --cc] [<common-diff-options>] [<path>...]

common diff options:
  -z            output diff-raw with lines terminated with NUL.
  -p            output patch format.
  -u            synonym for -p.
  --patch-with-raw
                output both a patch and the diff-raw format.
  --stat        show diffstat instead of patch.
  --numstat     show numeric diffstat instead of patch.
  --patch-with-stat
                output a patch and prepend its diffstat.
  --name-only   show only names of changed files.
  --name-status show names and status of changed files.
  --full-index  show full object name on index lines.
  --abbrev=<n>  abbreviate object names in diff-tree header and diff-raw.
  -R            swap input file pairs.
  -B            detect complete rewrites.
  -M            detect renames.
  -C            detect copies.
  --find-copies-harder
                try unchanged files as candidate for copy detection.
  -l<n>         limit rename attempts up to <n> paths.
  -O<file>      reorder diffs according to the <file>.
  -S<string>    find filepair whose only one side contains the string.
  --pickaxe-all
                show all files diff when -S is used and hit is found.
  -a  --text    treat all files as text.

";

/// Print the usage block to stdout and return the `-h` exit (code 129, matching
/// git's `parse-options` short-help behaviour). Returned as a `GitError` so the
/// parser can `return Err(diff_files_help())` from any branch.
fn diff_files_help() -> GitError {
    print!("{DIFF_FILES_USAGE}");
    GitError::Exit(129)
}

/// Print the usage block to stderr and signal a usage error (exit code 129),
/// matching git when an unrecognised option is supplied.
fn diff_files_usage_error() -> GitError {
    eprint!("{DIFF_FILES_USAGE}");
    GitError::Exit(129)
}

/// Resolved set of `diff-files` options after argument parsing. Mirrors the
/// subset of `git diff` knobs that `diff-files` honours.
struct DiffFilesOptions {
    name_status: bool,
    name_only: bool,
    quiet: bool,
    exit_code: bool,
    summary: bool,
    raw: bool,
    stat: bool,
    compact_summary: bool,
    stat_count: Option<usize>,
    numstat: bool,
    shortstat: bool,
    patch: bool,
    no_patch: bool,
    reverse: bool,
    combined: bool,
    z: bool,
    // `Some(None)` means `--no-abbrev`/full width; `Some(Some(n))` means abbreviate
    // raw object names to `n`; `None` means "default", which for diff-files is full.
    raw_abbrev: Option<Option<usize>>,
    patch_abbrev: Option<usize>,
    patch_full_index: bool,
    detect_renames: bool,
    detect_copies: bool,
    find_copies_harder: bool,
    rename_empty: bool,
    inexact_renames: bool,
    rename_threshold: u8,
    copy_threshold: u8,
    src_prefix: String,
    dst_prefix: String,
    diff_filter: DiffFilter,
    path_args: Vec<String>,
}

impl Default for DiffFilesOptions {
    fn default() -> Self {
        Self {
            name_status: false,
            name_only: false,
            quiet: false,
            exit_code: false,
            summary: false,
            raw: false,
            stat: false,
            compact_summary: false,
            stat_count: None,
            numstat: false,
            shortstat: false,
            patch: false,
            no_patch: false,
            reverse: false,
            combined: false,
            z: false,
            raw_abbrev: None,
            patch_abbrev: None,
            patch_full_index: false,
            // git enables rename detection by default (diff.renames defaults to
            // true). -M/-C pick the similarity thresholds; --no-renames disables.
            detect_renames: true,
            detect_copies: false,
            find_copies_harder: false,
            rename_empty: true,
            inexact_renames: true,
            rename_threshold: sley_diff_merge::DEFAULT_RENAME_THRESHOLD,
            copy_threshold: sley_diff_merge::DEFAULT_RENAME_THRESHOLD,
            src_prefix: "a/".to_string(),
            dst_prefix: "b/".to_string(),
            diff_filter: DiffFilter::default(),
            path_args: Vec::new(),
        }
    }
}

pub(crate) fn cmd_diff_files(args: &[String]) -> Result<()> {
    let options = parse_diff_files_args(args)?;
    run_diff_files(options)
}

/// Parse `diff-files` arguments. Output-mode and rename/copy flags share their
/// meaning with `git diff`; plumbing-only selectors (`-q`, stage selectors) are
/// accepted so that scripts written against plumbing keep working, and unknown
/// options produce git's usage error. Mode-compatibility checks (e.g. `-R` and
/// combined diffs) are deferred to [`run_diff_files`].
fn parse_diff_files_args(args: &[String]) -> Result<DiffFilesOptions> {
    let mut o = DiffFilesOptions::default();
    let mut positional_only = false;
    let mut idx = 0;
    while idx < args.len() {
        let arg = &args[idx];
        if positional_only {
            o.path_args.push(arg.clone());
            idx += 1;
            continue;
        }
        match arg.as_str() {
            "--" => positional_only = true,
            "-h" | "--help" => return Err(diff_files_help()),
            "--name-status" => {
                if o.no_patch {
                    return Err(diff_files_name_select_conflict());
                }
                o.name_status = true;
            }
            "--name-only" => {
                if o.no_patch {
                    return Err(diff_files_name_select_conflict());
                }
                o.name_only = true;
            }
            // In diff-files, `-q` is NOT `--quiet`: it only means "remain silent
            // even for nonexistent files". Since the index-vs-worktree engine
            // already tolerates missing worktree files, it is a no-op here.
            "-q" => {}
            "--quiet" => o.quiet = true,
            "--exit-code" => o.exit_code = true,
            "--summary" => {
                o.summary = true;
                o.no_patch = false;
            }
            "--raw" => {
                o.raw = true;
                o.no_patch = false;
            }
            "--stat" => {
                o.stat = true;
                o.no_patch = false;
            }
            "--compact-summary" => {
                o.compact_summary = true;
                o.no_patch = false;
            }
            "--numstat" => {
                o.numstat = true;
                o.no_patch = false;
            }
            "--shortstat" => {
                o.shortstat = true;
                o.no_patch = false;
            }
            "-p" | "-u" | "--patch" => {
                o.patch = true;
                o.no_patch = false;
            }
            "--patch-with-raw" => {
                o.raw = true;
                o.patch = true;
                o.no_patch = false;
            }
            "--patch-with-stat" => {
                o.stat = true;
                o.patch = true;
                o.no_patch = false;
            }
            "-s" | "--no-patch" => {
                o.name_status = false;
                o.name_only = false;
                o.summary = false;
                o.raw = false;
                o.stat = false;
                o.compact_summary = false;
                o.numstat = false;
                o.shortstat = false;
                o.patch = false;
                o.no_patch = true;
            }
            "-a" | "--text" | "--no-ext-diff" | "--no-textconv" => {}
            "-R" => o.reverse = true,
            // Stage selectors choose which index stage to compare against the
            // working tree. Without an in-progress merge there are no unmerged
            // stages, so against the ordinary stage-0 index they are no-ops, which
            // matches git for the common case and keeps plumbing callers working.
            "-0" | "-1" | "-2" | "-3" => {}
            // Combined-diff formats are only meaningful for unmerged (conflicted)
            // index entries, which this index-vs-worktree path does not model.
            // Reject rather than emit a non-combined diff that git would not.
            "-c" | "--cc" => o.combined = true,
            "--abbrev" => {
                o.raw_abbrev = Some(Some(7));
                o.patch_abbrev = Some(7);
            }
            "--no-abbrev" => o.raw_abbrev = Some(None),
            "--full-index" => o.patch_full_index = true,
            "--no-prefix" => {
                o.src_prefix.clear();
                o.dst_prefix.clear();
            }
            "--default-prefix" => {
                o.src_prefix = "a/".to_string();
                o.dst_prefix = "b/".to_string();
            }
            "--src-prefix" => {
                idx += 1;
                o.src_prefix = args
                    .get(idx)
                    .ok_or_else(|| GitError::Command("--src-prefix requires a value".into()))?
                    .clone();
            }
            "--dst-prefix" => {
                idx += 1;
                o.dst_prefix = args
                    .get(idx)
                    .ok_or_else(|| GitError::Command("--dst-prefix requires a value".into()))?
                    .clone();
            }
            "-z" => o.z = true,
            "-M" | "--find-renames" => {
                o.detect_renames = true;
                o.inexact_renames = true;
            }
            "-C" | "--find-copies" => {
                o.detect_copies = true;
                o.inexact_renames = true;
            }
            "--find-copies-harder" => {
                o.detect_copies = true;
                o.find_copies_harder = true;
                o.inexact_renames = true;
            }
            "--no-find-copies-harder" => o.find_copies_harder = false,
            "--no-renames" => {
                o.detect_renames = false;
                o.inexact_renames = false;
            }
            "--rename-empty" => o.rename_empty = true,
            "--no-rename-empty" => o.rename_empty = false,
            value if value.starts_with("-M") && value.len() > 2 => {
                log_validate_similarity_option(&value[2..], "find-renames")?;
                o.detect_renames = true;
                o.inexact_renames = true;
                o.rename_threshold = parse_similarity_threshold(&value[2..]);
            }
            value if let Some(value) = value.strip_prefix("--find-renames=") => {
                log_validate_similarity_option(value, "find-renames")?;
                o.detect_renames = true;
                o.inexact_renames = true;
                o.rename_threshold = parse_similarity_threshold(value);
            }
            value if value.starts_with("-C") && value.len() > 2 => {
                log_validate_similarity_option(&value[2..], "find-copies")?;
                o.detect_copies = true;
                o.inexact_renames = true;
                o.copy_threshold = parse_similarity_threshold(&value[2..]);
            }
            value if let Some(value) = value.strip_prefix("--find-copies=") => {
                log_validate_similarity_option(value, "find-copies")?;
                o.detect_copies = true;
                o.inexact_renames = true;
                o.copy_threshold = parse_similarity_threshold(value);
            }
            "-l" => {
                idx += 1;
                let value = args
                    .get(idx)
                    .ok_or_else(diff_rename_limit_requires_integer_error)?;
                validate_diff_rename_limit(value)?;
            }
            value if let Some(value) = value.strip_prefix("-l") => {
                validate_diff_rename_limit(value)?;
            }
            "--diff-filter" => {
                idx += 1;
                let value = args
                    .get(idx)
                    .ok_or_else(|| GitError::Command("--diff-filter requires a value".into()))?;
                o.diff_filter = parse_diff_filter(value)?;
            }
            value if let Some(value) = value.strip_prefix("--diff-filter=") => {
                o.diff_filter = parse_diff_filter(value)?;
            }
            value
                if value.starts_with("--stat=")
                    || value.starts_with("--stat-width=")
                    || value.starts_with("--stat-name-width=")
                    || value.starts_with("--stat-graph-width=")
                    || value.starts_with("--stat-count=") =>
            {
                o.stat = true;
                o.no_patch = false;
                if let Some(count) = diff_stat_count_option(value)? {
                    o.stat_count = count;
                }
            }
            value if let Some(value) = value.strip_prefix("--abbrev=") => {
                let width = parse_abbrev(value)?.max(4);
                o.raw_abbrev = Some(Some(width));
                o.patch_abbrev = Some(width);
            }
            value if let Some(value) = value.strip_prefix("--src-prefix=") => {
                o.src_prefix = value.to_string();
            }
            value if let Some(value) = value.strip_prefix("--dst-prefix=") => {
                o.dst_prefix = value.to_string();
            }
            value if !value.starts_with('-') => o.path_args.push(arg.clone()),
            _ => return Err(diff_files_usage_error()),
        }
        idx += 1;
    }
    if o.name_status && o.name_only {
        return Err(diff_files_name_select_conflict());
    }
    Ok(o)
}

/// git's fatal error when more than one of `--name-only`/`--name-status`/
/// `--check`/`-s` is requested. Printed with the `fatal:` prefix and exit 128 to
/// match the upstream `git diff-files` message exactly.
fn diff_files_name_select_conflict() -> GitError {
    eprintln!(
        "fatal: options '--name-only', '--name-status', '--check', and '-s' cannot be used together"
    );
    GitError::Exit(128)
}

/// Run the index-vs-worktree diff and render it according to `options`.
fn run_diff_files(o: DiffFilesOptions) -> Result<()> {
    // Combined-diff output (`-c`/`--cc`) requires unmerged index stages, which
    // this index-vs-worktree path does not reconstruct; reject it rather than
    // print a non-combined diff that upstream git would never emit here.
    if o.combined {
        return Err(GitError::Unsupported(
            "diff combined output is not supported".into(),
        ));
    }
    // `-R` (swap sides) is only wired up for the name-oriented output modes,
    // mirroring the crate's `diff`/`diff-index` handling. Other modes would need
    // worktree content on the reversed (old) side, which the shared raw/patch
    // renderers do not provide.
    if o.reverse && !o.name_status && !o.name_only {
        return Err(GitError::Unsupported(
            "diff reverse output is not supported for this output mode".into(),
        ));
    }
    let repo = RepositoryContext::discover_current()?;
    let cwd = repo.cwd();
    let git_dir = repo.git_dir();
    let format = repo.format();
    let db = repo.objects();
    let worktree_root = repo.worktree_root()?;

    // Raw object names: diff-files (plumbing) prints full names by default, so a
    // `None` here means "full". `--abbrev`/`--abbrev=<n>` set an explicit width;
    // `--no-abbrev` forces full. (Porcelain `git diff --raw` would default to
    // core.abbrev here instead.)
    let raw_abbrev = match o.raw_abbrev {
        Some(abbrev) => abbrev.map(|width| width.min(format.hex_len())),
        None => None,
    };
    // Patch `index` lines abbreviate to core.abbrev (default 7) unless
    // --full-index requests the full name or --abbrev overrides the width.
    let patch_abbrev = if o.patch_full_index {
        format.hex_len()
    } else {
        o.patch_abbrev
            .or(repo.abbrev()?)
            .unwrap_or(7)
            .min(format.hex_len())
    };

    let pathspec = if o.path_args.is_empty() {
        DiffPathspec::default()
    } else {
        DiffPathspec::new(cwd, worktree_root, &o.path_args)?
    };

    let name_status_options = sley_diff_merge::DiffNameStatusOptions {
        detect_renames: o.detect_renames,
        detect_copies: o.detect_copies,
        find_copies_harder: o.find_copies_harder,
        rename_empty: o.rename_empty,
    };
    let rename_options = sley_diff_merge::RenameDetectionOptions {
        base: name_status_options,
        detect_inexact: true,
        rename_threshold: o.rename_threshold,
        copy_threshold: o.copy_threshold,
    };

    // `git diff-files` selects changed paths by the cached *stat*, not by content:
    // it does NOT refresh the index, so a stat-dirty entry whose content is
    // unchanged (a `touch`ed file, or a freshly `rm --cached`-then-`reset
    // --no-refresh` entry with a zeroed cached stat) is still reported `M`. The
    // dedicated diff-files engine layers that stat-based selection over the
    // content diff; porcelain `git diff` (which refreshes first) keeps the plain
    // content engine.
    let entries = if o.inexact_renames {
        sley_diff_merge::diff_name_status_index_worktree_for_diff_files_with_rename_options(
            worktree_root,
            git_dir,
            format,
            rename_options,
        )?
    } else {
        sley_diff_merge::diff_name_status_index_worktree_for_diff_files_with_options(
            worktree_root,
            git_dir,
            format,
            name_status_options,
        )?
    };

    let entries = apply_diff_pathspec(entries, &pathspec);
    let entries = if o.reverse {
        reverse_diff_entries(entries)
    } else {
        entries
    };
    let entries: Vec<_> = if o.diff_filter.all_or_none {
        if !o.diff_filter.includes.is_empty()
            && entries.iter().any(|entry| {
                pathspec.matches(&entry.path) && o.diff_filter.matches_status(entry.status.code())
            })
        {
            entries
        } else {
            Vec::new()
        }
    } else {
        entries
            .into_iter()
            .filter(|entry| o.diff_filter.matches_status(entry.status.code()))
            .collect()
    };

    let has_differences = !entries.is_empty();
    if !o.quiet && !o.no_patch {
        render_diff_files_entries(
            &entries,
            &o,
            db,
            worktree_root,
            raw_abbrev,
            patch_abbrev,
            format,
        )?;
    }
    if (o.quiet || o.exit_code) && has_differences {
        return Err(GitError::Exit(1));
    }
    Ok(())
}

/// Emit the selected output mode(s) for the diffed entries. The new side is
/// always the working tree, so OIDs for it are reported as zeros in raw mode and
/// blob content is read from disk for patch/stat/numstat.
fn render_diff_files_entries(
    entries: &[sley_diff_merge::NameStatusEntry],
    o: &DiffFilesOptions,
    db: &FileObjectDatabase,
    worktree_root: &Path,
    raw_abbrev: Option<usize>,
    patch_abbrev: usize,
    format: ObjectFormat,
) -> Result<()> {
    let mut stdout = io::stdout();
    // diff-files always compares the working tree, so the new-side object is the
    // (unrecorded) worktree blob: raw output zeroes its OID, and patch/stat read
    // its bytes from disk.
    let zero_worktree_oids = true;
    let use_worktree_new = true;
    let worktree_root = Some(worktree_root);

    let show_raw = o.raw && !o.name_only && !o.name_status;
    let show_numstat = o.numstat && !o.name_only && !o.name_status;
    let show_stat = (o.stat || o.compact_summary) && !o.name_only && !o.name_status;
    let show_shortstat = o.shortstat && !o.name_only && !o.name_status;
    // With no explicit output mode, diff-files defaults to diff-raw (unlike
    // porcelain `git diff`, which defaults to a patch).
    let no_output_mode = !o.raw
        && !o.stat
        && !o.compact_summary
        && !o.numstat
        && !o.shortstat
        && !o.summary
        && !o.name_status
        && !o.name_only;
    let show_patch = !o.name_only && !o.name_status && o.patch;
    let show_default_raw = no_output_mode && !o.patch;
    let show_summary = o.summary && !o.name_only && !o.name_status;

    if show_raw || show_default_raw {
        for entry in entries {
            write_diff_raw_entry(
                &mut stdout,
                entry,
                o.z,
                zero_worktree_oids,
                raw_abbrev,
                format,
            )?;
        }
    }
    // Stat-family output (numstat/stat/shortstat) reflects *content*, so — like
    // git's diffcore, which drops unmodified pairs before the stat walk — the
    // stat-dirty-but-content-identical entries (a `touch`ed / `reset
    // --no-refresh`-restored file: shown `M` in raw/name-status, empty in stat)
    // must be excluded. The raw and name output keep the full set.
    let content_entries = if show_numstat || show_stat || show_shortstat {
        collect_diff_stat_entries(entries, db, worktree_root, use_worktree_new)?
            .into_iter()
            .filter(diff_files_stat_entry_has_content_change)
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    if show_numstat {
        for entry in &content_entries {
            write_diff_numstat_materialized_entry(&mut stdout, entry.entry, entry.stats, o.z)?;
        }
    }
    if show_stat {
        write_diff_stat_materialized(
            &mut stdout,
            &content_entries,
            DiffStatOptions {
                compact_summary: o.compact_summary,
                stat_count: o.stat_count,
                color: false,
            },
        )?;
    }
    if show_shortstat {
        write_diff_shortstat_materialized(&mut stdout, &content_entries)?;
    }
    if show_summary {
        for entry in entries {
            write_diff_summary_entry(&mut stdout, entry)?;
        }
    }
    if show_patch {
        // When a patch is combined with raw/stat/summary output, git separates
        // the two blocks with a blank line.
        if show_raw || show_numstat || show_stat || show_shortstat || show_summary {
            writeln!(stdout)?;
        }
        for entry in entries {
            let patch_options = DiffPatchOptions {
                db,
                worktree_root,
                use_worktree_new,
                format,
                abbrev: patch_abbrev,
                src_prefix: &o.src_prefix,
                dst_prefix: &o.dst_prefix,
                context: 3,
                userdiff: None,
                colors: None,
                word_diff: None,
                no_index_contents: None,
                dirty_submodules: None,
                ws_error_rule: None,
            };
            write_diff_patch_entry(&mut stdout, entry, patch_options)?;
        }
    } else if o.name_only || o.name_status {
        for entry in entries {
            write_diff_files_name_entry(&mut stdout, entry, o.name_only, o.z)?;
        }
    }
    Ok(())
}

/// Whether `entry` is a real content/mode change for the purpose of stat-family
/// output — i.e. NOT one of the unmodified pairs git's diffcore drops before the
/// stat walk. A plain `Modified` entry whose old/new content and mode are
/// identical (a stat-dirty-but-content-unchanged `diff-files` entry) returns
/// `false`; every other entry (adds, deletes, real modifies, renames, copies, or
/// mode flips) returns `true`. Mirrors the suppression in `write_diff_patch_entry`.
fn diff_files_stat_entry_has_content_change(data: &DiffStatEntryData<'_>) -> bool {
    let entry = data.entry;
    if !matches!(entry.status, sley_diff_merge::NameStatus::Modified) {
        return true;
    }
    let mode_unchanged = match (entry.old_mode, entry.new_mode) {
        (Some(old_mode), Some(new_mode)) => old_mode == new_mode,
        _ => true,
    };
    if !mode_unchanged {
        return true;
    }
    match data.stats {
        DiffLineStats::Binary { unchanged, .. } => !unchanged,
        DiffLineStats::Text { inserted, deleted } => inserted != 0 || deleted != 0,
    }
}

/// Render a single entry for `--name-only` / `--name-status`, honouring `-z`
/// (NUL-terminated, unquoted) vs the default tab/newline (quoted) layout.
fn write_diff_files_name_entry(
    stdout: &mut io::Stdout,
    entry: &sley_diff_merge::NameStatusEntry,
    name_only: bool,
    z: bool,
) -> Result<()> {
    if z {
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
    } else {
        write!(stdout, "{}", entry.status.label())?;
        if let Some(old_path) = &entry.old_path {
            let old_path = status_quote_path(old_path, false);
            write!(stdout, "\t{old_path}")?;
        }
        let path = status_quote_path(&entry.path, false);
        writeln!(stdout, "\t{path}")?;
    }
    Ok(())
}
