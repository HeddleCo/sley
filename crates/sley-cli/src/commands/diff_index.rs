//! `git diff-index` — compare a tree-ish against the index (and optionally the
//! working tree).
//!
//! This is the plumbing sibling of `git diff`. It always takes a `<tree-ish>`
//! argument and diffs it against either the cached index (`--cached`) or the
//! working tree (the default). Unlike `git diff`, the default output format is
//! the raw `:<oldmode> <newmode> <oldoid> <newoid> <status>\t<path>` listing,
//! not a patch, and rename/copy detection is strictly opt-in via `-M`/`-C`
//! (the `diff.renames` config is intentionally ignored, matching git).
//!
//! The heavy lifting is shared with `git diff`: the change set is computed by
//! `sley_diff_merge`'s tree-vs-index / tree-vs-worktree name-status engines, and
//! the output is rendered through the same crate-root helpers that back
//! `cmd_diff` (`write_diff_raw_entry`, `write_diff_patch_entry`,
//! `write_diff_stat`, etc.). This keeps every output mode byte-identical with
//! `git diff` for the formats both commands share.

use std::io::{self, Write};
use std::path::Path;

use sley_core::{GitError, ObjectFormat, ObjectId, Result};

// Pull in the crate-root helpers this command shares with `cmd_diff`
// (RepositoryContext, FileObjectDatabase, the
// DiffPathspec/DiffFilter/DiffStatOptions/DiffPatchOptions types, and every
// write_diff_* renderer), matching the established `commands::*` pattern.
use crate::*;

/// Output selection for `git diff-index`. Several of these can be combined
/// (e.g. `--patch-with-raw`, `--patch-with-stat`); the default — when no format
/// flag is given — is the raw listing.
#[derive(Default)]
struct DiffIndexOutput {
    raw: bool,
    patch: bool,
    name_status: bool,
    name_only: bool,
    stat: bool,
    compact_summary: bool,
    numstat: bool,
    shortstat: bool,
    summary: bool,
}

pub(crate) fn cmd_diff_index(args: &[String]) -> Result<()> {
    let mut output = DiffIndexOutput::default();
    let mut cached = false;
    let mut match_missing = false;
    let mut quiet = false;
    let mut exit_code = false;
    let mut check = false;
    let mut z = false;
    let mut reverse = false;
    let mut detect_renames = false;
    let mut detect_copies = false;
    let mut find_copies_harder = false;
    let mut rename_empty = true;
    let mut inexact_renames = false;
    let mut rename_threshold = sley_diff_merge::DEFAULT_RENAME_THRESHOLD;
    let mut copy_threshold = sley_diff_merge::DEFAULT_RENAME_THRESHOLD;
    let mut diff_filter = DiffFilter::default();
    let mut abbrev = AbbrevRequest::Default;
    let mut patch_full_index = false;
    let mut merge_base = false;
    let mut src_prefix = "a/".to_string();
    let mut dst_prefix = "b/".to_string();
    let mut indent_heuristic: Option<bool> = None;
    let mut submodule_format = commands::diff_options::SubmoduleDiffFormat::Short;
    let mut ignore_submodules_cli: Option<SubmoduleIgnoreMode> = None;
    let mut setup_args: Vec<String> = Vec::new();
    let mut positional_only = false;
    let mut idx = 0;
    while idx < args.len() {
        let arg = &args[idx];
        if positional_only {
            setup_args.push(arg.clone());
            idx += 1;
            continue;
        }
        match arg.as_str() {
            "-h" | "--help" => return diff_index_help(),
            "--" => {
                setup_args.push(arg.clone());
                positional_only = true;
            }
            "--cached" | "--staged" => cached = true,
            "--quiet" => {
                quiet = true;
                exit_code = true;
            }
            "--exit-code" => exit_code = true,
            "--check" => check = true,
            "-z" => z = true,
            "-R" => reverse = true,
            // In diff-index, `-m` is "match missing": missing worktree files
            // are treated as matching the index/tree side and omitted.
            "-m" => match_missing = true,
            "--merge-base" => merge_base = true,
            "-p" | "-u" | "--patch" => output.patch = true,
            "--raw" => output.raw = true,
            "--name-status" => output.name_status = true,
            "--name-only" => output.name_only = true,
            "--stat" => output.stat = true,
            "--compact-summary" => {
                output.stat = true;
                output.compact_summary = true;
            }
            "--numstat" => output.numstat = true,
            "--shortstat" => output.shortstat = true,
            "--summary" => output.summary = true,
            "--patch-with-raw" => {
                output.patch = true;
                output.raw = true;
            }
            "--patch-with-stat" => {
                output.patch = true;
                output.stat = true;
            }
            // Text/whitespace toggles that do not change the formats we emit.
            "-a" | "--text" | "--no-ext-diff" | "--no-textconv" | "--no-color" => {}
            "-M" | "--find-renames" => {
                detect_renames = true;
                inexact_renames = true;
            }
            // Copy detection always implies rename detection (git scores
            // rename candidates before copies), so `-C`/`--find-copies-harder`
            // enable both even without an explicit `-M`.
            "-C" | "--find-copies" => {
                detect_renames = true;
                detect_copies = true;
                inexact_renames = true;
            }
            "--find-copies-harder" => {
                detect_renames = true;
                detect_copies = true;
                find_copies_harder = true;
                inexact_renames = true;
            }
            "--no-renames" => {
                detect_renames = false;
                detect_copies = false;
                inexact_renames = false;
            }
            "--rename-empty" => rename_empty = true,
            "--no-rename-empty" => rename_empty = false,
            "--indent-heuristic" => indent_heuristic = Some(true),
            "--no-indent-heuristic" => indent_heuristic = Some(false),
            "-B" | "--break-rewrites" => {}
            "--full-index" => patch_full_index = true,
            "--submodule" => submodule_format = commands::diff_options::SubmoduleDiffFormat::Log,
            value if let Some(value) = value.strip_prefix("--submodule=") => {
                log_validate_submodule_format(value)?;
                submodule_format = commands::diff_options::SubmoduleDiffFormat::parse(value);
            }
            "--ignore-submodules" => ignore_submodules_cli = Some(SubmoduleIgnoreMode::All),
            value if let Some(value) = value.strip_prefix("--ignore-submodules=") => {
                let Some(mode) = parse_submodule_ignore_mode(value) else {
                    eprintln!("fatal: bad --ignore-submodules argument: {value}");
                    return Err(GitError::Exit(128));
                };
                ignore_submodules_cli = Some(mode);
            }
            "--abbrev" => abbrev = AbbrevRequest::Auto,
            "--no-abbrev" => abbrev = AbbrevRequest::None,
            "--no-prefix" => {
                src_prefix.clear();
                dst_prefix.clear();
            }
            "--default-prefix" => {
                src_prefix = "a/".to_string();
                dst_prefix = "b/".to_string();
            }
            "--src-prefix" => {
                idx += 1;
                src_prefix = take_value(args, idx, "--src-prefix")?.to_string();
            }
            "--dst-prefix" => {
                idx += 1;
                dst_prefix = take_value(args, idx, "--dst-prefix")?.to_string();
            }
            "--diff-filter" => {
                idx += 1;
                diff_filter = parse_diff_filter(take_value(args, idx, "--diff-filter")?)?;
            }
            value if let Some(value) = value.strip_prefix("--diff-filter=") => {
                diff_filter = parse_diff_filter(value)?;
            }
            "--max-depth" => {
                idx += 1;
                let value = take_value(args, idx, "--max-depth")?;
                if value != "-1" {
                    return diff_index_usage_error();
                }
            }
            value if let Some(value) = value.strip_prefix("--max-depth=") => {
                if value != "-1" {
                    return diff_index_usage_error();
                }
            }
            value if let Some(value) = value.strip_prefix("--abbrev=") => {
                abbrev = AbbrevRequest::Width(parse_diff_index_abbrev(value)?);
            }
            value if let Some(value) = value.strip_prefix("--src-prefix=") => {
                src_prefix = value.to_string();
            }
            value if let Some(value) = value.strip_prefix("--dst-prefix=") => {
                dst_prefix = value.to_string();
            }
            value if let Some(value) = value.strip_prefix("-M") => {
                log_validate_similarity_option(value, "find-renames")?;
                detect_renames = true;
                inexact_renames = true;
                rename_threshold = parse_similarity_threshold(value);
            }
            value if let Some(value) = value.strip_prefix("--find-renames=") => {
                log_validate_similarity_option(value, "find-renames")?;
                detect_renames = true;
                inexact_renames = true;
                rename_threshold = parse_similarity_threshold(value);
            }
            value if let Some(value) = value.strip_prefix("-C") => {
                log_validate_similarity_option(value, "find-copies")?;
                detect_renames = true;
                detect_copies = true;
                inexact_renames = true;
                copy_threshold = parse_similarity_threshold(value);
            }
            value if let Some(value) = value.strip_prefix("--find-copies=") => {
                log_validate_similarity_option(value, "find-copies")?;
                detect_renames = true;
                detect_copies = true;
                inexact_renames = true;
                copy_threshold = parse_similarity_threshold(value);
            }
            value if value.starts_with('-') && value != "-" => {
                return diff_index_usage_error();
            }
            _ => setup_args.push(arg.clone()),
        }
        idx += 1;
    }

    let repo = RepositoryContext::discover_current()?;
    let cwd = repo.cwd();
    let git_dir = repo.git_dir();
    let format = repo.format();
    let db = repo.objects();
    let setup = sley_rev::setup_revisions(
        &setup_args,
        &sley_rev::RevisionSetupContext {
            git_dir,
            worktree_root: repo.worktree_root().ok(),
            cwd,
            format,
            reader: db,
            config: Some(repo.config()),
        },
    )?;
    if !setup.leftovers.is_empty()
        || !setup.options.negatives.is_empty()
        || !setup.options.symmetric_ranges.is_empty()
        || setup.options.positives.len() != 1
    {
        return diff_index_usage_error();
    }
    let tree_tip = &setup.options.positives[0];
    let tree_oid = if merge_base {
        let head = commands::diff::diff_resolve_commit_arg(git_dir, format, db, "HEAD")?;
        let other =
            commands::diff::diff_resolve_commit_arg(git_dir, format, db, &tree_tip.rev)?;
        let base = commands::diff::diff_single_merge_base(git_dir, format, db, &head, &other)?;
        sley_rev::peel_to_tree(db, format, &base)?
    } else {
        resolve_tree_ish_oid(&repo, tree_tip.oid, &tree_tip.rev)?
    };

    // `core.abbrev` (defaulting to 7) is the width used when abbreviation is
    // requested, but unlike porcelain `git diff` the plumbing `diff-index`
    // shows *full* oids in the raw listing unless `--abbrev` is given, and
    // `core.abbrev` alone never abbreviates the raw output.
    let configured_abbrev = repo.abbrev()?.unwrap_or(DEFAULT_ABBREV);
    let raw_abbrev: Option<usize> = match abbrev {
        AbbrevRequest::Default | AbbrevRequest::None => None,
        AbbrevRequest::Auto => Some(configured_abbrev.min(format.hex_len())),
        AbbrevRequest::Width(width) => Some(width.min(format.hex_len())),
    };
    // The patch index line abbreviates to `core.abbrev` (default 7) and honours
    // `--abbrev`; `--full-index` forces the full oid. `--no-abbrev` does not
    // affect the patch index line, matching git.
    let patch_abbrev = if patch_full_index {
        format.hex_len()
    } else {
        match abbrev {
            AbbrevRequest::Width(width) => width.min(format.hex_len()),
            _ => configured_abbrev.min(format.hex_len()),
        }
    };

    let worktree_root = if cached {
        None
    } else {
        Some(repo.worktree_root()?)
    };
    let pathspec = if setup.pathspecs.is_empty() {
        DiffPathspec::default()
    } else {
        let worktree_root = match worktree_root {
            Some(worktree_root) => worktree_root,
            None => repo.worktree_root()?,
        };
        DiffPathspec::new(cwd, worktree_root, &setup.pathspecs)?
    };

    let base_options = sley_diff_merge::DiffNameStatusOptions {
        detect_renames,
        detect_copies,
        find_copies_harder,
        rename_empty,
    };
    let rename_options = sley_diff_merge::RenameDetectionOptions {
        base: base_options,
        detect_inexact: true,
        rename_threshold,
        copy_threshold,
    };

    let entries = if cached {
        if inexact_renames {
            sley_diff_merge::diff_name_status_tree_index_with_rename_options(
                git_dir,
                format,
                &tree_oid,
                rename_options,
            )?
        } else {
            sley_diff_merge::diff_name_status_tree_index_with_options(
                git_dir,
                format,
                &tree_oid,
                base_options,
            )?
        }
    } else {
        let worktree_root = worktree_root
            .ok_or_else(|| GitError::Command("diff-index requires a worktree".into()))?;
        if inexact_renames {
            sley_diff_merge::diff_name_status_tree_worktree_with_rename_options(
                worktree_root,
                git_dir,
                format,
                &tree_oid,
                rename_options,
            )?
        } else {
            sley_diff_merge::diff_name_status_tree_worktree_with_options(
                worktree_root,
                git_dir,
                format,
                &tree_oid,
                base_options,
            )?
        }
    };
    let submodule_config = submodule_diff_config(git_dir, worktree_root, ignore_submodules_cli);
    let mut entries = apply_submodule_ignore_filter(entries, &submodule_config);
    let submodule_dirt = match (!cached, worktree_root) {
        (true, Some(root)) => collect_dirty_submodules(
            &mut entries,
            git_dir,
            format,
            root,
            &submodule_config,
            None,
        )?,
        _ => HashMap::new(),
    };

    let entries = apply_diff_pathspec(entries, &pathspec);
    let entries = if match_missing && !cached {
        entries
            .into_iter()
            .filter(|entry| entry.status != sley_diff_merge::NameStatus::Deleted)
            .collect()
    } else {
        entries
    };
    let entries = if reverse {
        reverse_diff_entries(entries)
    } else {
        entries
    };
    let entries = apply_diff_index_filter(entries, &diff_filter, &pathspec);

    // `-R` swaps the patch prefixes in addition to the file pairs: the source
    // side renders with the dst prefix and vice versa (e.g. `diff --git b/x
    // a/x`). The raw and name-status outputs carry no prefix, so swapping the
    // strings only affects the patch renderer. `reverse_diff_entries` already
    // swapped the per-file old/new content and oids.
    let (src_prefix, dst_prefix) = if reverse {
        (dst_prefix, src_prefix)
    } else {
        (src_prefix, dst_prefix)
    };

    let has_differences = !entries.is_empty();
    // `--check`: whitespace-error report instead of the diff body (exit 2 on a
    // whitespace error, OR-ing in 1 when `--exit-code`/`--quiet` + changes).
    if check {
        let resolver = commands::diff::WhitespaceRuleResolver::from_git_dir(git_dir)?;
        let check_failed = commands::diff::run_diff_check(
            &entries,
            db,
            worktree_root,
            !cached,
            &resolver,
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
    if !quiet {
        // `--indent-heuristic` / `--no-indent-heuristic` win over
        // `diff.indentHeuristic`, which defaults to git's enabled behavior.
        let indent_heuristic = indent_heuristic
            .unwrap_or_else(|| repo.config().get_bool("diff", None, "indentheuristic").unwrap_or(true));
        render(
            &entries,
            &output,
            RenderContext {
                db,
                worktree_root,
                use_worktree_new: !cached,
                format,
                z,
                raw_abbrev,
                patch_abbrev,
                src_prefix: &src_prefix,
                dst_prefix: &dst_prefix,
                indent_heuristic,
                submodule_format,
                submodule_dirt: &submodule_dirt,
            },
        )?;
    }

    if (quiet || exit_code) && has_differences {
        return Err(GitError::Exit(1));
    }
    Ok(())
}

/// Apply `--diff-filter` selection, mirroring `cmd_diff` exactly: the `*` form
/// (`all_or_none`) keeps every entry when at least one matches an included
/// status, otherwise drops them all; the ordinary form keeps only entries whose
/// status is selected. Pathspec filtering has already run, so the `*` form's
/// pathspec re-check here is a no-op kept for parity.
fn apply_diff_index_filter(
    entries: Vec<sley_diff_merge::NameStatusEntry>,
    diff_filter: &DiffFilter,
    pathspec: &DiffPathspec,
) -> Vec<sley_diff_merge::NameStatusEntry> {
    if diff_filter.all_or_none {
        if !diff_filter.includes.is_empty()
            && entries.iter().any(|entry| {
                pathspec.matches(&entry.path) && diff_filter.matches_status(entry.status.code())
            })
        {
            entries
        } else {
            Vec::new()
        }
    } else if diff_filter.includes.is_empty() && diff_filter.excludes.is_empty() {
        entries
    } else {
        entries
            .into_iter()
            .filter(|entry| diff_filter.matches_status(entry.status.code()))
            .collect()
    }
}

/// Shared rendering parameters threaded into the format writers.
struct RenderContext<'a> {
    db: &'a FileObjectDatabase,
    worktree_root: Option<&'a Path>,
    use_worktree_new: bool,
    format: ObjectFormat,
    z: bool,
    raw_abbrev: Option<usize>,
    patch_abbrev: usize,
    src_prefix: &'a str,
    dst_prefix: &'a str,
    indent_heuristic: bool,
    submodule_format: commands::diff_options::SubmoduleDiffFormat,
    submodule_dirt: &'a HashMap<Vec<u8>, u8>,
}

fn render(
    entries: &[sley_diff_merge::NameStatusEntry],
    output: &DiffIndexOutput,
    ctx: RenderContext<'_>,
) -> Result<()> {
    let mut stdout = io::stdout();
    // The default (no explicit format flag) is the raw listing — the key
    // difference from `git diff`, whose default is a patch.
    let no_format = !output.raw
        && !output.patch
        && !output.name_status
        && !output.name_only
        && !output.stat
        && !output.numstat
        && !output.shortstat
        && !output.summary;
    let show_raw = output.raw || no_format;
    let show_numstat = output.numstat;
    let show_stat = output.stat;
    let show_shortstat = output.shortstat;
    let show_summary = output.summary;
    let show_name_status = output.name_status;
    let show_name_only = output.name_only;
    let show_patch = output.patch;

    if show_raw {
        for entry in entries {
            write_diff_raw_entry(&mut stdout, entry, ctx.z, false, ctx.raw_abbrev, ctx.format)?;
        }
    }
    if show_name_status {
        for entry in entries {
            write_name_status_entry(&mut stdout, entry, ctx.z)?;
        }
    }
    if show_name_only {
        for entry in entries {
            write_name_only_entry(&mut stdout, entry, ctx.z)?;
        }
    }
    if show_numstat {
        for entry in entries {
            write_diff_numstat_entry(
                &mut stdout,
                entry,
                ctx.z,
                ctx.db,
                ctx.worktree_root,
                ctx.use_worktree_new,
            )?;
        }
    }
    if show_stat {
        write_diff_stat(
            &mut stdout,
            entries,
            ctx.db,
            ctx.worktree_root,
            ctx.use_worktree_new,
            DiffStatOptions {
                compact_summary: output.compact_summary,
                stat_count: None,
                color: false,
            },
        )?;
    }
    if show_shortstat {
        write_diff_shortstat(
            &mut stdout,
            entries,
            ctx.db,
            ctx.worktree_root,
            ctx.use_worktree_new,
        )?;
    }
    if show_summary {
        for entry in entries {
            write_diff_summary_entry(&mut stdout, entry)?;
        }
    }
    if show_patch {
        // git separates a preceding raw/stat block from the patch with one blank
        // line (e.g. `--patch-with-raw`, `--patch-with-stat`).
        if show_raw || show_numstat || show_stat || show_shortstat || show_summary {
            writeln!(stdout)?;
        }
        for entry in entries {
            let options = DiffPatchOptions {
                db: ctx.db,
                worktree_root: ctx.worktree_root,
                use_worktree_new: ctx.use_worktree_new,
                format: ctx.format,
                abbrev: ctx.patch_abbrev,
                src_prefix: ctx.src_prefix,
                dst_prefix: ctx.dst_prefix,
                context: 3,
                userdiff: None,
                colors: None,
                word_diff: None,
                no_index_contents: None,
                submodule_format: ctx.submodule_format,
                submodule_dirt: Some(ctx.submodule_dirt),
                ws_error_rule: None,
                interhunk: 0,
                ws_ignore: sley_diff_merge::WsIgnore::default(),
                diff_algorithm: sley_diff_merge::DiffAlgorithm::Myers,
                ignore_blank_lines: false,
                ignore_regexes: &[],
                line_ranges: None,
                indent_heuristic: ctx.indent_heuristic,
            };
            write_diff_patch_entry(&mut stdout, entry, options)?;
        }
    }
    Ok(())
}

fn write_name_status_entry(
    stdout: &mut io::Stdout,
    entry: &sley_diff_merge::NameStatusEntry,
    z: bool,
) -> Result<()> {
    if z {
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
    Ok(())
}

fn write_name_only_entry(
    stdout: &mut io::Stdout,
    entry: &sley_diff_merge::NameStatusEntry,
    z: bool,
) -> Result<()> {
    if z {
        stdout.write_all(&entry.path)?;
        stdout.write_all(b"\0")?;
    } else {
        let path = status_quote_path(&entry.path, false);
        writeln!(stdout, "{path}")?;
    }
    Ok(())
}

fn take_value<'a>(args: &'a [String], idx: usize, option: &str) -> Result<&'a str> {
    args.get(idx)
        .map(String::as_str)
        .ok_or_else(|| GitError::Command(format!("{option} requires a value")))
}

/// Peel a resolved `<tree-ish>` argument to a tree oid.
fn resolve_tree_ish_oid(repo: &RepositoryContext, oid: ObjectId, rev: &str) -> Result<ObjectId> {
    let format = repo.format();
    // The canonical empty tree need not be present in the object database; git
    // always accepts it. Skip peeling (which would try to read the object) so
    // `diff-index <empty-tree-sha>` works in a fresh repository.
    if sley_core::object_id_for_bytes(format, "tree", b"").is_ok_and(|empty| empty == oid) {
        return Ok(oid);
    }
    sley_rev::peel_to_tree(repo.objects(), format, &oid)
        .map_err(|_| sley_rev::ambiguous_argument_error(rev))
}

fn parse_diff_index_abbrev(value: &str) -> Result<usize> {
    value
        .parse::<usize>()
        .map(|width| width.max(MIN_ABBREV))
        .map_err(|_| GitError::Command(format!("invalid --abbrev value: {value}")))
}

fn diff_index_usage_error<T>() -> Result<T> {
    eprint!("{DIFF_INDEX_USAGE}");
    Err(GitError::Exit(129))
}

fn diff_index_help() -> Result<()> {
    print!("{DIFF_INDEX_USAGE}");
    Err(GitError::Exit(129))
}

const DEFAULT_ABBREV: usize = 7;
const MIN_ABBREV: usize = 4;

/// How object-name abbreviation was requested on the command line. Resolved to
/// concrete widths for the raw listing and the patch index line once
/// `core.abbrev` is known, because plumbing `diff-index` treats the two outputs
/// differently (see the resolution in [`cmd_diff_index`]).
#[derive(Clone, Copy)]
enum AbbrevRequest {
    /// No `--abbrev`/`--no-abbrev`: raw is full, patch uses `core.abbrev`.
    Default,
    /// `--no-abbrev`: raw is full; the patch index line is unaffected.
    None,
    /// `--abbrev` with no value: use `core.abbrev` (default 7) for both.
    Auto,
    /// `--abbrev=<n>`: use `<n>` (floored at 4) for both.
    Width(usize),
}

const DIFF_INDEX_USAGE: &str = "usage: git diff-index [-m] [--cached] [--merge-base] [<common-diff-options>] <tree-ish> [<path>...]\n\ncommon diff options:\n  -z            output diff-raw with lines terminated with NUL.\n  -p            output patch format.\n  -u            synonym for -p.\n  --patch-with-raw\n                output both a patch and the diff-raw format.\n  --stat        show diffstat instead of patch.\n  --numstat     show numeric diffstat instead of patch.\n  --patch-with-stat\n                output a patch and prepend its diffstat.\n  --name-only   show only names of changed files.\n  --name-status show names and status of changed files.\n  --full-index  show full object name on index lines.\n  --abbrev=<n>  abbreviate object names in diff-tree header and diff-raw.\n  -R            swap input file pairs.\n  -B            detect complete rewrites.\n  -M            detect renames.\n  -C            detect copies.\n  --find-copies-harder\n                try unchanged files as candidate for copy detection.\n  -l<n>         limit rename attempts up to <n> paths.\n  -O<file>      reorder diffs according to the <file>.\n  -S<string>    find filepair whose only one side contains the string.\n  --pickaxe-all\n                show all files diff when -S is used and hit is found.\n  -a  --text    treat all files as text.\n\n";
