//! Extracted from the crate root (sley#8 phase 1) — code motion only.
#![allow(clippy::expect_used)]

use sley::plumbing::{
    sley_diff_merge, sley_index, sley_object, sley_refs, sley_rev, sley_worktree,
};
// A glob of the crate root brings every shared helper/type into scope via
// descendant-privacy; see commands::stash for the rationale.
use crate::*;
use sley_pathspec::{LsFilesPathFilter, parse_normalized_pathspec_element, pathspec_filters_match};

pub(crate) fn cmd_status(args: &[String]) -> Result<()> {
    // `-h`/`--help` short-circuits before any repository state is read, so it
    // works even in a broken repo (t7508 'status -h in broken repository').
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        return status_usage();
    }
    let mut short = false;
    let mut porcelain_v1 = false;
    let mut porcelain_v2 = false;
    let mut z = false;
    let mut explicit_long = false;
    let mut branch = false;
    // Track whether the format / branch / untracked-mode were set explicitly on
    // the command line. When they weren't, the corresponding `status.*` config
    // value supplies the default (upstream wt-status defaults come from config).
    let mut explicit_short = false;
    let mut explicit_branch: Option<bool> = None;
    let mut explicit_untracked = false;
    let mut untracked_mode = sley_worktree::StatusUntrackedMode::Normal;
    let mut show_ignored = false;
    let mut ignored_mode = sley_worktree::StatusIgnoredMode::Traditional;
    let mut show_stash = false;
    let mut column_untracked = false;
    let mut ahead_behind = true;
    let mut explicit_ahead_behind = false;
    // `--no-renames` / `--renames` (Some(true)/Some(false)) and
    // `-M`/`--find-renames[=<n>]` (Some(optional-score)); resolved against
    // `status.renames`/`diff.renames` config after the parse loop.
    let mut cli_no_renames: Option<bool> = None;
    let mut cli_rename_score: Option<Option<String>> = None;
    // `git status -v` verbosity: 0 (none), 1 (append the staged HEAD-vs-index
    // diff), 2+ (also append the index-vs-worktree diff). `-vv` and repeated
    // `-v` accumulate; `--no-verbose` resets to 0 (wt-status verbose level).
    let mut verbose: u8 = 0;
    // `--ignore-submodules[=<when>]` from the command line, the highest-priority
    // source for the per-submodule ignore resolution (above `.git/config`,
    // `.gitmodules`, and `diff.ignoreSubmodules`). `None` means the flag was not
    // given; the bare flag resolves to `All` exactly as git's parse-options does.
    let mut ignore_submodules_arg: Option<IgnoreSubmodules> = None;
    let mut path_args = Vec::new();
    let mut positional_only = false;
    for arg in args {
        if positional_only {
            path_args.push(arg.clone());
            continue;
        }
        match arg.as_str() {
            "--" => positional_only = true,
            "--short" | "-s" => {
                short = true;
                explicit_short = true;
                porcelain_v1 = false;
                porcelain_v2 = false;
                explicit_long = false;
            }
            "--porcelain" | "--porcelain=1" | "--porcelain=v1" => {
                short = true;
                porcelain_v1 = true;
                porcelain_v2 = false;
                explicit_long = false;
            }
            "--porcelain=v2" | "--porcelain=2" => {
                short = true;
                porcelain_v1 = false;
                porcelain_v2 = true;
                explicit_long = false;
            }
            "--no-porcelain" => {
                short = false;
                porcelain_v1 = false;
                porcelain_v2 = false;
                explicit_long = false;
            }
            "--branch" | "-b" => {
                branch = true;
                explicit_branch = Some(true);
                explicit_long = false;
            }
            "-sb" | "-bs" => {
                short = true;
                explicit_short = true;
                branch = true;
                explicit_branch = Some(true);
                explicit_long = false;
            }
            "-su" | "-us" => {
                short = true;
                explicit_short = true;
                porcelain_v1 = false;
                porcelain_v2 = false;
                explicit_long = false;
                untracked_mode = sley_worktree::StatusUntrackedMode::All;
                explicit_untracked = true;
            }
            // `-s` bundled with `-u<mode>` (e.g. `-suno`): in a single-dash
            // cluster, `-u` consumes the remainder of the cluster as its mode
            // argument, so `-suno` is `-s -u no`.
            value if value.starts_with("-su") && value.len() > 3 => {
                short = true;
                explicit_short = true;
                porcelain_v1 = false;
                porcelain_v2 = false;
                explicit_long = false;
                untracked_mode = parse_status_untracked_mode(&value[3..])?;
                explicit_untracked = true;
            }
            "--no-short" => {
                short = false;
                explicit_short = true;
                porcelain_v1 = false;
                porcelain_v2 = false;
            }
            "--no-branch" => {
                branch = false;
                explicit_branch = Some(false);
            }
            "--no-untracked-files" => {
                // `--untracked-files` is an OPTION_STRING with PARSE_OPT_OPTARG;
                // its `--no-` form clears the override (NULL arg), so the config
                // / default applies rather than forcing "no".
                untracked_mode = sley_worktree::StatusUntrackedMode::Normal;
                explicit_untracked = false;
            }
            "-u" | "--untracked-files" => {
                untracked_mode = sley_worktree::StatusUntrackedMode::All;
                explicit_untracked = true;
            }
            value if value.starts_with("-u") && value.len() > 2 => {
                untracked_mode = parse_status_untracked_mode(&value[2..])?;
                explicit_untracked = true;
            }
            value if value.starts_with("--untracked-files=") => {
                untracked_mode = parse_status_untracked_mode(&value["--untracked-files=".len()..])?;
                explicit_untracked = true;
            }
            value if value.starts_with("--porcelain=") => {
                return status_unsupported_porcelain_version_error(&value["--porcelain=".len()..]);
            }
            "-z" | "--null" => {
                short = true;
                z = true;
            }
            "--no-null" => z = false,
            "--ignored" | "--ignored=traditional" => {
                show_ignored = true;
                ignored_mode = sley_worktree::StatusIgnoredMode::Traditional;
            }
            "--ignored=matching" => {
                show_ignored = true;
                ignored_mode = sley_worktree::StatusIgnoredMode::Matching;
            }
            "--ignored=no" | "--no-ignored" => {
                show_ignored = false;
                ignored_mode = sley_worktree::StatusIgnoredMode::Traditional;
            }
            value if value.starts_with("--ignored=") => {
                return status_invalid_ignored_mode_error(&value["--ignored=".len()..]);
            }
            "--long" => {
                short = false;
                porcelain_v1 = false;
                porcelain_v2 = false;
                explicit_long = true;
            }
            "--no-long" => {
                short = false;
                porcelain_v1 = false;
                porcelain_v2 = false;
                explicit_long = false;
            }
            "-v" | "--verbose" => verbose = verbose.saturating_add(1),
            "--no-verbose" => verbose = 0,
            "--no-renames" => cli_no_renames = Some(true),
            "--renames" => cli_no_renames = Some(false),
            "--find-renames" => cli_rename_score = Some(None),
            value if value.starts_with("--find-renames=") => {
                let rest = &value["--find-renames=".len()..];
                let rest = rest.strip_prefix('=').unwrap_or(rest);
                cli_rename_score = Some(Some(rest.to_string()));
            }
            "--column"
            | "--column="
            | "--column=auto"
            | "--column=always"
            | "--column=plain"
            | "--column=column"
            | "--column=row"
            | "--column=dense"
            | "--column=nodense"
            | "--column=column dense" => {
                column_untracked = true;
            }
            "--no-column" | "--column=never" => {
                column_untracked = false;
            }
            // `--ignore-submodules[=<when>]` (builtin/commit.c's OPT_CALLBACK
            // with PARSE_OPT_OPTARG): the bare flag means "all"; `--no-` clears
            // any prior selection back to the config/default.
            "--ignore-submodules" | "--ignore-submodules=all" => {
                ignore_submodules_arg = Some(IgnoreSubmodules::All);
            }
            "--ignore-submodules=dirty" => {
                ignore_submodules_arg = Some(IgnoreSubmodules::Dirty);
            }
            "--ignore-submodules=untracked" => {
                ignore_submodules_arg = Some(IgnoreSubmodules::Untracked);
            }
            "--ignore-submodules=none" => {
                ignore_submodules_arg = Some(IgnoreSubmodules::None);
            }
            "--no-ignore-submodules" => {
                ignore_submodules_arg = None;
            }
            "--ahead-behind" => {
                ahead_behind = true;
                explicit_ahead_behind = true;
            }
            "--no-ahead-behind" => {
                ahead_behind = false;
                explicit_ahead_behind = true;
            }
            "--show-stash" => show_stash = true,
            "--no-show-stash" => show_stash = false,
            "-M" => cli_rename_score = Some(None),
            value if value.starts_with("-M") && value.len() > 2 => {
                // `-M<n>`; a leading `=` after `-M` is stripped (opt_parse_rename_score).
                let rest = &value[2..];
                let rest = rest.strip_prefix('=').unwrap_or(rest);
                cli_rename_score = Some(Some(rest.to_string()));
            }
            value if value.starts_with("--short=") => {
                return status_option_takes_no_value_error("short");
            }
            value if value.starts_with("--no-short=") => {
                return status_option_takes_no_value_error("no-short");
            }
            value if value.starts_with("--no-porcelain=") => {
                return status_option_takes_no_value_error("no-porcelain");
            }
            value if value.starts_with("--branch=") => {
                return status_option_takes_no_value_error("branch");
            }
            value if value.starts_with("--no-branch=") => {
                return status_option_takes_no_value_error("no-branch");
            }
            value if value.starts_with("--null=") => {
                return status_option_takes_no_value_error("null");
            }
            value if value.starts_with("--no-null=") => {
                return status_option_takes_no_value_error("no-null");
            }
            value if value.starts_with("--no-ignored=") => {
                return status_option_takes_no_value_error("no-ignored");
            }
            value if value.starts_with("--long=") => {
                return status_option_takes_no_value_error("long");
            }
            value if value.starts_with("--no-long=") => {
                return status_option_takes_no_value_error("no-long");
            }
            value if value.starts_with("--ahead-behind=") => {
                return status_option_takes_no_value_error("ahead-behind");
            }
            value if value.starts_with("--no-ahead-behind=") => {
                return status_option_takes_no_value_error("no-ahead-behind");
            }
            value if value.starts_with("--verbose=") => {
                return status_option_takes_no_value_error("verbose");
            }
            value if value.starts_with("--no-verbose=") => {
                return status_option_takes_no_value_error("no-verbose");
            }
            value if value.starts_with("--show-stash=") => {
                return status_option_takes_no_value_error("show-stash");
            }
            value if value.starts_with("--no-show-stash=") => {
                return status_option_takes_no_value_error("no-show-stash");
            }
            value if value.starts_with("--renames=") => {
                return status_option_takes_no_value_error("no-no-renames");
            }
            value if value.starts_with("--no-renames=") => {
                return status_option_takes_no_value_error("no-renames");
            }
            value if value.starts_with("--column=") => {
                return status_unsupported_column_option_error(&value["--column=".len()..]);
            }
            value if value.starts_with("--no-column=") => {
                return status_option_takes_no_value_error("no-column");
            }
            value if value.starts_with("--ignore-submodules=") => {
                return status_bad_ignore_submodules_argument_error(
                    &value["--ignore-submodules=".len()..],
                );
            }
            value if value.starts_with("--no-ignore-submodules=") => {
                return status_option_takes_no_value_error("no-ignore-submodules");
            }
            // `-vv`/`-vvv`: a run of `v` short flags raises the verbose level by
            // its length (parse-options collapses adjacent shorts).
            value
                if value.len() > 1
                    && value.starts_with('-')
                    && !value.starts_with("--")
                    && value[1..].bytes().all(|byte| byte == b'v') =>
            {
                verbose = verbose.saturating_add((value.len() - 1) as u8);
            }
            value if value.starts_with('-') => {
                return Err(GitError::Command(
                    "status currently supports only --short, --porcelain, --porcelain=1, --porcelain=v1, --porcelain=v2, --long, --branch, -z/--null, --untracked-files, --ignored=no, --no-renames, simple display toggles, and literal pathspecs"
                        .into(),
                ));
            }
            _ => path_args.push(arg.clone()),
        }
    }
    if explicit_long && z {
        eprintln!("fatal: options '--long' and '-z' cannot be used together");
        return Err(GitError::Exit(128));
    }
    let cwd = env::current_dir()?;
    let git_dir = crate::session::cli_git_dir_from(&cwd)?;
    let config = read_repo_config(&git_dir).map_err(report_config_setup_error)?;
    crate::repository::warn_graft_file_deprecated(&git_dir, &config);
    // Config-derived display defaults. The command line wins where it set a
    // value explicitly; otherwise `status.*` config supplies the default, as
    // upstream's wt-status initialization does.
    if !explicit_short
        && !porcelain_v1
        && !porcelain_v2
        && !explicit_long
        && config.get_bool("status", None, "short") == Some(true)
    {
        short = true;
    }
    if let Some(want_branch) = explicit_branch {
        branch = want_branch;
    } else if !porcelain_v1
        && !porcelain_v2
        && config.get_bool("status", None, "branch") == Some(true)
    {
        // `status.branch` adds the branch header to short/long output, but
        // `--porcelain` ignores it unless `-b` was passed explicitly
        // (t7508 '"status.branch=true" weaker than "--porcelain"').
        branch = true;
    }
    if !explicit_untracked {
        match config.get("status", None, "showUntrackedFiles") {
            Some("no") | Some("false") | Some("0") | Some("off") => {
                untracked_mode = sley_worktree::StatusUntrackedMode::None;
            }
            Some("all") => untracked_mode = sley_worktree::StatusUntrackedMode::All,
            // "normal"/"true"/unset keep the Normal default.
            _ => {}
        }
    }
    if !explicit_ahead_behind
        && !porcelain_v2
        && config.get_bool("status", None, "aheadbehind") == Some(false)
    {
        ahead_behind = false;
    }
    // advice.statusHints defaults to true; `relativePaths` to true; comment
    // prefix is off unless status.displayCommentPrefix is set.
    let status_hints = config
        .get_bool("advice", None, "statusHints")
        .unwrap_or(true);
    let relative_paths = config
        .get_bool("status", None, "relativePaths")
        .unwrap_or(true);
    let comment_prefix = status_comment_prefix(&config);
    let rename_config = resolve_status_rename_config(&config, cli_no_renames, cli_rename_score);
    // status needs a work tree; emit git's diagnostic (bare / no-worktree, or
    // the core.bare+core.worktree conflict) when one isn't available.
    let worktree_root = require_work_tree(&git_dir)?;
    let format = repository_object_format(&git_dir)?;
    commands::submodule::ensure_populated_gitlinks_readable(&worktree_root, &git_dir, format)?;
    let status_options = sley_worktree::ShortStatusOptions {
        include_ignored: show_ignored,
        ignored_mode,
        untracked_mode,
    };
    let pathspec = StatusPathspec::new(&cwd, &worktree_root, &path_args)?;
    if !porcelain_v2 && (z || short) {
        print_status_short_stream(
            &worktree_root,
            &git_dir,
            format,
            &config,
            status_options,
            &pathspec,
            ignore_submodules_arg,
            StatusShortStreamDisplay {
                branch,
                ahead_behind,
                z,
                porcelain_v1,
                relative_paths,
                rename_config,
            },
        )?;
        if !pathspec.has_filters() && !show_ignored && explicit_untracked {
            sley_worktree::emit_untracked_cache_bypass_trace();
        } else if !pathspec.has_filters() && !show_ignored {
            sley_worktree::refresh_untracked_cache_after_status(
                &worktree_root,
                &git_dir,
                format,
                &config,
                untracked_mode,
            )?;
        }
        apply_status_split_index_config(&git_dir, format, &config)?;
        commands::hooks::run_post_index_change_hook(false, false)?;
        return Ok(());
    }
    // Resolve the per-submodule ignore setting (command line > `.git/config` >
    // `.gitmodules` > `diff.ignoreSubmodules`) and apply it to the worktree-side
    // submodule change detail, exactly as git's handle_ignore_submodules_arg ahead
    // of the diff. Computed before the relativePaths display rewrite so gitlink
    // lookups use worktree-root-relative paths.
    let ignore_resolver = SubmoduleIgnoreResolver::load(&git_dir, &config, ignore_submodules_arg)?;
    let collection_options = status_collection_options_for_pathspec(status_options, &pathspec);
    let mut entries = crate::collect_short_status_with_options(
        &worktree_root,
        &git_dir,
        format,
        collection_options,
    )?;
    if pathspec.has_filters() {
        entries.retain(|entry| pathspec.matches(&entry.path));
    }
    apply_submodule_ignore(&mut entries, &ignore_resolver);
    entries = status_collapse_pathspec_untracked_entries(entries, status_options, &pathspec);
    // The long-format `Submodule changes to be committed:` /
    // `Submodules changed but not updated:` sections (status.submodulesummary).
    // Only the long output renders them; compute before the display rewrite so
    // the gitlink paths still address the worktree.
    let submodule_summary = if !short && !porcelain_v1 && !porcelain_v2 && !z {
        status_submodule_summary(
            &git_dir,
            &worktree_root,
            format,
            &config,
            "HEAD",
            &ignore_resolver,
        )?
    } else {
        SubmoduleSummarySections::default()
    };
    // `status.relativePaths=false` displays paths from the worktree root rather
    // than relative to the current directory (upstream status.relativePaths).
    if !z && !porcelain_v1 && relative_paths {
        for entry in &mut entries {
            entry.path = pathspec.display(&entry.path);
        }
    }
    if porcelain_v2 {
        print_status_porcelain_v2(
            &git_dir,
            format,
            &config,
            entries,
            branch,
            ahead_behind,
            z,
            show_stash,
            &rename_config,
        )?;
    } else if z {
        let mut stdout = io::stdout().lock();
        if branch {
            stdout.write_all(
                status_branch_header(&git_dir, format, &config, ahead_behind)?.as_bytes(),
            )?;
            stdout.write_all(&[0])?;
        }
        for entry in entries {
            write!(stdout, "{}{} ", entry.index as char, entry.worktree as char)?;
            stdout.write_all(&entry.path)?;
            stdout.write_all(&[0])?;
        }
    } else if short {
        if branch {
            println!(
                "{}",
                status_branch_header(&git_dir, format, &config, ahead_behind)?
            );
        }
        for entry in entries {
            // `--short` (but not --porcelain) refines a submodule's worktree
            // column per upstream short_submodule_status(): 'M' new commits,
            // 'm' modified content, '?' untracked content.
            let worktree_code = if porcelain_v1 {
                entry.worktree
            } else {
                status_short_submodule_code(&entry)
            };
            println!(
                "{}{} {}",
                entry.index as char,
                worktree_code as char,
                status_quote_path(&entry.path, true)
            );
        }
    } else {
        let display = StatusLongDisplay {
            commit_preview: false,
            show_stash,
            ahead_behind,
            hints: status_hints,
            untracked_suppressed: untracked_mode == sley_worktree::StatusUntrackedMode::None,
            comment_prefix,
            submodule_summary,
            sparse_footer: status_sparse_footer(&git_dir, format)?,
            rename_config,
        };
        print_status_long_with_column(&git_dir, format, entries, &display, column_untracked)?;
        // `git status -v` appends the staged diff (HEAD vs index). `-vv` instead
        // frames both diffs with section headers and a 50-dash separator and
        // renders them with diff.mnemonicprefix=true (commit/index `c/`,`i/` for
        // the cached half; index/worktree `i/`,`w/` for the unstaged half) —
        // exactly wt-status's verbose>1 layout. Reuse the diff command so the
        // hunk bytes match `git diff` verbatim.
        if verbose == 1 {
            io::stdout().flush()?;
            commands::diff::cmd_diff(&["--cached".to_string()])?;
        } else if verbose >= 2 {
            io::stdout().flush()?;
            println!("Changes to be committed:");
            io::stdout().flush()?;
            commands::diff::cmd_diff(&[
                "--cached".to_string(),
                "--src-prefix=c/".to_string(),
                "--dst-prefix=i/".to_string(),
            ])?;
            println!("--------------------------------------------------");
            println!("Changes not staged for commit:");
            io::stdout().flush()?;
            commands::diff::cmd_diff(&[
                "--src-prefix=i/".to_string(),
                "--dst-prefix=w/".to_string(),
            ])?;
        }
    }
    if !pathspec.has_filters() && !show_ignored && explicit_untracked {
        sley_worktree::emit_untracked_cache_bypass_trace();
    } else if !pathspec.has_filters() && !show_ignored {
        sley_worktree::refresh_untracked_cache_after_status(
            &worktree_root,
            &git_dir,
            format,
            &config,
            untracked_mode,
        )?;
    }
    apply_status_split_index_config(&git_dir, format, &config)?;
    commands::hooks::run_post_index_change_hook(false, false)?;
    Ok(())
}

fn apply_status_split_index_config(
    git_dir: &Path,
    format: ObjectFormat,
    config: &GitConfig,
) -> Result<()> {
    match config.get_bool("core", None, "splitIndex") {
        Some(true) => sley_worktree::enable_split_index(git_dir, format).map(|_| ()),
        Some(false) => sley_worktree::disable_split_index(git_dir, format).map(|_| ()),
        None => Ok(()),
    }
}

/// `git status -h`: usage synopsis + exit 129, mirroring commit_usage().
fn status_usage() -> Result<()> {
    eprintln!("usage: git status [<options>] [--] [<pathspec>...]");
    Err(GitError::Exit(129))
}

/// What kind of rename/copy detection `git status` performs, mirroring
/// `diff_options.detect_rename` (`DIFF_DETECT_RENAME` / `DIFF_DETECT_COPY`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StatusRenameDetect {
    Off,
    Renames,
    Copies,
}

/// Resolved `status.renames` / `diff.renames` / `-M` / `--no-renames` settings.
/// Thresholds are similarity percentages (0..=100), matching `blob_similarity`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct StatusRenameConfig {
    pub(crate) detect: StatusRenameDetect,
    pub(crate) rename_threshold: u8,
    pub(crate) copy_threshold: u8,
}

impl StatusRenameConfig {
    fn enabled(&self) -> bool {
        self.detect != StatusRenameDetect::Off
    }
}

/// git's default `-M`/`-C` similarity floor (`DEFAULT_RENAME_SCORE` = 50%).
const STATUS_DEFAULT_RENAME_THRESHOLD: u8 = 50;

/// `git_config_rename`: map a `status.renames`/`diff.renames` value to a
/// `detect_rename` code (0 = off, 1 = renames, 2 = copies). A bare key with no
/// value means renames-on.
fn status_config_rename_value(value: Option<&str>) -> i8 {
    match value {
        None => 1,
        Some(v) => {
            let lower = v.to_ascii_lowercase();
            if lower == "copies" || lower == "copy" {
                2
            } else if parse_git_config_bool(v) {
                1
            } else {
                0
            }
        }
    }
}

/// `git config_bool`-style truthiness for rename config values.
fn parse_git_config_bool(value: &str) -> bool {
    !matches!(
        value.to_ascii_lowercase().as_str(),
        "false" | "no" | "off" | "0" | ""
    )
}

/// `parse_rename_score`, expressed as a similarity percentage (0..=100). Accepts
/// the `<n>`, `<n>%`, and `.<frac>` forms git's diff option parser does.
fn parse_status_rename_score_percent(arg: &str) -> u8 {
    let mut num: u64 = 0;
    let mut scale: u64 = 1;
    let mut dot = false;
    for ch in arg.bytes() {
        match ch {
            b'.' if !dot => {
                scale = 1;
                dot = true;
            }
            b'%' => {
                scale = if dot { scale * 100 } else { 100 };
                break;
            }
            b'0'..=b'9' => {
                if scale < 100_000 {
                    scale *= 10;
                    num = num * 10 + u64::from(ch - b'0');
                }
            }
            _ => break,
        }
    }
    if num >= scale {
        100
    } else {
        (100 * num / scale) as u8
    }
}

/// Resolve `git status` rename detection from config + CLI, mirroring
/// builtin/commit.c: `diff.renames` is a default (only applied when unset),
/// `status.renames` overrides it, then `--no-renames` / `--renames` and
/// `-M`/`--find-renames[=<n>]` apply on top.
pub(crate) fn resolve_status_rename_config(
    config: &GitConfig,
    cli_no_renames: Option<bool>,
    cli_rename_score: Option<Option<String>>,
) -> StatusRenameConfig {
    let mut detect: i8 = -1;
    if let Some(v) = config.get("diff", None, "renames")
        && detect == -1
    {
        detect = status_config_rename_value(Some(v));
    }
    if let Some(v) = config.get("status", None, "renames") {
        detect = status_config_rename_value(Some(v));
    }
    let mut threshold = STATUS_DEFAULT_RENAME_THRESHOLD;
    if let Some(no) = cli_no_renames {
        detect = if no { 0 } else { 1 };
    }
    if let Some(score) = cli_rename_score {
        if detect < 1 {
            detect = 1;
        }
        if let Some(score) = score {
            threshold = parse_status_rename_score_percent(&score);
        }
    }
    let detect = match detect {
        0 => StatusRenameDetect::Off,
        2 => StatusRenameDetect::Copies,
        _ => StatusRenameDetect::Renames,
    };
    StatusRenameConfig {
        detect,
        rename_threshold: threshold,
        copy_threshold: threshold,
    }
}

/// Display knobs for the long ("porcelain off") `git status` output, derived
/// from the command line plus `status.*` / `advice.*` config.
pub(crate) struct StatusLongDisplay {
    /// `commit --dry-run` preview wording (initial-commit hint text).
    pub(crate) commit_preview: bool,
    pub(crate) show_stash: bool,
    pub(crate) ahead_behind: bool,
    /// `advice.statusHints` — when false, the parenthetical `(use "git ...")`
    /// guidance lines are suppressed throughout the output.
    pub(crate) hints: bool,
    /// True when untracked files are hidden (`-uno` / `status.showUntrackedFiles
    /// no`); drives the "Untracked files not listed" line when committable.
    pub(crate) untracked_suppressed: bool,
    /// `core.commentChar` / `status.displayCommentPrefix`: when set, every line
    /// is prefixed with the comment character (e.g. `# `), as in COMMIT_EDITMSG.
    pub(crate) comment_prefix: Option<String>,
    /// Rendered `Submodule changes to be committed:` /
    /// `Submodules changed but not updated:` sections (status.submodulesummary).
    pub(crate) submodule_summary: SubmoduleSummarySections,
    /// Long-status sparse-checkout trailer, omitted from porcelain and short
    /// formats. A sparse index uses Git's terse wording; a full sparse checkout
    /// reports the tracked-file percentage present in the worktree.
    pub(crate) sparse_footer: Option<StatusSparseFooter>,
    /// Resolved `status.renames`/`diff.renames`/`-M` rename detection for the
    /// "Changes to be committed" section.
    pub(crate) rename_config: StatusRenameConfig,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum StatusSparseFooter {
    SparseIndex,
    Percentage(u8),
}

/// Upstream wt-status.c short_submodule_status(): in `--short` output a
/// changed submodule's worktree column shows 'M' for new commits, 'm' for
/// modified content, '?' for untracked content (priority in that order).
fn status_short_submodule_code(entry: &sley_worktree::ShortStatusEntry) -> u8 {
    let Some(submodule) = entry.submodule else {
        return entry.worktree;
    };
    if submodule.new_commits {
        b'M'
    } else if submodule.modified_content {
        b'm'
    } else if submodule.untracked_content {
        b'?'
    } else {
        entry.worktree
    }
}

#[derive(Debug, Clone, Copy)]
struct StatusShortStreamDisplay {
    branch: bool,
    ahead_behind: bool,
    z: bool,
    porcelain_v1: bool,
    relative_paths: bool,
    rename_config: StatusRenameConfig,
}

fn print_status_short_stream(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    config: &GitConfig,
    options: sley_worktree::ShortStatusOptions,
    pathspec: &StatusPathspec,
    ignore_submodules_arg: Option<IgnoreSubmodules>,
    display: StatusShortStreamDisplay,
) -> Result<()> {
    if display.z {
        let mut stdout = io::stdout().lock();
        if display.branch {
            stdout.write_all(
                status_branch_header(git_dir, format, config, display.ahead_behind)?.as_bytes(),
            )?;
            stdout.write_all(&[0])?;
        }
        let mut ignore_resolver = None;
        sley_worktree::stream_short_status_with_options(
            worktree_root,
            git_dir,
            format,
            options,
            |entry| {
                if pathspec.has_filters() && !pathspec.matches(entry.path) {
                    return Ok(sley_worktree::StreamControl::Continue);
                }
                let mut entry = entry.to_owned_entry();
                if status_entry_needs_submodule_ignore(&entry)
                    && !apply_submodule_ignore_entry(
                        &mut entry,
                        lazy_submodule_ignore_resolver(
                            &mut ignore_resolver,
                            git_dir,
                            config,
                            ignore_submodules_arg,
                        )?,
                    )
                {
                    return Ok(sley_worktree::StreamControl::Continue);
                }
                write!(stdout, "{}{} ", entry.index as char, entry.worktree as char)?;
                stdout.write_all(&entry.path)?;
                stdout.write_all(&[0])?;
                Ok(sley_worktree::StreamControl::Continue)
            },
        )?;
        stdout.flush()?;
        return Ok(());
    }

    if display.branch {
        println!(
            "{}",
            status_branch_header(git_dir, format, config, display.ahead_behind)?
        );
    }
    let mut ignore_resolver = None;
    if display.rename_config.enabled() {
        let collection_options = status_collection_options_for_pathspec(options, pathspec);
        let mut entries = crate::collect_short_status_with_options(
            worktree_root,
            git_dir,
            format,
            collection_options,
        )?;
        if pathspec.has_filters() {
            entries.retain(|entry| pathspec.matches(&entry.path));
        }
        apply_submodule_ignore(
            &mut entries,
            lazy_submodule_ignore_resolver(
                &mut ignore_resolver,
                git_dir,
                config,
                ignore_submodules_arg,
            )?,
        );
        entries = status_collapse_pathspec_untracked_entries(entries, options, pathspec);
        for entry in status_entries_with_renames(
            worktree_root,
            git_dir,
            format,
            entries,
            &display.rename_config,
        )? {
            let mut entry = entry;
            if !display.porcelain_v1 && display.relative_paths {
                entry.entry.path = pathspec.display(&entry.entry.path);
                if let Some(path) = entry.rename_from.as_mut() {
                    *path = pathspec.display(path);
                }
            }
            let worktree_code = if display.porcelain_v1 {
                entry.entry.worktree
            } else {
                status_short_submodule_code(&entry.entry)
            };
            if let Some(rename_from) = entry.rename_from {
                println!(
                    "{}{} {} -> {}",
                    entry.entry.index as char,
                    worktree_code as char,
                    status_quote_path(&rename_from, true),
                    status_quote_path(&entry.entry.path, true)
                );
            } else {
                println!(
                    "{}{} {}",
                    entry.entry.index as char,
                    worktree_code as char,
                    status_quote_path(&entry.entry.path, true)
                );
            }
        }
        return Ok(());
    }
    sley_worktree::stream_short_status_with_options(
        worktree_root,
        git_dir,
        format,
        options,
        |entry| {
            if pathspec.has_filters() && !pathspec.matches(entry.path) {
                return Ok(sley_worktree::StreamControl::Continue);
            }
            let mut entry = entry.to_owned_entry();
            if status_entry_needs_submodule_ignore(&entry)
                && !apply_submodule_ignore_entry(
                    &mut entry,
                    lazy_submodule_ignore_resolver(
                        &mut ignore_resolver,
                        git_dir,
                        config,
                        ignore_submodules_arg,
                    )?,
                )
            {
                return Ok(sley_worktree::StreamControl::Continue);
            }
            if !display.porcelain_v1 && display.relative_paths {
                entry.path = pathspec.display(&entry.path);
            }
            let worktree_code = if display.porcelain_v1 {
                entry.worktree
            } else {
                status_short_submodule_code(&entry)
            };
            println!(
                "{}{} {}",
                entry.index as char,
                worktree_code as char,
                status_quote_path(&entry.path, true)
            );
            Ok(sley_worktree::StreamControl::Continue)
        },
    )
}

fn status_collection_options_for_pathspec(
    mut options: sley_worktree::ShortStatusOptions,
    pathspec: &StatusPathspec,
) -> sley_worktree::ShortStatusOptions {
    if options.include_ignored
        && pathspec.has_filters()
        && matches!(
            options.untracked_mode,
            sley_worktree::StatusUntrackedMode::Normal
        )
    {
        options.untracked_mode = sley_worktree::StatusUntrackedMode::All;
    }
    options
}

fn status_collapse_pathspec_untracked_entries(
    entries: Vec<sley_worktree::ShortStatusEntry>,
    options: sley_worktree::ShortStatusOptions,
    pathspec: &StatusPathspec,
) -> Vec<sley_worktree::ShortStatusEntry> {
    if !options.include_ignored
        || !pathspec.has_filters()
        || !matches!(
            options.untracked_mode,
            sley_worktree::StatusUntrackedMode::Normal
        )
    {
        return entries;
    }
    let mut collapsed = BTreeMap::new();
    for mut entry in entries {
        if entry.index == b'?'
            && entry.worktree == b'?'
            && let Some(directory) = pathspec.recursive_directory_for(&entry.path)
        {
            entry.path = directory;
        }
        collapsed
            .entry((entry.index, entry.worktree, entry.path.clone()))
            .or_insert(entry);
    }
    let mut entries = collapsed.into_values().collect::<Vec<_>>();
    entries.sort_by(|left, right| {
        status_output_sort_category(left)
            .cmp(&status_output_sort_category(right))
            .then_with(|| left.path.cmp(&right.path))
    });
    entries
}

fn status_output_sort_category(entry: &sley_worktree::ShortStatusEntry) -> u8 {
    match (entry.index, entry.worktree) {
        (b'?', b'?') => 1,
        (b'!', b'!') => 2,
        _ => 0,
    }
}

struct StatusOutputEntry {
    entry: sley_worktree::ShortStatusEntry,
    rename_from: Option<Vec<u8>>,
}

fn status_entries_with_exact_renames(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    entries: Vec<sley_worktree::ShortStatusEntry>,
) -> Result<Vec<StatusOutputEntry>> {
    let mut used = vec![false; entries.len()];
    let mut staged_deletes = Vec::<sley_worktree::ShortStatusEntry>::new();
    let mut staged_used = Vec::<bool>::new();
    let mut residual_deletes = Vec::<sley_worktree::ShortStatusEntry>::new();
    let mut residual_used = Vec::<bool>::new();
    let mut output = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        if used[index] {
            continue;
        }
        if entry.index == b' ' && entry.worktree == b'D' {
            let mut has_later_add = false;
            for added in entries.iter().skip(index + 1) {
                if status_entries_are_exact_worktree_rename(
                    worktree_root,
                    git_dir,
                    format,
                    entry,
                    added,
                )? {
                    has_later_add = true;
                    break;
                }
            }
            if has_later_add {
                residual_deletes.push(entry.clone());
                residual_used.push(false);
                used[index] = true;
                continue;
            }
        }
        if entry.index == b'D' && entry.worktree == b' ' {
            let has_later_add = entries
                .iter()
                .skip(index + 1)
                .any(|added| status_entries_are_exact_rename(entry, added));
            if has_later_add {
                staged_deletes.push(entry.clone());
                staged_used.push(false);
                used[index] = true;
                continue;
            }
        }
        if entry.index == b'A' {
            // git's `find_identical_files`: among the identical-OID delete
            // candidates, prefer one that shares the added path's basename (the
            // first such wins; otherwise the first candidate overall). Search the
            // in-line deletes first, then the deferred staged-delete pool.
            let added_base = sley_diff_merge::path_basename(&entry.path);
            let mut staged_match: Option<(usize, Option<usize>, sley_worktree::ShortStatusEntry)> =
                None;
            let mut chosen_same_basename = false;
            for (candidate_index, candidate) in entries.iter().enumerate() {
                if used[candidate_index] || !status_entries_are_exact_rename(candidate, entry) {
                    continue;
                }
                let same = sley_diff_merge::path_basename(&candidate.path) == added_base;
                if staged_match.is_none() || (same && !chosen_same_basename) {
                    staged_match = Some((candidate_index, None, candidate.clone()));
                    chosen_same_basename = same;
                    if same {
                        break;
                    }
                }
            }
            if !chosen_same_basename {
                for (staged_index, candidate) in staged_deletes.iter().enumerate() {
                    if staged_used[staged_index]
                        || !status_entries_are_exact_rename(candidate, entry)
                    {
                        continue;
                    }
                    let same = sley_diff_merge::path_basename(&candidate.path) == added_base;
                    if staged_match.is_none() || (same && !chosen_same_basename) {
                        staged_match = Some((index, Some(staged_index), candidate.clone()));
                        chosen_same_basename = same;
                        if same {
                            break;
                        }
                    }
                }
            }
            let Some((deleted_index, staged_index, deleted)) = staged_match else {
                used[index] = true;
                output.push(StatusOutputEntry {
                    entry: entry.clone(),
                    rename_from: None,
                });
                continue;
            };
            let mut renamed = entry.clone();
            renamed.index = b'R';
            renamed.worktree = b' ';
            renamed.head_mode = deleted.head_mode;
            renamed.head_oid = deleted.head_oid;
            renamed.worktree_mode = entry.index_mode;
            used[index] = true;
            if let Some(staged_index) = staged_index {
                staged_used[staged_index] = true;
            } else {
                used[deleted_index] = true;
            }
            output.push(StatusOutputEntry {
                entry: renamed,
                rename_from: Some(deleted.path.clone()),
            });
            if entry.worktree != b' ' {
                let mut residual = entry.clone();
                residual.index = b' ';
                residual.head_mode = entry.index_mode;
                residual.head_oid = entry.index_oid;
                residual_deletes.push(residual);
                residual_used.push(false);
            }
            continue;
        }
        if entry.index == b' ' && entry.worktree == b'A' {
            let mut worktree_match = None;
            for (candidate_index, candidate) in entries.iter().enumerate() {
                if used[candidate_index] {
                    continue;
                }
                if status_entries_are_exact_worktree_rename(
                    worktree_root,
                    git_dir,
                    format,
                    candidate,
                    entry,
                )? {
                    worktree_match = Some((candidate_index, None, candidate.clone()));
                    break;
                }
            }
            if worktree_match.is_none() {
                for (residual_index, candidate) in residual_deletes.iter().enumerate() {
                    if residual_used[residual_index] {
                        continue;
                    }
                    if status_entries_are_exact_worktree_rename(
                        worktree_root,
                        git_dir,
                        format,
                        candidate,
                        entry,
                    )? {
                        worktree_match = Some((index, Some(residual_index), candidate.clone()));
                        break;
                    }
                }
            }
            let Some((deleted_index, residual_index, deleted)) = worktree_match else {
                used[index] = true;
                output.push(StatusOutputEntry {
                    entry: entry.clone(),
                    rename_from: None,
                });
                continue;
            };
            let mut renamed = entry.clone();
            renamed.worktree = b'R';
            renamed.head_mode = deleted.head_mode;
            renamed.index_mode = deleted.index_mode;
            renamed.head_oid = deleted.head_oid;
            renamed.index_oid = deleted.index_oid;
            renamed.worktree_mode = entry.worktree_mode;
            used[index] = true;
            if let Some(residual_index) = residual_index {
                residual_used[residual_index] = true;
            } else {
                used[deleted_index] = true;
            }
            output.push(StatusOutputEntry {
                entry: renamed,
                rename_from: Some(deleted.path.clone()),
            });
            continue;
        }
        used[index] = true;
        output.push(StatusOutputEntry {
            entry: entry.clone(),
            rename_from: None,
        });
    }
    for (entry, used) in staged_deletes.into_iter().zip(staged_used) {
        if !used {
            output.push(StatusOutputEntry {
                entry,
                rename_from: None,
            });
        }
    }
    for (entry, used) in residual_deletes.into_iter().zip(residual_used) {
        if !used {
            output.push(StatusOutputEntry {
                entry,
                rename_from: None,
            });
        }
    }
    Ok(output)
}

/// Resolve rename/copy detection for `git status` output. With detection off,
/// every entry passes through unpaired. Otherwise exact-OID renames are detected
/// first (always scored 100), then inexact (content-similarity) renames and —
/// when `status.renames=copies` — copies are detected among the leftovers.
fn status_entries_with_renames(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    entries: Vec<sley_worktree::ShortStatusEntry>,
    rename_config: &StatusRenameConfig,
) -> Result<Vec<StatusOutputEntry>> {
    if !rename_config.enabled() {
        return Ok(entries
            .into_iter()
            .map(|entry| StatusOutputEntry {
                entry,
                rename_from: None,
            })
            .collect());
    }
    let mut output = status_entries_with_exact_renames(worktree_root, git_dir, format, entries)?;
    status_apply_inexact_staged_renames(git_dir, format, &mut output, rename_config)?;
    Ok(output)
}

/// Inexact rename/copy detection over the HEAD-vs-index (staged) changes that the
/// exact pass left unpaired. Mirrors diffcore-rename's similarity matching:
/// staged adds are paired with the most-similar staged delete at or above the
/// rename threshold; under copy detection, any remaining add is also matched to
/// its most-similar source (renames consume their source delete, copies do not).
/// Only the clean-worktree staged columns are considered — the cases the t7525
/// rename suite and `git diff -M` parity exercise.
fn status_apply_inexact_staged_renames(
    git_dir: &Path,
    format: ObjectFormat,
    output: &mut Vec<StatusOutputEntry>,
    rename_config: &StatusRenameConfig,
) -> Result<()> {
    let db = FileObjectDatabase::from_git_dir(git_dir, format);

    fn read_blob(db: &FileObjectDatabase, oid: &ObjectId) -> Option<Vec<u8>> {
        db.read_object(oid).ok().map(|obj| obj.body.clone())
    }

    let is_staged_add = |entry: &StatusOutputEntry| {
        entry.rename_from.is_none()
            && entry.entry.index == b'A'
            && entry.entry.worktree == b' '
            && entry.entry.index_oid.is_some()
            && !entry.entry.index_mode.is_some_and(sley_index::is_gitlink)
    };
    let is_staged_delete = |entry: &StatusOutputEntry| {
        entry.rename_from.is_none()
            && entry.entry.index == b'D'
            && entry.entry.worktree == b' '
            && entry.entry.head_oid.is_some()
            && !entry.entry.head_mode.is_some_and(sley_index::is_gitlink)
    };

    let source_indices: Vec<usize> = output
        .iter()
        .enumerate()
        .filter(|(_, e)| is_staged_delete(e))
        .map(|(i, _)| i)
        .collect();
    let target_indices: Vec<usize> = output
        .iter()
        .enumerate()
        .filter(|(_, e)| is_staged_add(e))
        .map(|(i, _)| i)
        .collect();
    if source_indices.is_empty() || target_indices.is_empty() {
        return Ok(());
    }

    // Cache source/target blob bytes once.
    let source_blobs: Vec<Option<Vec<u8>>> = source_indices
        .iter()
        .map(|&i| {
            output[i]
                .entry
                .head_oid
                .and_then(|oid| read_blob(&db, &oid))
        })
        .collect();
    let target_blobs: Vec<Option<Vec<u8>>> = target_indices
        .iter()
        .map(|&i| {
            output[i]
                .entry
                .index_oid
                .and_then(|oid| read_blob(&db, &oid))
        })
        .collect();

    // Score every (target, source) pair once.
    let mut pairs: Vec<(u8, usize, usize)> = Vec::new();
    for (ti, target_blob) in target_blobs.iter().enumerate() {
        let Some(target_blob) = target_blob else {
            continue;
        };
        for (si, source_blob) in source_blobs.iter().enumerate() {
            let Some(source_blob) = source_blob else {
                continue;
            };
            let score = sley_diff_merge::blob_similarity(source_blob, target_blob);
            pairs.push((score, ti, si));
        }
    }
    // Highest similarity first; diffcore-rename prefers the best alignment.
    pairs.sort_by(|a, b| b.0.cmp(&a.0));

    let detect_copies = rename_config.detect == StatusRenameDetect::Copies;
    let mut target_paired: Vec<Option<(usize, bool)>> = vec![None; target_indices.len()];

    if detect_copies {
        // Copy detection: each target takes its most-similar source (sources may
        // be shared). Then, for every source used by ≥1 target, the LAST target
        // in pathname order is the rename and the earlier ones are copies —
        // diffcore's `--p->one->rename_used > 0 ? COPIED : RENAMED` resolution.
        for &(score, ti, si) in &pairs {
            if score < rename_config.copy_threshold {
                break;
            }
            if target_paired[ti].is_some() {
                continue;
            }
            target_paired[ti] = Some((si, true));
        }
        let mut rename_target_for_source = vec![None; source_indices.len()];
        for (ti, pairing) in target_paired.iter().enumerate() {
            if let Some((si, _)) = pairing {
                let slot = &mut rename_target_for_source[*si];
                if slot.is_none_or(|prev| ti > prev) {
                    *slot = Some(ti);
                }
            }
        }
        for ti in rename_target_for_source.into_iter().flatten() {
            if let Some((si, _)) = target_paired[ti] {
                target_paired[ti] = Some((si, false));
            }
        }
    } else {
        // Rename-only: each source delete renames to at most one target, going to
        // the highest-similarity pairing first (diffcore's score-sorted matrix).
        let mut source_renamed = vec![false; source_indices.len()];
        for &(score, ti, si) in &pairs {
            if score < rename_config.rename_threshold {
                break;
            }
            if target_paired[ti].is_some() || source_renamed[si] {
                continue;
            }
            target_paired[ti] = Some((si, false));
            source_renamed[si] = true;
        }
    }

    // Apply: relabel each paired add, and mark renamed sources for removal.
    let mut remove = vec![false; output.len()];
    for (ti, pairing) in target_paired.iter().enumerate() {
        let Some((si, is_copy)) = *pairing else {
            continue;
        };
        let target_idx = target_indices[ti];
        let source_idx = source_indices[si];
        let source = &output[source_idx].entry;
        let source_path = source.path.clone();
        let source_head_mode = source.head_mode;
        let source_head_oid = source.head_oid;
        let target = &mut output[target_idx];
        target.entry.index = if is_copy { b'C' } else { b'R' };
        target.entry.head_mode = source_head_mode;
        target.entry.head_oid = source_head_oid;
        target.rename_from = Some(source_path);
        if !is_copy {
            remove[source_idx] = true;
        }
    }

    if remove.iter().any(|&r| r) {
        let mut idx = 0;
        output.retain(|_| {
            let keep = !remove[idx];
            idx += 1;
            keep
        });
    }
    Ok(())
}

fn status_entries_are_exact_rename(
    deleted: &sley_worktree::ShortStatusEntry,
    added: &sley_worktree::ShortStatusEntry,
) -> bool {
    deleted.index == b'D'
        && deleted.worktree == b' '
        && added.index == b'A'
        && deleted.head_mode == added.index_mode
        && deleted.head_oid == added.index_oid
}

fn status_entries_are_exact_worktree_rename(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    deleted: &sley_worktree::ShortStatusEntry,
    added: &sley_worktree::ShortStatusEntry,
) -> Result<bool> {
    if deleted.index != b' '
        || deleted.worktree != b'D'
        || added.index != b' '
        || added.worktree != b'A'
        || deleted.index_mode != added.worktree_mode
    {
        return Ok(false);
    }
    let Some(index_oid) = deleted.index_oid else {
        return Ok(false);
    };
    let Some(worktree_oid) = status_worktree_blob_oid(worktree_root, git_dir, format, &added.path)?
    else {
        return Ok(false);
    };
    Ok(index_oid == worktree_oid)
}

fn status_worktree_blob_oid(
    worktree_root: &Path,
    _git_dir: &Path,
    format: ObjectFormat,
    path: &[u8],
) -> Result<Option<ObjectId>> {
    let absolute = worktree_root.join(repo_path_to_path(path));
    let metadata = match fs::symlink_metadata(&absolute) {
        Ok(metadata) => metadata,
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(None);
        }
        Err(err) => return Err(err.into()),
    };
    if metadata.is_dir() {
        return Ok(None);
    }
    let body = if metadata.file_type().is_symlink() {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            fs::read_link(&absolute)?.as_os_str().as_bytes().to_vec()
        }
        #[cfg(not(unix))]
        {
            fs::read_link(&absolute)?
                .to_string_lossy()
                .replace('\\', "/")
                .into_bytes()
        }
    } else {
        fs::read(&absolute)?
    };
    Ok(Some(
        EncodedObject::new(ObjectType::Blob, body).object_id(format)?,
    ))
}

fn status_entry_needs_submodule_ignore(entry: &sley_worktree::ShortStatusEntry) -> bool {
    entry.submodule.is_some()
        || entry.head_mode.is_some_and(sley_index::is_gitlink)
        || entry.index_mode.is_some_and(sley_index::is_gitlink)
        || entry.worktree_mode.is_some_and(sley_index::is_gitlink)
}

fn lazy_submodule_ignore_resolver<'a>(
    resolver: &'a mut Option<SubmoduleIgnoreResolver>,
    git_dir: &Path,
    config: &GitConfig,
    ignore_submodules_arg: Option<IgnoreSubmodules>,
) -> Result<&'a SubmoduleIgnoreResolver> {
    if resolver.is_none() {
        *resolver = Some(SubmoduleIgnoreResolver::load(
            git_dir,
            config,
            ignore_submodules_arg,
        )?);
    }
    Ok(resolver
        .as_ref()
        .expect("submodule ignore resolver initialized"))
}

fn status_option_takes_no_value_error(option: &str) -> Result<()> {
    eprintln!("error: option `{option}' takes no value");
    Err(GitError::Exit(129))
}

fn status_invalid_untracked_files_mode_error(mode: &str) -> Result<()> {
    eprintln!("fatal: Invalid untracked files mode '{mode}'");
    Err(GitError::Exit(128))
}

/// Parse a `-u<mode>` / `--untracked-files=<mode>` value. Upstream accepts the
/// keywords `no`/`normal`/`all` and the git-boolean forms (`true`/`yes`/`on`/`1`
/// → normal, `false`/`no`/`off`/`0`/empty → no), erroring otherwise.
fn parse_status_untracked_mode(value: &str) -> Result<sley_worktree::StatusUntrackedMode> {
    match value.to_ascii_lowercase().as_str() {
        "all" => Ok(sley_worktree::StatusUntrackedMode::All),
        "normal" | "true" | "yes" | "on" | "1" => Ok(sley_worktree::StatusUntrackedMode::Normal),
        "no" | "false" | "off" | "0" | "" => Ok(sley_worktree::StatusUntrackedMode::None),
        other => {
            status_invalid_untracked_files_mode_error(other)?;
            unreachable!()
        }
    }
}

fn status_invalid_ignored_mode_error(mode: &str) -> Result<()> {
    eprintln!("fatal: Invalid ignored mode '{mode}'");
    Err(GitError::Exit(128))
}

fn status_unsupported_porcelain_version_error(version: &str) -> Result<()> {
    eprintln!("fatal: unsupported porcelain version '{version}'");
    Err(GitError::Exit(128))
}

fn status_bad_ignore_submodules_argument_error(value: &str) -> Result<()> {
    eprintln!("fatal: bad --ignore-submodules argument: {value}");
    Err(GitError::Exit(128))
}

fn status_unsupported_column_option_error(value: &str) -> Result<()> {
    eprintln!("error: unsupported option '{value}'");
    Err(GitError::Exit(129))
}

struct StatusPathspec {
    prefix: Vec<u8>,
    filters: Vec<LsFilesPathFilter>,
    cwd_depth: usize,
}

impl StatusPathspec {
    fn new(cwd: &Path, worktree_root: &Path, path_args: &[String]) -> Result<Self> {
        let root = fs::canonicalize(worktree_root)?;
        let cwd = fs::canonicalize(cwd)?;
        // An explicit GIT_WORK_TREE may point away from the repository and the
        // process cwd. With no pathspec, Git scans that whole worktree; it does
        // not reject the command merely because the repository cwd is outside
        // it. A pathspec still needs an in-worktree cwd so relative arguments
        // have an unambiguous repository prefix.
        let prefix = match cwd.strip_prefix(&root) {
            Ok(relative) => relative.to_string_lossy().replace('\\', "/").into_bytes(),
            Err(_) if path_args.is_empty() => Vec::new(),
            Err(_) => {
                return Err(GitError::InvalidPath(format!(
                    "path {} is outside worktree",
                    cwd.display()
                )));
            }
        };
        let cwd_depth = path_component_count(&prefix);
        let mut filters = Vec::new();
        let magic = effective_pathspec_flags();
        for arg in path_args {
            let element = parse_normalized_pathspec_element(&prefix, arg, magic)?;
            let is_glob =
                !element.magic().literal && sley_worktree::pathspec_is_glob(element.pattern());
            let arg_path = Path::new(arg);
            let absolute = if arg_path.is_absolute() {
                arg_path.to_path_buf()
            } else {
                cwd.join(arg_path)
            };
            filters.push(LsFilesPathFilter {
                original: arg.clone(),
                recursive: arg == "." || arg.ends_with('/') || absolute.is_dir(),
                is_glob,
                element,
                matched: Cell::new(false),
            });
        }
        Ok(Self {
            prefix,
            filters,
            cwd_depth,
        })
    }

    fn has_filters(&self) -> bool {
        !self.filters.is_empty()
    }

    fn display(&self, path: &[u8]) -> Vec<u8> {
        if self.prefix.is_empty() {
            return path.to_vec();
        }
        if let Some(rest) = path.strip_prefix(self.prefix.as_slice())
            && let Some(rest) = rest.strip_prefix(b"/")
        {
            return rest.to_vec();
        }
        let mut display = Vec::new();
        for _ in 0..self.cwd_depth {
            display.extend_from_slice(b"../");
        }
        display.extend_from_slice(path);
        display
    }

    fn matches(&self, path: &[u8]) -> bool {
        pathspec_filters_match(&self.filters, path)
    }

    fn recursive_directory_for(&self, path: &[u8]) -> Option<Vec<u8>> {
        self.filters
            .iter()
            .filter(|filter| !filter.is_exclude() && filter.recursive && !filter.is_glob)
            .filter_map(|filter| {
                let directory = filter.element.pattern();
                if directory.is_empty() || path == directory {
                    return None;
                }
                path.strip_prefix(directory)
                    .and_then(|rest| rest.starts_with(b"/").then_some(directory))
            })
            .max_by_key(|directory| directory.len())
            .map(|directory| {
                let mut directory = directory.to_vec();
                if directory.last() != Some(&b'/') {
                    directory.push(b'/');
                }
                directory
            })
    }
}

fn print_status_porcelain_v2(
    git_dir: &Path,
    format: ObjectFormat,
    config: &GitConfig,
    entries: Vec<sley_worktree::ShortStatusEntry>,
    branch: bool,
    ahead_behind: bool,
    z: bool,
    show_stash: bool,
    rename_config: &StatusRenameConfig,
) -> Result<()> {
    let mut stdout = io::stdout().lock();
    let separator = if z { b'\0' } else { b'\n' };
    if branch {
        for header in status_porcelain_v2_branch_headers(git_dir, format, config, ahead_behind)? {
            stdout.write_all(header.as_bytes())?;
            stdout.write_all(&[separator])?;
        }
    }
    // `# stash <count>` follows the branch headers when `--show-stash` is set
    // and at least one stash entry exists (wt_porcelain_v2_print_stash).
    if show_stash {
        let stash_count = status_stash_count(git_dir, format)?;
        if stash_count > 0 {
            write!(stdout, "# stash {stash_count}")?;
            stdout.write_all(&[separator])?;
        }
    }
    let zero = zero_oid(format)?;
    let worktree_root = worktree_root_for_git_dir(git_dir).ok();
    let entries = match worktree_root.as_ref() {
        Some(worktree_root) => {
            status_entries_with_renames(worktree_root, git_dir, format, entries, rename_config)?
        }
        None => entries
            .into_iter()
            .map(|entry| StatusOutputEntry {
                entry,
                rename_from: None,
            })
            .collect(),
    };
    // Conflicted paths render as `u` records, read straight from the index
    // stages (the short-status entries carry no per-stage modes/oids). These
    // paths are skipped in the 1/2/?/! stream below.
    let unmerged = match worktree_root.as_ref() {
        Some(worktree_root) => {
            let trust_filemode = config.get_bool("core", None, "fileMode").unwrap_or(true);
            status_unmerged_v2_records(git_dir, worktree_root, format, trust_filemode)?
        }
        None => BTreeMap::new(),
    };
    let mut emitted_unmerged: BTreeSet<Vec<u8>> = BTreeSet::new();
    for output in entries {
        let entry = output.entry;
        if let Some(record) = unmerged.get(&entry.path) {
            if emitted_unmerged.insert(entry.path.clone()) {
                let submodule_token = if record
                    .stage_modes
                    .iter()
                    .any(|&mode| sley_index::is_gitlink(mode))
                {
                    "S..."
                } else {
                    "N..."
                };
                write!(
                    stdout,
                    "u {} {} {:06o} {:06o} {:06o} {:06o} {} {} {} ",
                    status_porcelain_v2_unmerged_key(record.stagemask),
                    submodule_token,
                    record.stage_modes[0],
                    record.stage_modes[1],
                    record.stage_modes[2],
                    record.worktree_mode,
                    record.stage_oids[0].to_hex(),
                    record.stage_oids[1].to_hex(),
                    record.stage_oids[2].to_hex(),
                )?;
                if z {
                    stdout.write_all(&entry.path)?;
                } else {
                    stdout.write_all(status_quote_path(&entry.path, false).as_bytes())?;
                }
                stdout.write_all(&[separator])?;
            }
            continue;
        }
        if entry.index == b'!' && entry.worktree == b'!' {
            stdout.write_all(b"! ")?;
            if z {
                stdout.write_all(&entry.path)?;
            } else {
                stdout.write_all(status_quote_path(&entry.path, false).as_bytes())?;
            }
            stdout.write_all(&[separator])?;
            continue;
        }
        if entry.index == b'?' && entry.worktree == b'?' {
            stdout.write_all(b"? ")?;
            if z {
                stdout.write_all(&entry.path)?;
            } else {
                stdout.write_all(status_quote_path(&entry.path, false).as_bytes())?;
            }
            stdout.write_all(&[separator])?;
            continue;
        }
        let index = status_porcelain_v2_code(entry.index);
        let worktree = status_porcelain_v2_code(entry.worktree);
        // Porcelain v2 submodule field (wt-status.c wt_porcelain_v2_*):
        // "N..." for an ordinary path; "S<C><M><U>" for a submodule, with C
        // for new commits, M for modified content, U for untracked content.
        if output.rename_from.is_some() {
            write!(stdout, "2 {index}{worktree} ",)?;
        } else {
            write!(stdout, "1 {index}{worktree} ",)?;
        }
        match entry.submodule {
            Some(submodule) => write!(
                stdout,
                "S{}{}{} ",
                if submodule.new_commits { 'C' } else { '.' },
                if submodule.modified_content { 'M' } else { '.' },
                if submodule.untracked_content {
                    'U'
                } else {
                    '.'
                },
            )?,
            None if entry.index_mode.is_some_and(sley_index::is_gitlink)
                || entry.worktree_mode.is_some_and(sley_index::is_gitlink) =>
            {
                stdout.write_all(b"S... ")?;
            }
            None => stdout.write_all(b"N... ")?,
        }
        if let Some(rename_from) = output.rename_from {
            write!(
                stdout,
                "{:06o} {:06o} {:06o} {} {} R100 ",
                entry.head_mode.unwrap_or(0),
                entry.index_mode.unwrap_or(0),
                entry.worktree_mode.unwrap_or(0),
                entry.head_oid.as_ref().unwrap_or(&zero).to_hex(),
                entry.index_oid.as_ref().unwrap_or(&zero).to_hex()
            )?;
            if z {
                stdout.write_all(&entry.path)?;
                stdout.write_all(&[separator])?;
                stdout.write_all(&rename_from)?;
            } else {
                stdout.write_all(status_quote_path(&entry.path, false).as_bytes())?;
                stdout.write_all(b"\t")?;
                stdout.write_all(status_quote_path(&rename_from, false).as_bytes())?;
            }
        } else {
            write!(
                stdout,
                "{:06o} {:06o} {:06o} {} {} ",
                entry.head_mode.unwrap_or(0),
                entry.index_mode.unwrap_or(0),
                entry.worktree_mode.unwrap_or(0),
                entry.head_oid.as_ref().unwrap_or(&zero).to_hex(),
                entry.index_oid.as_ref().unwrap_or(&zero).to_hex()
            )?;
            if z {
                stdout.write_all(&entry.path)?;
            } else {
                stdout.write_all(status_quote_path(&entry.path, false).as_bytes())?;
            }
        }
        stdout.write_all(&[separator])?;
    }
    stdout.flush()?;
    Ok(())
}

/// The `--ignore-submodules[=<when>]` / `submodule.<name>.ignore` /
/// `diff.ignoreSubmodules` levels, mirroring git's `enum submodule_ignore` and
/// the `dirty`/`untracked`/`all`/`none` keywords. Ordered by how much they hide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IgnoreSubmodules {
    /// `none`: show every kind of submodule change (the default).
    None,
    /// `untracked`: hide submodules whose only change is untracked content.
    Untracked,
    /// `dirty`: additionally hide modified (tracked) content; new commits still
    /// show.
    Dirty,
    /// `all`: hide the submodule entirely, including its summary section.
    All,
}

impl IgnoreSubmodules {
    /// Parse a `dirty`/`untracked`/`all`/`none` config/CLI keyword. Unknown
    /// values are treated as `None` (git's `parse_submodule_ignore` rejects them
    /// with a warning; for status purposes the safe fallback is to show
    /// everything).
    fn parse(value: &str) -> Option<Self> {
        match value {
            "none" => Some(Self::None),
            "untracked" => Some(Self::Untracked),
            "dirty" => Some(Self::Dirty),
            "all" => Some(Self::All),
            _ => None,
        }
    }
}

/// Resolves the effective per-submodule ignore setting from the four layered
/// sources, in git's precedence order: the `--ignore-submodules` command line
/// (applies to every submodule) wins over `submodule.<name>.ignore` in
/// `.git/config`, which wins over the same key in `.gitmodules`, which wins over
/// the global `diff.ignoreSubmodules`.
pub(crate) struct SubmoduleIgnoreResolver {
    /// `--ignore-submodules[=<when>]`; `Some` overrides every other source.
    cli: Option<IgnoreSubmodules>,
    /// `diff.ignoreSubmodules` — the all-submodule fallback.
    diff_default: Option<IgnoreSubmodules>,
    /// `submodule.<name>.ignore` read from `.git/config` (repo-local), keyed by
    /// the bound submodule path. Overrides the `.gitmodules` value.
    by_path_repo: BTreeMap<Vec<u8>, IgnoreSubmodules>,
    /// `submodule.<name>.ignore` read from `.gitmodules`, keyed by bound path.
    by_path_gitmodules: BTreeMap<Vec<u8>, IgnoreSubmodules>,
}

impl SubmoduleIgnoreResolver {
    pub(crate) fn load(
        git_dir: &Path,
        config: &GitConfig,
        cli: Option<IgnoreSubmodules>,
    ) -> Result<Self> {
        let diff_default = config
            .get("diff", None, "ignoreSubmodules")
            .and_then(IgnoreSubmodules::parse);
        // `.git/config`'s `submodule.<name>.ignore` + `.path` (the repo-local
        // override). `read_repo_config` already merges global+repo, but the
        // submodule sections we want are repo-local; read the raw repo config.
        let by_path_repo = submodule_ignore_by_path(config);
        // `.gitmodules` lives in the worktree root.
        let by_path_gitmodules = match worktree_root_for_git_dir(git_dir) {
            Ok(root) => GitConfig::read(root.join(".gitmodules"))
                .map(|cfg| submodule_ignore_by_path(&cfg))
                .unwrap_or_default(),
            Err(_) => BTreeMap::new(),
        };
        Ok(Self {
            cli,
            diff_default,
            by_path_repo,
            by_path_gitmodules,
        })
    }

    /// The effective ignore for the submodule bound at `path`.
    fn for_path(&self, path: &[u8]) -> IgnoreSubmodules {
        if let Some(cli) = self.cli {
            return cli;
        }
        if let Some(value) = self.by_path_repo.get(path) {
            return *value;
        }
        if let Some(value) = self.by_path_gitmodules.get(path) {
            return *value;
        }
        self.diff_default.unwrap_or(IgnoreSubmodules::None)
    }

    /// Whether the whole summary is suppressed by the command line. git gates the
    /// summary block on `!ignore_submodule_arg || strcmp(arg, "all")`, so a
    /// `--ignore-submodules=all` on the CLI hides both summary sections wholesale
    /// (per-submodule `all` is handled inside the summary instead).
    fn cli_suppresses_summary(&self) -> bool {
        self.cli == Some(IgnoreSubmodules::All)
    }
}

/// Extract `submodule.<name>.ignore` keyed by the submodule's bound `.path`,
/// from a single config source (`.git/config` or `.gitmodules`). Names without a
/// `.path` are dropped — without a path binding there is nothing to match a
/// status entry against.
fn submodule_ignore_by_path(config: &GitConfig) -> BTreeMap<Vec<u8>, IgnoreSubmodules> {
    let set = sley_submodule::SubmoduleConfigSet::parse(config);
    let mut map = BTreeMap::new();
    for sub in set.iter() {
        let (Some(path), Some(ignore)) = (
            sub.path.as_deref(),
            sub.ignore.as_deref().and_then(IgnoreSubmodules::parse),
        ) else {
            continue;
        };
        map.insert(path.as_bytes().to_vec(), ignore);
    }
    map
}

/// Apply the resolved per-submodule ignore to the worktree-side change detail of
/// each status entry, mirroring git's `handle_ignore_submodules_arg` before the
/// diff: `untracked` clears untracked-content, `dirty` additionally clears
/// modified-content, `all` clears every worktree change (the gitlink's `M`
/// worktree code and all three detail bits). New commits survive `dirty`/
/// `untracked` and are only hidden by `all`.
pub(crate) fn apply_submodule_ignore(
    entries: &mut Vec<sley_worktree::ShortStatusEntry>,
    resolver: &SubmoduleIgnoreResolver,
) {
    entries.retain_mut(|entry| apply_submodule_ignore_entry(entry, resolver));
}

fn apply_submodule_ignore_entry(
    entry: &mut sley_worktree::ShortStatusEntry,
    resolver: &SubmoduleIgnoreResolver,
) -> bool {
    // A bare `--ignore-submodules=all` on the COMMAND LINE sets the diffopt
    // ignore_submodules flag for the whole status run, hiding even the *staged*
    // gitlink change (`modified: sm` under "Changes to be committed"). A
    // per-submodule `ignore=all` from `.git/config`/`.gitmodules` does NOT — it
    // only touches the worktree-side detail and the summary (cells #93/#94 keep
    // the staged line).
    let cli_all = resolver.cli == Some(IgnoreSubmodules::All);
    let is_gitlink = entry.head_mode.is_some_and(sley_index::is_gitlink)
        || entry.index_mode.is_some_and(sley_index::is_gitlink)
        || entry.worktree_mode.is_some_and(sley_index::is_gitlink);
    if cli_all && is_gitlink {
        return false;
    }
    let Some(submodule) = entry.submodule.as_mut() else {
        return true;
    };
    let ignore = resolver.for_path(&entry.path);
    match ignore {
        IgnoreSubmodules::None => {}
        IgnoreSubmodules::Untracked => {
            submodule.untracked_content = false;
        }
        IgnoreSubmodules::Dirty => {
            submodule.untracked_content = false;
            submodule.modified_content = false;
        }
        IgnoreSubmodules::All => {
            submodule.new_commits = false;
            submodule.modified_content = false;
            submodule.untracked_content = false;
        }
    }
    if !submodule.any() {
        // No worktree-side submodule change survives the ignore. The gitlink
        // may still carry a *staged* (index) change; keep the entry only if
        // its index column is non-empty, and clear the worktree column so the
        // "Changes not staged" section drops it.
        entry.submodule = None;
        entry.worktree = b' ';
        return entry.index != b' ';
    }
    true
}

/// The two rendered long-status submodule-summary sections. Each `Vec<String>`
/// holds the lines of one section (header, blank, `* path old...new (N):`, and
/// the `  > subject` / `  < subject` lines), or is empty when that section has no
/// content. Empty by default (summary disabled or no gitlink changes).
#[derive(Default)]
pub(crate) struct SubmoduleSummarySections {
    staged: Vec<String>,
    unstaged: Vec<String>,
}

/// Build the `Submodule changes to be committed:` (HEAD↔index) and `Submodules
/// changed but not updated:` (index↔worktree) sections for the long status,
/// gated on `status.submodulesummary`. `base_ref` is the commit whose tree
/// supplies the staged comparison's "old" gitlinks (`HEAD`, or `HEAD^` for a
/// `commit --amend --dry-run`). A faithful port of wt-status.c's
/// `wt_longstatus_print_submodule_summary` → `git submodule summary
/// --cached/--files --for-status`.
pub(crate) fn status_submodule_summary(
    git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
    config: &GitConfig,
    base_ref: &str,
    resolver: &SubmoduleIgnoreResolver,
) -> Result<SubmoduleSummarySections> {
    let mut sections = SubmoduleSummarySections::default();
    let Some(limit) = status_submodule_summary_limit(config) else {
        return Ok(sections);
    };
    // `--ignore-submodules=all` on the command line drops the whole summary.
    if resolver.cli_suppresses_summary() {
        return Ok(sections);
    }
    let db = FileObjectDatabase::from_git_dir(git_dir, format);

    // "old" gitlinks: the base commit's tree (HEAD / HEAD^).
    let base_gitlinks = match sley_rev::resolve_revision(git_dir, format, base_ref) {
        Ok(commit_oid) => {
            let tree = sley_rev::peel_to_tree(&db, format, &commit_oid)?;
            tree_gitlinks(&db, format, &tree)?
        }
        // No base commit yet (unborn HEAD): every staged gitlink is "added".
        Err(_) => BTreeMap::new(),
    };
    // "index" gitlinks: what is staged right now.
    let index_gitlinks = index_gitlinks(git_dir, format)?;
    // "worktree" gitlinks: the commit each populated submodule actually has
    // checked out (its HEAD).
    let worktree_gitlinks = worktree_gitlinks(worktree_root, &index_gitlinks);

    // Staged: base-tree → index.
    let staged_pairs = gitlink_change_pairs(&base_gitlinks, &index_gitlinks);
    sections.staged = render_summary_section(
        worktree_root,
        format,
        resolver,
        limit,
        SUMMARY_HEADER_STAGED,
        &staged_pairs,
    )?;
    // Unstaged: index → worktree HEAD.
    let unstaged_pairs = gitlink_change_pairs(&index_gitlinks, &worktree_gitlinks);
    sections.unstaged = render_summary_section(
        worktree_root,
        format,
        resolver,
        limit,
        SUMMARY_HEADER_UNSTAGED,
        &unstaged_pairs,
    )?;
    Ok(sections)
}

const SUMMARY_HEADER_STAGED: &str = "Submodule changes to be committed:";
const SUMMARY_HEADER_UNSTAGED: &str = "Submodules changed but not updated:";

/// `status.submodulesummary` → the summary limit, or `None` when disabled. git
/// stores it as an int (`git_config_int`) with the boolean shorthand mapping
/// true→-1 (unlimited) and false/0→off. A positive N caps the `>`/`<` lines per
/// submodule; `-1` (true) means unlimited. `diff.submoduleSummary` does NOT
/// enable the *status* summary — only `status.submodulesummary` does.
fn status_submodule_summary_limit(config: &GitConfig) -> Option<i64> {
    let value = config.get("status", None, "submodulesummary")?;
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" => Some(-1),
        "false" | "no" | "off" | "" => None,
        other => match other.parse::<i64>() {
            Ok(0) => None,
            Ok(n) => Some(n),
            Err(_) => None,
        },
    }
}

/// Flatten a tree and keep only its gitlink (mode 160000) entries, path → oid.
fn tree_gitlinks(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &ObjectId,
) -> Result<BTreeMap<Vec<u8>, ObjectId>> {
    let flat = sley_diff_merge::flatten_tree(db, format, tree_oid)?;
    Ok(flat
        .into_iter()
        .filter(|(_, (mode, _))| sley_index::is_gitlink(*mode))
        .map(|(path, (_, oid))| (path, oid))
        .collect())
}

/// The gitlink entries in the index (stage-0), path → staged commit oid.
fn index_gitlinks(git_dir: &Path, format: ObjectFormat) -> Result<BTreeMap<Vec<u8>, ObjectId>> {
    let index_path = sley_worktree::repository_index_path(git_dir);
    if !index_path.exists() {
        return Ok(BTreeMap::new());
    }
    let index = Index::parse(&fs::read(&index_path)?, format)?;
    Ok(index
        .entries
        .iter()
        .filter(|entry| {
            entry.stage() == sley_index::Stage::Normal && sley_index::is_gitlink(entry.mode)
        })
        .map(|entry| (entry.path.to_vec(), entry.oid))
        .collect())
}

/// For each index gitlink, the commit its checked-out worktree actually has at
/// HEAD. A submodule whose worktree is absent / not a repository falls back to
/// the index oid (no unstaged change), matching git treating an unpopulated
/// gitlink as unchanged.
fn worktree_gitlinks(
    worktree_root: &Path,
    index_gitlinks: &BTreeMap<Vec<u8>, ObjectId>,
) -> BTreeMap<Vec<u8>, ObjectId> {
    let mut map = BTreeMap::new();
    for (path, index_oid) in index_gitlinks {
        let Ok(path_str) = std::str::from_utf8(path) else {
            continue;
        };
        let sub_root = worktree_root.join(path_str);
        // The submodule's repo always uses the super-repo's hash for its gitlink
        // oids in this corpus; read its HEAD with the same format.
        let oid = sley_diff_merge::gitlink_head_oid(&sub_root, ObjectFormat::Sha1)
            .or_else(|| sley_diff_merge::gitlink_head_oid(&sub_root, ObjectFormat::Sha256))
            .unwrap_or(*index_oid);
        map.insert(path.clone(), oid);
    }
    map
}

/// A gitlink change between two oid maps: paths present in both with differing
/// oids, plus pure additions (old null) and removals (new null). Returns
/// (path, old_oid_or_null, new_oid_or_null) sorted by path. `None` oid encodes
/// git's `null_oid` (a fresh / removed gitlink).
fn gitlink_change_pairs(
    old: &BTreeMap<Vec<u8>, ObjectId>,
    new: &BTreeMap<Vec<u8>, ObjectId>,
) -> Vec<(Vec<u8>, Option<ObjectId>, Option<ObjectId>)> {
    let mut out = Vec::new();
    let mut paths: BTreeSet<&Vec<u8>> = BTreeSet::new();
    paths.extend(old.keys());
    paths.extend(new.keys());
    for path in paths {
        let old_oid = old.get(path).copied();
        let new_oid = new.get(path).copied();
        if old_oid == new_oid {
            continue;
        }
        out.push((path.clone(), old_oid, new_oid));
    }
    out
}

/// Render one summary section's lines for the given header and change pairs.
/// Returns an empty vec (no header) when nothing renders, so the caller can skip
/// the whole block — git only prints the header `if (cmd_stdout.len)`.
fn render_summary_section(
    worktree_root: &Path,
    format: ObjectFormat,
    resolver: &SubmoduleIgnoreResolver,
    limit: i64,
    header: &str,
    pairs: &[(Vec<u8>, Option<ObjectId>, Option<ObjectId>)],
) -> Result<Vec<String>> {
    let mut bodies: Vec<String> = Vec::new();
    for (path, old_oid, new_oid) in pairs {
        // Per-submodule `ignore=all` (from .git/config or .gitmodules, NOT the
        // CLI which already short-circuited) skips this submodule's summary
        // unless it is a pure addition — git's prepare_submodule_summary keeps
        // status 'A' even under ignore=all.
        let is_addition = old_oid.is_none();
        if !is_addition && resolver.for_path(path) == IgnoreSubmodules::All {
            continue;
        }
        let Some(body) =
            render_one_submodule(worktree_root, format, limit, path, *old_oid, *new_oid)?
        else {
            continue;
        };
        bodies.push(body);
    }
    if bodies.is_empty() {
        return Ok(Vec::new());
    }
    // header, blank, then each submodule body (already multi-line, no trailing
    // newline). The caller separates this whole block from neighbours.
    let mut lines = vec![header.to_string(), String::new()];
    for body in bodies {
        for line in body.lines() {
            lines.push(line.to_string());
        }
    }
    Ok(lines)
}

/// Render `* <path> <old7>...<new7> (N):` plus up to `limit` `> subject` /
/// `< subject` lines for one changed gitlink. `None` when the submodule's repo is
/// not populated (git only summarises checked-out submodules) — the caller drops
/// it. A faithful port of `generate_submodule_summary` for the gitlink→gitlink
/// case (type changes to/from a blob do not occur for a status gitlink change).
fn render_one_submodule(
    worktree_root: &Path,
    format: ObjectFormat,
    limit: i64,
    path: &[u8],
    old_oid: Option<ObjectId>,
    new_oid: Option<ObjectId>,
) -> Result<Option<String>> {
    use std::fmt::Write as _;

    let Ok(path_str) = std::str::from_utf8(path) else {
        return Ok(None);
    };
    let sub_root = worktree_root.join(path_str);
    // git: `prepare_submodule_summary` only summarises submodules whose worktree
    // is a non-bare repository (is_nonbare_repository_dir); skip otherwise.
    let Some(sub_git_dir) = sley_diff_merge::gitlink_git_dir(&sub_root) else {
        return Ok(None);
    };
    let sub_db = FileObjectDatabase::from_git_dir(&sub_git_dir, format);

    let null = ObjectId::null(format);
    let old = old_oid.unwrap_or(null);
    let new = new_oid.unwrap_or(null);
    let src_abbrev = abbrev7(&old);
    let dst_abbrev = abbrev7(&new);
    // git treats a null oid as "not a gitlink" (mode 0): the source of a fresh
    // submodule add, or the dest of a removal. Both sides being gitlinks is the
    // common case; a null side switches to the single-tip rendering.
    let src_is_gitlink = old_oid.is_some();
    let dst_is_gitlink = new_oid.is_some();

    // Whether each *gitlink* side's commit is present in the submodule's object
    // store (git's verify_submodule_committish). A null side is never "missing".
    let src_present = !src_is_gitlink || sub_db.read_object(&old).is_ok();
    let dst_present = !dst_is_gitlink || sub_db.read_object(&new).is_ok();

    if !src_present || !dst_present {
        // git only warns when the destination is still a gitlink (it is here).
        let warn = if !src_present && !dst_present {
            format!(
                "  Warn: {path_str} doesn't contain commits {} and {}\n",
                old.to_hex(),
                new.to_hex()
            )
        } else {
            let missing = if !src_present { &old } else { &new };
            format!(
                "  Warn: {path_str} doesn't contain commit {}\n",
                missing.to_hex()
            )
        };
        return Ok(Some(format!(
            "* {path_str} {src_abbrev}...{dst_abbrev}:\n{warn}"
        )));
    }

    let (total, marked) = if src_is_gitlink && dst_is_gitlink {
        // Symmetric first-parent difference, marked + date-ordered like
        // `git log --first-parent --pretty="  %m %s" src...dst`. The count is
        // `rev-list --first-parent --count src...dst`.
        let marked = submodule_summary_log(&sub_db, format, &old, &new)?;
        (marked.len(), marked)
    } else if dst_is_gitlink {
        // Fresh submodule add: count = `rev-list --first-parent --count dst`; one
        // `> dst` line (git uses `--pretty="  > %s" -1 dst`).
        let chain = first_parent_chain(&sub_db, format, &new)?;
        let subject = chain.first().map(|c| c.subject.clone()).unwrap_or_default();
        (chain.len(), vec![('>', subject)])
    } else {
        // Submodule removal: count = first-parent commits from src; one `< src`.
        let chain = first_parent_chain(&sub_db, format, &old)?;
        let subject = chain.first().map(|c| c.subject.clone()).unwrap_or_default();
        (chain.len(), vec![('<', subject)])
    };

    let mut body = format!("* {path_str} {src_abbrev}...{dst_abbrev} ({total}):\n");
    // The single-tip add/remove forms always show their one line (git's `-1`);
    // only the gitlink↔gitlink form honours the summary limit.
    let shown = if src_is_gitlink && dst_is_gitlink && limit > 0 {
        (limit as usize).min(marked.len())
    } else {
        marked.len()
    };
    for (marker, subject) in marked.iter().take(shown) {
        writeln!(body, "  {marker} {subject}").expect("writing to String cannot fail");
    }
    Ok(Some(body))
}

/// `git rev-parse --short <oid>^0` for the tiny submodule repos in the corpus is
/// a fixed 7-char abbreviation (git's own fallback `xstrndup(oid_to_hex, 7)`).
fn abbrev7(oid: &ObjectId) -> String {
    oid.to_hex()[..7].to_string()
}

/// Walk the symmetric first-parent difference `src...dst` in the submodule's
/// object store and return `(marker, subject)` pairs in git's log order: a
/// commit-date priority walk from both tips, marking `<` for the src side and
/// `>` for the dst side, following only first parents, stopping where the two
/// histories meet. Equivalent to
/// `git log --first-parent --pretty="  %m %s" src...dst` over the gitlink commits.
fn submodule_summary_log(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    src: &ObjectId,
    dst: &ObjectId,
) -> Result<Vec<(char, String)>> {
    // First-parent ancestor chains of each tip.
    let src_chain = first_parent_chain(db, format, src)?;
    let dst_chain = first_parent_chain(db, format, dst)?;
    let src_set: HashSet<ObjectId> = src_chain.iter().map(|c| c.oid).collect();
    let dst_set: HashSet<ObjectId> = dst_chain.iter().map(|c| c.oid).collect();

    // A commit-date max-heap seeded with each tip, tagged with its side. As each
    // commit is emitted we push its first parent (lazily), so a child always
    // precedes its parent and ties resolve to the newer date first — exactly the
    // pop order of git's `src...dst` walk.
    let mut by_oid: HashMap<ObjectId, FpCommit> = HashMap::new();
    for c in src_chain.into_iter().chain(dst_chain.into_iter()) {
        by_oid.entry(c.oid).or_insert(c);
    }

    // Marker per emitted oid: `<` if only in src, `>` if only in dst. Commits in
    // BOTH are the common base and are never emitted (uninteresting boundary).
    let marker_for = |oid: &ObjectId| -> Option<char> {
        let in_src = src_set.contains(oid);
        let in_dst = dst_set.contains(oid);
        match (in_src, in_dst) {
            (true, false) => Some('<'),
            (false, true) => Some('>'),
            _ => None,
        }
    };

    let mut heap: std::collections::BinaryHeap<SummaryHeapEntry> = Default::default();
    let mut pushed: HashSet<ObjectId> = HashSet::new();
    for tip in [src, dst] {
        if let Some(c) = by_oid.get(tip) {
            if pushed.insert(*tip) {
                heap.push(SummaryHeapEntry {
                    time: c.commit_time,
                    oid: *tip,
                });
            }
        }
    }

    let mut out = Vec::new();
    while let Some(entry) = heap.pop() {
        let Some(commit) = by_oid.get(&entry.oid) else {
            continue;
        };
        let first_parent = commit.first_parent;
        if let Some(marker) = marker_for(&entry.oid) {
            out.push((marker, commit.subject.clone()));
        }
        // Push the first parent so the chain continues toward the merge base.
        if let Some(parent) = first_parent {
            if let Some(pc) = by_oid.get(&parent) {
                if pushed.insert(parent) {
                    heap.push(SummaryHeapEntry {
                        time: pc.commit_time,
                        oid: parent,
                    });
                }
            }
        }
    }
    Ok(out)
}

/// One commit's first-parent-walk metadata for the summary log.
struct FpCommit {
    oid: ObjectId,
    first_parent: Option<ObjectId>,
    commit_time: i64,
    subject: String,
}

/// The chain of commits reachable from `tip` by following ONLY first parents,
/// reading each commit's subject + committer time.
fn first_parent_chain(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tip: &ObjectId,
) -> Result<Vec<FpCommit>> {
    let mut chain = Vec::new();
    let mut cursor = Some(*tip);
    let mut seen = HashSet::new();
    while let Some(oid) = cursor {
        if !seen.insert(oid) {
            break;
        }
        let object = db.read_object(&oid)?;
        if object.object_type != ObjectType::Commit {
            break;
        }
        let commit = sley_object::Commit::parse(format, &object.body)?;
        let first_parent = commit.parents.first().copied();
        chain.push(FpCommit {
            oid,
            first_parent,
            commit_time: commit_committer_time(&commit.committer),
            subject: commit_subject(&commit.message),
        });
        cursor = first_parent;
    }
    Ok(chain)
}

/// Parse the committer timestamp (seconds since epoch) from a commit's committer
/// identity line (`Name <email> <secs> <tz>`). Falls back to 0 when unparsable —
/// the corpus always carries a well-formed timestamp.
fn commit_committer_time(committer: &[u8]) -> i64 {
    let text = String::from_utf8_lossy(committer);
    let mut parts = text.rsplit(' ');
    let _tz = parts.next();
    parts
        .next()
        .and_then(|secs| secs.parse::<i64>().ok())
        .unwrap_or(0)
}

/// Max-heap entry for the summary's date-priority walk: newest commit-time pops
/// first; ties break on the SMALLER oid (matching sley's RevWalk heap and git's
/// `(time, Reverse(oid))` ordering).
struct SummaryHeapEntry {
    time: i64,
    oid: ObjectId,
}
impl PartialEq for SummaryHeapEntry {
    fn eq(&self, other: &Self) -> bool {
        self.time == other.time && self.oid == other.oid
    }
}
impl Eq for SummaryHeapEntry {}
impl Ord for SummaryHeapEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.time
            .cmp(&other.time)
            .then_with(|| other.oid.cmp(&self.oid))
    }
}
impl PartialOrd for SummaryHeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// The effective `core.commentChar` string (git's `comment_line_str`), default
/// `#`. May be multi-char; an empty or `auto` value falls back to `#` (we do not
/// implement the `auto` scan, which picks an unused character). Used by
/// commit-message cleanup (scissors detection + comment stripping).
pub(crate) fn commit_comment_string(git_dir: &Path) -> String {
    read_repo_config(git_dir)
        .ok()
        .and_then(|c| {
            c.get("core", None, "commentchar")
                .filter(|value| !value.is_empty() && *value != "auto")
                .map(str::to_string)
        })
        .unwrap_or_else(|| "#".to_string())
}

/// Comment prefix for `git status` output when `status.displayCommentPrefix` is
/// on. Upstream uses `core.commentChar` (default `#`); the prefix string is the
/// comment char (which may be multi-byte / multi-char). Returns `None` when the
/// prefix is disabled.
pub(crate) fn status_comment_prefix(config: &GitConfig) -> Option<String> {
    if config.get_bool("status", None, "displayCommentPrefix") != Some(true) {
        return None;
    }
    let comment_char = config
        .get("core", None, "commentchar")
        .filter(|value| !value.is_empty() && *value != "auto")
        .unwrap_or("#");
    Some(comment_char.to_string())
}

/// Buffers long-status lines so the comment prefix (and, where relevant, hint
/// gating) can be applied uniformly on flush — mirroring upstream's
/// status_vprintf(), which prefixes every emitted line.
pub(crate) struct StatusLineSink {
    lines: Vec<String>,
    hints: bool,
    comment_prefix: Option<String>,
}

impl StatusLineSink {
    pub(crate) fn new(hints: bool, comment_prefix: Option<String>) -> Self {
        Self {
            lines: Vec::new(),
            hints,
            comment_prefix,
        }
    }

    /// A normal output line.
    fn line(&mut self, text: impl Into<String>) {
        self.lines.push(text.into());
    }

    /// A blank separator line.
    fn blank(&mut self) {
        self.lines.push(String::new());
    }

    /// A parenthetical guidance line, suppressed when `advice.statusHints` is
    /// false (upstream gates all `(use "git ...")` hints on `s->hints`).
    fn hint(&mut self, text: impl Into<String>) {
        if self.hints {
            self.lines.push(text.into());
        }
    }

    fn flush(self) {
        let mut out = io::stdout().lock();
        self.write_to(&mut out);
        let _ = out.flush();
    }

    /// Render the buffered lines (with the comment prefix applied) into an
    /// arbitrary writer. Used both for stdout (status preview) and for building
    /// the COMMIT_EDITMSG template block.
    pub(crate) fn write_to(&self, out: &mut impl Write) {
        for line in &self.lines {
            if let Some(prefix) = &self.comment_prefix {
                if line.is_empty() {
                    // Empty line → just the comment char (no trailing space).
                    let _ = writeln!(out, "{prefix}");
                } else if line.starts_with('\t') {
                    // Indented (file) lines: comment char immediately, no space.
                    let _ = writeln!(out, "{prefix}{line}");
                } else {
                    let _ = writeln!(out, "{prefix} {line}");
                }
            } else {
                let _ = writeln!(out, "{line}");
            }
        }
    }
}

pub(crate) fn print_status_long(
    git_dir: &Path,
    format: ObjectFormat,
    entries: Vec<sley_worktree::ShortStatusEntry>,
    display: &StatusLongDisplay,
) -> Result<()> {
    let sink = build_status_long_sink_inner(git_dir, format, entries, display, false)?;
    sink.flush();
    Ok(())
}

fn print_status_long_with_column(
    git_dir: &Path,
    format: ObjectFormat,
    entries: Vec<sley_worktree::ShortStatusEntry>,
    display: &StatusLongDisplay,
    column_untracked: bool,
) -> Result<()> {
    let sink = build_status_long_sink_inner(git_dir, format, entries, display, column_untracked)?;
    sink.flush();
    Ok(())
}

#[derive(Debug, Clone)]
struct StatusUnmergedPath {
    path: Vec<u8>,
    label: &'static str,
    stages: BTreeSet<u16>,
}

impl StatusUnmergedPath {
    fn has_index_side(&self) -> bool {
        self.stages.contains(&2)
    }

    /// Upstream `stagemask`: bit 0 = stage 1 (base), bit 1 = stage 2 (ours),
    /// bit 2 = stage 3 (theirs).
    fn stagemask(&self) -> u8 {
        self.stages
            .iter()
            .fold(0u8, |mask, &stage| mask | (1 << (stage - 1)))
    }
}

fn status_unmerged_paths(git_dir: &Path, format: ObjectFormat) -> Result<Vec<StatusUnmergedPath>> {
    let Some(index) = sley_worktree::read_repository_index(git_dir, format)? else {
        return Ok(Vec::new());
    };
    let mut by_path: BTreeMap<Vec<u8>, BTreeSet<u16>> = BTreeMap::new();
    for entry in index.entries {
        let stage = index_entry_stage(&entry);
        if stage > 0 {
            by_path
                .entry(entry.path.into_bytes())
                .or_default()
                .insert(stage);
        }
    }
    Ok(by_path
        .into_iter()
        .map(|(path, stages)| StatusUnmergedPath {
            label: status_unmerged_label(&stages),
            path,
            stages,
        })
        .collect())
}

/// Per-path conflict data for the porcelain v2 `u` record: stage 1/2/3 modes and
/// oids (0/zero when a stage is absent), the worktree file mode, and the
/// stagemask that selects the `DD`/`AU`/… key.
struct StatusUnmergedV2 {
    stagemask: u8,
    stage_modes: [u32; 3],
    stage_oids: [ObjectId; 3],
    worktree_mode: u32,
}

fn status_unmerged_v2_records(
    git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
    trust_filemode: bool,
) -> Result<BTreeMap<Vec<u8>, StatusUnmergedV2>> {
    let Some(index) = sley_worktree::read_repository_index(git_dir, format)? else {
        return Ok(BTreeMap::new());
    };
    let zero = ObjectId::null(format);
    let mut by_path: BTreeMap<Vec<u8>, StatusUnmergedV2> = BTreeMap::new();
    for entry in index.entries {
        let stage = index_entry_stage(&entry);
        if stage == 0 {
            continue;
        }
        let record = by_path
            .entry(entry.path.into_bytes())
            .or_insert_with(|| StatusUnmergedV2 {
                stagemask: 0,
                stage_modes: [0; 3],
                stage_oids: [zero; 3],
                worktree_mode: 0,
            });
        let slot = (stage - 1) as usize;
        record.stage_modes[slot] = entry.mode;
        record.stage_oids[slot] = entry.oid;
        record.stagemask |= 1 << slot;
    }
    for (path, record) in &mut by_path {
        record.worktree_mode =
            status_worktree_blob_mode(worktree_root, path, trust_filemode)?.unwrap_or(0);
    }
    Ok(by_path)
}

/// The canonical worktree blob mode (`100644`/`100755`/`120000`) for `path`, or
/// `None` when it is absent or a directory. Honors `core.fileMode`: with the
/// executable bit untrusted, a regular file always reports `100644`.
fn status_worktree_blob_mode(
    worktree_root: &Path,
    path: &[u8],
    trust_filemode: bool,
) -> Result<Option<u32>> {
    let absolute = worktree_root.join(repo_path_to_path(path));
    let metadata = match fs::symlink_metadata(&absolute) {
        Ok(metadata) => metadata,
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(None);
        }
        Err(err) => return Err(err.into()),
    };
    if metadata.file_type().is_symlink() {
        return Ok(Some(0o120000));
    }
    if !metadata.is_file() {
        return Ok(None);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let executable = trust_filemode && (metadata.permissions().mode() & 0o111 != 0);
        Ok(Some(if executable { 0o100755 } else { 0o100644 }))
    }
    #[cfg(not(unix))]
    {
        let _ = trust_filemode;
        Ok(Some(0o100644))
    }
}

fn status_unmerged_label(stages: &BTreeSet<u16>) -> &'static str {
    match (
        stages.contains(&1),
        stages.contains(&2),
        stages.contains(&3),
    ) {
        (true, true, true) => "both modified:",
        (true, true, false) => "deleted by them:",
        (true, false, true) => "deleted by us:",
        (true, false, false) => "both deleted:",
        (false, true, true) => "both added:",
        (false, true, false) => "added by us:",
        (false, false, true) => "added by them:",
        _ => "unmerged:",
    }
}

fn status_long_operation_lines(
    git_dir: &Path,
    format: ObjectFormat,
    has_unmerged: bool,
    has_staged: bool,
    has_unstaged: bool,
    sink: &mut StatusLineSink,
) -> Result<bool> {
    if git_dir.join("MERGE_HEAD").is_file() {
        if has_unmerged {
            sink.line("You have unmerged paths.");
            sink.hint("  (fix conflicts and run \"git commit\")");
            sink.hint("  (use \"git merge --abort\" to abort the merge)");
        } else {
            sink.line("All conflicts fixed but you are still merging.");
            sink.hint("  (use \"git commit\" to conclude merge)");
        }
        return Ok(true);
    }
    if let Some(am) = status_am_state(git_dir)? {
        sink.line("You are in the middle of an am session.");
        if am.empty_patch {
            sink.line("The current patch is empty.");
        }
        if !am.empty_patch {
            sink.hint("  (fix conflicts and then run \"git am --continue\")");
        }
        sink.hint("  (use \"git am --skip\" to skip this patch)");
        if am.empty_patch {
            sink.hint("  (use \"git am --allow-empty\" to record this patch as an empty commit)");
        }
        sink.hint("  (use \"git am --abort\" to restore the original branch)");
        status_long_bisect_lines(git_dir, format, true, sink)?;
        return Ok(true);
    }
    if let Some(rebase) = status_rebase_state(git_dir, format)? {
        status_rebase_information(git_dir, format, &rebase, sink)?;
        if has_unmerged {
            status_rebase_state_line(&rebase, "rebasing", sink);
            sink.hint("  (fix conflicts and then run \"git rebase --continue\")");
            sink.hint("  (use \"git rebase --skip\" to skip this patch)");
            sink.hint("  (use \"git rebase --abort\" to check out the original branch)");
        } else if !rebase.interactive || git_dir.join("MERGE_MSG").is_file() {
            status_rebase_state_line(&rebase, "rebasing", sink);
            sink.hint("  (all conflicts fixed: run \"git rebase --continue\")");
        } else if status_split_commit_in_progress(git_dir, has_staged, has_unstaged) {
            status_rebase_state_line(&rebase, "splitting", sink);
            sink.hint("  (Once your working directory is clean, run \"git rebase --continue\")");
        } else {
            status_rebase_state_line(&rebase, "editing", sink);
            sink.hint("  (use \"git commit --amend\" to amend the current commit)");
            sink.hint("  (use \"git rebase --continue\" once you are satisfied with your changes)");
        }
        status_long_bisect_lines(git_dir, format, true, sink)?;
        return Ok(true);
    }
    if git_dir.join("CHERRY_PICK_HEAD").is_file() {
        let oid = status_state_oid(git_dir, format, "CHERRY_PICK_HEAD")?;
        sink.line(format!("You are currently cherry-picking commit {oid}."));
        if has_unmerged {
            sink.hint("  (fix conflicts and run \"git cherry-pick --continue\")");
        } else if has_staged {
            sink.hint("  (all conflicts fixed: run \"git cherry-pick --continue\")");
        } else {
            sink.hint("  (run \"git cherry-pick --continue\" to continue)");
        }
        sink.hint("  (use \"git cherry-pick --skip\" to skip this patch)");
        sink.hint("  (use \"git cherry-pick --abort\" to cancel the cherry-pick operation)");
        return Ok(true);
    }
    if git_dir.join("REVERT_HEAD").is_file() {
        let oid = status_state_oid(git_dir, format, "REVERT_HEAD")?;
        sink.line(format!("You are currently reverting commit {oid}."));
        if has_unmerged {
            sink.hint("  (fix conflicts and run \"git revert --continue\")");
        } else if has_staged {
            sink.hint("  (all conflicts fixed: run \"git revert --continue\")");
        } else {
            sink.hint("  (run \"git revert --continue\" to continue)");
        }
        sink.hint("  (use \"git revert --skip\" to skip this patch)");
        sink.hint("  (use \"git revert --abort\" to cancel the revert operation)");
        return Ok(true);
    }
    match status_sequencer_action(git_dir) {
        Some(SequencerAction::Pick) => {
            sink.line("Cherry-pick currently in progress.");
            sink.hint("  (run \"git cherry-pick --continue\" to continue)");
            sink.hint("  (use \"git cherry-pick --skip\" to skip this patch)");
            sink.hint("  (use \"git cherry-pick --abort\" to cancel the cherry-pick operation)");
            Ok(true)
        }
        Some(SequencerAction::Revert) => {
            sink.line("Revert currently in progress.");
            sink.hint("  (run \"git revert --continue\" to continue)");
            sink.hint("  (use \"git revert --skip\" to skip this patch)");
            sink.hint("  (use \"git revert --abort\" to cancel the revert operation)");
            Ok(true)
        }
        None => Ok(false),
    }
}

#[derive(Debug, Clone)]
struct StatusAmState {
    empty_patch: bool,
}

fn status_am_state(git_dir: &Path) -> Result<Option<StatusAmState>> {
    let state = git_dir.join("rebase-apply");
    if !state.join("applying").is_file() || state.join("head-name").is_file() {
        return Ok(None);
    }
    let empty_patch = fs::metadata(state.join("patch"))
        .map(|meta| meta.len() == 0)
        .unwrap_or(false);
    Ok(Some(StatusAmState { empty_patch }))
}

#[derive(Debug, Clone)]
struct StatusRebaseState {
    dir_name: &'static str,
    interactive: bool,
    branch: Option<String>,
    onto: Option<String>,
}

fn status_rebase_state(git_dir: &Path, format: ObjectFormat) -> Result<Option<StatusRebaseState>> {
    let apply = git_dir.join("rebase-apply");
    if apply.is_dir() && apply.join("head-name").is_file() {
        return Ok(Some(StatusRebaseState {
            dir_name: "rebase-apply",
            interactive: false,
            branch: status_state_branch(git_dir, format, "rebase-apply/head-name")?,
            onto: status_state_branch(git_dir, format, "rebase-apply/onto")?,
        }));
    }
    let merge = git_dir.join("rebase-merge");
    if merge.is_dir() {
        return Ok(Some(StatusRebaseState {
            dir_name: "rebase-merge",
            interactive: merge.join("interactive").is_file(),
            branch: status_state_branch(git_dir, format, "rebase-merge/head-name")?,
            onto: status_state_branch(git_dir, format, "rebase-merge/onto")?,
        }));
    }
    Ok(None)
}

fn status_state_branch(git_dir: &Path, format: ObjectFormat, name: &str) -> Result<Option<String>> {
    let Ok(mut text) = fs::read_to_string(git_dir.join(name)) else {
        return Ok(None);
    };
    while text.ends_with('\n') {
        text.pop();
    }
    if text.is_empty() || text == "detached HEAD" {
        return Ok(None);
    }
    if let Some(branch) = text.strip_prefix("refs/heads/") {
        return Ok(Some(branch.to_string()));
    }
    if text.starts_with("refs/") {
        return Ok(Some(text));
    }
    if let Ok(oid) = ObjectId::from_hex(format, &text) {
        return Ok(Some(format_log_abbrev_oid(&oid)));
    }
    Ok(Some(text))
}

fn status_rebase_information(
    git_dir: &Path,
    format: ObjectFormat,
    rebase: &StatusRebaseState,
    sink: &mut StatusLineSink,
) -> Result<()> {
    if !rebase.interactive {
        return Ok(());
    }
    let done = status_rebase_todo_lines(git_dir, format, &format!("{}/done", rebase.dir_name))?;
    let todo = status_rebase_todo_lines(
        git_dir,
        format,
        &format!("{}/git-rebase-todo", rebase.dir_name),
    )?;
    if done.is_empty() {
        sink.line("No commands done.");
    } else if done.len() == 1 {
        sink.line("Last command done (1 command done):");
        sink.line(format!("   {}", done[0]));
    } else {
        sink.line(format!(
            "Last commands done ({} commands done):",
            done.len()
        ));
        let start = done.len().saturating_sub(2);
        for line in &done[start..] {
            sink.line(format!("   {line}"));
        }
        if done.len() > 2 {
            sink.hint(format!(
                "  (see more in file .git/{}/done)",
                rebase.dir_name
            ));
        }
    }

    if todo.is_empty() {
        sink.line("No commands remaining.");
    } else if todo.len() == 1 {
        sink.line("Next command to do (1 remaining command):");
        sink.line(format!("   {}", todo[0]));
        sink.hint("  (use \"git rebase --edit-todo\" to view and edit)");
    } else {
        sink.line(format!(
            "Next commands to do ({} remaining commands):",
            todo.len()
        ));
        for line in todo.iter().take(2) {
            sink.line(format!("   {line}"));
        }
        sink.hint("  (use \"git rebase --edit-todo\" to view and edit)");
    }
    Ok(())
}

fn status_rebase_todo_lines(
    git_dir: &Path,
    format: ObjectFormat,
    name: &str,
) -> Result<Vec<String>> {
    let Ok(text) = fs::read_to_string(git_dir.join(name)) else {
        return Ok(Vec::new());
    };
    Ok(text
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                None
            } else {
                Some(status_rebase_abbrev_todo_line(format, trimmed))
            }
        })
        .collect())
}

fn status_rebase_abbrev_todo_line(format: ObjectFormat, line: &str) -> String {
    if line.starts_with("exec ")
        || line.starts_with("x ")
        || line.starts_with("label ")
        || line.starts_with("l ")
    {
        return line.to_string();
    }
    let mut parts = line.splitn(3, ' ');
    let Some(command) = parts.next() else {
        return line.to_string();
    };
    let Some(oid_text) = parts.next() else {
        return line.to_string();
    };
    let Some(rest) = parts.next() else {
        return line.to_string();
    };
    let Ok(oid) = ObjectId::from_hex(format, oid_text) else {
        return line.to_string();
    };
    format!("{command} {} {rest}", format_log_abbrev_oid(&oid))
}

fn status_rebase_state_line(rebase: &StatusRebaseState, mode: &str, sink: &mut StatusLineSink) {
    match (mode, rebase.branch.as_deref(), rebase.onto.as_deref()) {
        ("rebasing", Some(branch), Some(onto)) => sink.line(format!(
            "You are currently rebasing branch '{branch}' on '{onto}'."
        )),
        ("splitting", Some(branch), Some(onto)) => sink.line(format!(
            "You are currently splitting a commit while rebasing branch '{branch}' on '{onto}'."
        )),
        ("editing", Some(branch), Some(onto)) => sink.line(format!(
            "You are currently editing a commit while rebasing branch '{branch}' on '{onto}'."
        )),
        ("splitting", _, _) => sink.line("You are currently splitting a commit during a rebase."),
        ("editing", _, _) => sink.line("You are currently editing a commit during a rebase."),
        _ => sink.line("You are currently rebasing."),
    }
}

fn status_split_commit_in_progress(git_dir: &Path, has_staged: bool, has_unstaged: bool) -> bool {
    git_dir.join("rebase-merge").join("amend").is_file() && has_unstaged && !has_staged
}

fn status_long_bisect_lines(
    git_dir: &Path,
    format: ObjectFormat,
    after_state: bool,
    sink: &mut StatusLineSink,
) -> Result<bool> {
    if !git_dir.join("BISECT_LOG").is_file() {
        return Ok(false);
    }
    if after_state {
        sink.blank();
    }
    if let Some(branch) = status_state_branch(git_dir, format, "BISECT_START")? {
        sink.line(format!(
            "You are currently bisecting, started from branch '{branch}'."
        ));
    } else {
        sink.line("You are currently bisecting.");
    }
    sink.hint("  (use \"git bisect reset\" to get back to the original branch)");
    Ok(true)
}

fn status_state_oid(git_dir: &Path, format: ObjectFormat, name: &str) -> Result<String> {
    let text = fs::read_to_string(git_dir.join(name))?;
    let oid = ObjectId::from_hex(format, text.trim())?;
    Ok(format_log_abbrev_oid(&oid))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SequencerAction {
    Pick,
    Revert,
}

fn status_sequencer_action(git_dir: &Path) -> Option<SequencerAction> {
    let todo = fs::read_to_string(git_dir.join("sequencer").join("todo")).ok()?;
    for line in todo.lines() {
        let line = line.trim_start();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with("pick ") {
            return Some(SequencerAction::Pick);
        }
        if line.starts_with("revert ") {
            return Some(SequencerAction::Revert);
        }
    }
    None
}

/// Build (but do not emit) the buffered long-status output. Shared by the
/// `git status` stdout path and the COMMIT_EDITMSG template builder.
pub(crate) fn build_status_long_sink(
    git_dir: &Path,
    format: ObjectFormat,
    entries: Vec<sley_worktree::ShortStatusEntry>,
    display: &StatusLongDisplay,
) -> Result<StatusLineSink> {
    build_status_long_sink_inner(git_dir, format, entries, display, false)
}

fn build_status_long_sink_inner(
    git_dir: &Path,
    format: ObjectFormat,
    entries: Vec<sley_worktree::ShortStatusEntry>,
    display: &StatusLongDisplay,
    column_untracked: bool,
) -> Result<StatusLineSink> {
    let StatusLongDisplay {
        commit_preview,
        show_stash,
        ahead_behind,
        hints,
        untracked_suppressed,
        comment_prefix,
        submodule_summary,
        sparse_footer,
        rename_config,
    } = display;
    let commit_preview = *commit_preview;
    let show_stash = *show_stash;
    let ahead_behind = *ahead_behind;
    let untracked_suppressed = *untracked_suppressed;

    let mut sink = StatusLineSink::new(*hints, comment_prefix.clone());
    // `commit --dry-run`/template previews suppress the upstream-divergence
    // advice hints (`(use "git pull" ...)`) — wt-status passes `!commit_template`
    // as `show_divergence_advice` to format_tracking_info. The branch state lines
    // themselves still print.
    let head_initial =
        status_long_branch_lines(git_dir, format, ahead_behind, commit_preview, &mut sink)?;
    if head_initial {
        sink.blank();
        if commit_preview {
            sink.line("Initial commit");
        } else {
            sink.line("No commits yet");
        }
    }

    let mut staged = Vec::<(&str, String)>::new();
    let mut unstaged = Vec::<(&str, String, String, bool)>::new();
    let mut untracked = Vec::new();
    let mut ignored = Vec::new();
    let unmerged = status_unmerged_paths(git_dir, format)?;
    let unmerged_paths: BTreeSet<Vec<u8>> =
        unmerged.iter().map(|entry| entry.path.clone()).collect();
    let entries = match worktree_root_for_git_dir(git_dir) {
        Ok(worktree_root) => {
            status_entries_with_renames(&worktree_root, git_dir, format, entries, rename_config)?
        }
        Err(_) => entries
            .into_iter()
            .map(|entry| StatusOutputEntry {
                entry,
                rename_from: None,
            })
            .collect(),
    };
    for output in entries {
        let entry = output.entry;
        if unmerged_paths.contains(&entry.path) {
            continue;
        }
        if entry.index == b'?' && entry.worktree == b'?' {
            untracked.push(entry.path);
            continue;
        }
        if entry.index == b'!' && entry.worktree == b'!' {
            ignored.push(entry.path);
            continue;
        }
        if let Some(label) = status_long_change_label(entry.index) {
            staged.push((
                label,
                status_long_path_display(&entry.path, output.rename_from.as_deref()),
            ));
        }
        if let Some(label) = status_long_change_label(entry.worktree) {
            // Submodule change detail (wt-status.c): " (new commits, modified
            // content, untracked content)" — whichever apply, in that order.
            let mut extras = Vec::new();
            if let Some(submodule) = entry.submodule {
                if submodule.new_commits {
                    extras.push("new commits");
                }
                if submodule.modified_content {
                    extras.push("modified content");
                }
                if submodule.untracked_content {
                    extras.push("untracked content");
                }
            }
            let suffix = if extras.is_empty() {
                String::new()
            } else {
                format!(" ({})", extras.join(", "))
            };
            // The "(commit or discard ...)" hint keys on dirty *content* only,
            // not on new commits (wt_status_check_worktree_changes).
            let dirty_submodule = entry
                .submodule
                .is_some_and(|sub| sub.modified_content || sub.untracked_content);
            unstaged.push((
                label,
                status_long_path_display(&entry.path, output.rename_from.as_deref()),
                suffix,
                dirty_submodule,
            ));
        }
    }

    let has_staged = !staged.is_empty();
    let has_unstaged = !unstaged.is_empty();
    let has_untracked = !untracked.is_empty();
    let has_ignored = !ignored.is_empty();
    let has_unmerged = !unmerged.is_empty();

    if status_long_operation_lines(
        git_dir,
        format,
        has_unmerged,
        has_staged,
        has_unstaged,
        &mut sink,
    )? || status_long_bisect_lines(git_dir, format, false, &mut sink)?
    {
        sink.blank();
    }

    if has_staged {
        if head_initial {
            sink.blank();
        }
        sink.line("Changes to be committed:");
        if status_suppress_staged_unstage_hint(git_dir) {
        } else if head_initial {
            sink.hint("  (use \"git rm --cached <file>...\" to unstage)");
        } else {
            sink.hint("  (use \"git restore --staged <file>...\" to unstage)");
        }
        for (label, path) in staged {
            sink.line(format!("\t{label:<12}{path}"));
        }
    }

    if has_unstaged {
        if head_initial || has_staged {
            sink.blank();
        }
        sink.line("Changes not staged for commit:");
        if unstaged.iter().any(|(label, _, _, _)| *label == "deleted:") {
            sink.hint("  (use \"git add/rm <file>...\" to update what will be committed)");
        } else {
            sink.hint("  (use \"git add <file>...\" to update what will be committed)");
        }
        sink.hint("  (use \"git restore <file>...\" to discard changes in working directory)");
        if unstaged.iter().any(|(_, _, _, dirty)| *dirty) {
            sink.hint("  (commit or discard the untracked or modified content in submodules)");
        }
        for (label, path, suffix, _) in unstaged {
            sink.line(format!("\t{label:<12}{path}{suffix}"));
        }
    }

    if has_unmerged {
        if head_initial || has_staged || has_unstaged {
            sink.blank();
        }
        sink.line("Unmerged paths:");
        if status_unmerged_needs_unstage_hint(git_dir)
            && unmerged.iter().any(|entry| entry.has_index_side())
        {
            sink.hint("  (use \"git restore --staged <file>...\" to unstage)");
        }
        // Mark-resolution hint depends on the mix of conflict kinds present
        // (wt_longstatus_print_unmerged_header): stagemask 1 = both deleted,
        // 3/5 = delete/modify, anything else = a "not-deleted" conflict.
        let mut both_deleted = false;
        let mut del_mod_conflict = false;
        let mut not_deleted = false;
        for entry in &unmerged {
            match entry.stagemask() {
                0 => {}
                1 => both_deleted = true,
                3 | 5 => del_mod_conflict = true,
                _ => not_deleted = true,
            }
        }
        if !both_deleted {
            if !del_mod_conflict {
                sink.hint("  (use \"git add <file>...\" to mark resolution)");
            } else {
                sink.hint("  (use \"git add/rm <file>...\" as appropriate to mark resolution)");
            }
        } else if !del_mod_conflict && !not_deleted {
            sink.hint("  (use \"git rm <file>...\" to mark resolution)");
        } else {
            sink.hint("  (use \"git add/rm <file>...\" as appropriate to mark resolution)");
        }
        for entry in unmerged {
            sink.line(format!(
                "\t{:<17}{}",
                entry.label,
                status_quote_path(&entry.path, false)
            ));
        }
    }

    // `Submodule changes to be committed:` then `Submodules changed but not
    // updated:` (wt-status.c calls both summaries right after print_changed).
    // Each non-empty section is separated from what precedes it by one blank
    // line; the trailing blank before "Untracked files" is supplied by that
    // section's own leading-blank logic (see `has_summary` below).
    let mut printed_anything = head_initial || has_staged || has_unstaged || has_unmerged;
    for section in [&submodule_summary.staged, &submodule_summary.unstaged] {
        if section.is_empty() {
            continue;
        }
        if printed_anything {
            sink.blank();
        }
        for line in section {
            sink.line(line.clone());
        }
        printed_anything = true;
    }
    let has_summary =
        !submodule_summary.staged.is_empty() || !submodule_summary.unstaged.is_empty();

    if has_untracked {
        if head_initial || has_staged || has_unstaged || has_unmerged || has_summary {
            sink.blank();
        }
        sink.line("Untracked files:");
        sink.hint("  (use \"git add <file>...\" to include in what will be committed)");
        if column_untracked {
            for line in status_column_lines(&untracked) {
                sink.line(format!("\t{line}"));
            }
        } else {
            for path in untracked {
                sink.line(format!("\t{}", status_quote_path(&path, false)));
            }
        }
    }

    if has_ignored {
        if head_initial
            || has_staged
            || has_unstaged
            || has_unmerged
            || has_summary
            || has_untracked
        {
            sink.blank();
        }
        sink.line("Ignored files:");
        sink.hint("  (use \"git add -f <file>...\" to include in what will be committed)");
        for path in ignored {
            sink.line(format!("\t{}", status_quote_path(&path, false)));
        }
    }

    // "Untracked files not listed" appears when untracked output is suppressed
    // (-uno / status.showUntrackedFiles=no) AND there is something to commit
    // (upstream gates this on `s->committable`, i.e. staged changes present).
    // It takes the place of the untracked section, so it gets the same leading
    // blank separator that section would have, and there is no trailing blank.
    // The "(use -u option ...)" suffix is itself a hint, gated separately.
    let printed_not_listed = untracked_suppressed && has_staged;
    if printed_not_listed {
        if head_initial || has_staged || has_unstaged || has_unmerged || has_summary {
            sink.blank();
        }
        if *hints {
            sink.line("Untracked files not listed (use -u option to show untracked files)");
        } else {
            sink.line("Untracked files not listed");
        }
    }

    if !has_staged && !has_unstaged && !has_unmerged && !has_untracked && !has_ignored {
        status_slow_untracked_advice(git_dir, &mut sink);
        if head_initial {
            sink.blank();
            sink.line("nothing to commit (create/copy files and use \"git add\" to track)");
        } else if untracked_suppressed {
            sink.line("nothing to commit (use -u to show untracked files)");
        } else {
            sink.line("nothing to commit, working tree clean");
        }
    } else if !has_staged && (has_unstaged || has_unmerged) {
        sink.blank();
        if *hints {
            sink.line("no changes added to commit (use \"git add\" and/or \"git commit -a\")");
        } else {
            sink.line("no changes added to commit");
        }
    } else if !has_staged && has_untracked {
        sink.blank();
        if *hints {
            sink.line(
                "nothing added to commit but untracked files present (use \"git add\" to track)",
            );
        } else {
            sink.line("nothing added to commit but untracked files present");
        }
    } else if !printed_not_listed {
        // A real untracked section (or staged-only) ends with a trailing blank;
        // the "not listed" line already supplied the trailing content.
        sink.blank();
    }
    if show_stash {
        let stash_count = status_stash_count(git_dir, format)?;
        if stash_count == 1 {
            sink.line("Your stash currently has 1 entry");
        } else if stash_count > 1 {
            sink.line(format!("Your stash currently has {stash_count} entries"));
        }
    }
    if let Some(footer) = sparse_footer {
        match footer {
            StatusSparseFooter::SparseIndex => {
                sink.line("You are in a sparse checkout.");
            }
            StatusSparseFooter::Percentage(percent) => {
                sink.line(format!(
                    "You are in a sparse checkout with {percent}% of tracked files present."
                ));
            }
        }
        sink.blank();
    }
    Ok(sink)
}

fn status_sparse_footer(
    git_dir: &Path,
    format: ObjectFormat,
) -> Result<Option<StatusSparseFooter>> {
    let sparse_enabled = GitConfig::read(git_dir.join("config.worktree"))
        .ok()
        .and_then(|config| config.get_bool("core", None, "sparseCheckout"))
        == Some(true)
        || read_repo_config(git_dir)
            .ok()
            .and_then(|config| config.get_bool("core", None, "sparseCheckout"))
            == Some(true);
    if !sparse_enabled {
        return Ok(None);
    }
    let Some(mut index) = sley_worktree::read_repository_index(git_dir, format)? else {
        return Ok(None);
    };
    if index.is_sparse()
        || index
            .entries
            .iter()
            .any(|entry| entry.mode == sley_index::SPARSE_DIR_MODE && entry.is_skip_worktree())
    {
        if !status_sparse_index_has_materialized_sparse_dir(git_dir, &index)? {
            return Ok(Some(StatusSparseFooter::SparseIndex));
        }
        let db = FileObjectDatabase::from_git_dir(git_dir, format);
        sley_worktree::expand_sparse_index(&mut index, &db, format)?;
    }
    let total = index
        .entries
        .iter()
        .filter(|entry| entry.stage() == sley_index::Stage::Normal)
        .count();
    if total == 0 {
        return Ok(None);
    }
    let present = index
        .entries
        .iter()
        .filter(|entry| entry.stage() == sley_index::Stage::Normal && !entry.is_skip_worktree())
        .count();
    let percent = ((present * 100) / total).min(100) as u8;
    Ok(Some(StatusSparseFooter::Percentage(percent)))
}

fn status_sparse_index_has_materialized_sparse_dir(git_dir: &Path, index: &Index) -> Result<bool> {
    let worktree_root = worktree_root_for_git_dir(git_dir)?;
    for entry in index
        .entries
        .iter()
        .filter(|entry| entry.mode == sley_index::SPARSE_DIR_MODE && entry.is_skip_worktree())
    {
        let path_bytes = entry.path.as_bytes();
        let path_bytes = path_bytes.strip_suffix(b"/").unwrap_or(path_bytes);
        let path = String::from_utf8_lossy(path_bytes);
        if !path.is_empty() && worktree_root.join(path.as_ref()).exists() {
            return Ok(true);
        }
    }
    Ok(false)
}

fn status_column_lines(paths: &[Vec<u8>]) -> Vec<String> {
    if paths.is_empty() {
        return Vec::new();
    }
    let rendered: Vec<String> = paths
        .iter()
        .map(|path| status_quote_path(path, false))
        .collect();
    let available = env::var("COLUMNS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(80)
        .saturating_sub(8)
        .max(1);
    let mut best_cols = 1;
    for cols in (1..=rendered.len()).rev() {
        if status_column_width(&rendered, cols) <= available {
            best_cols = cols;
            break;
        }
    }
    let rows = rendered.len().div_ceil(best_cols);
    let mut widths = vec![0usize; best_cols];
    for (idx, value) in rendered.iter().enumerate() {
        let col = idx / rows;
        widths[col] = widths[col].max(value.len());
    }
    let mut lines = Vec::new();
    for row in 0..rows {
        let mut line = String::new();
        for col in 0..best_cols {
            let idx = col * rows + row;
            let Some(value) = rendered.get(idx) else {
                continue;
            };
            if col > 0 {
                let previous = col - 1;
                let previous_idx = previous * rows + row;
                if let Some(previous_value) = rendered.get(previous_idx) {
                    let padding = widths[previous].saturating_sub(previous_value.len()) + 1;
                    line.extend(std::iter::repeat_n(' ', padding));
                }
            }
            line.push_str(value);
        }
        lines.push(line);
    }
    lines
}

fn status_column_width(values: &[String], cols: usize) -> usize {
    let rows = values.len().div_ceil(cols);
    let mut total = 0usize;
    let mut used = 0usize;
    for col in 0..cols {
        let start = col * rows;
        if start >= values.len() {
            continue;
        }
        let end = ((col + 1) * rows).min(values.len());
        let width = values[start..end]
            .iter()
            .map(|value| value.len())
            .max()
            .unwrap_or(0);
        if used > 0 {
            total += 1;
        }
        total += width;
        used += 1;
    }
    total
}

fn status_slow_untracked_advice(git_dir: &Path, sink: &mut StatusLineSink) {
    if env::var_os("GIT_TEST_UF_DELAY_WARNING").is_none() {
        return;
    }
    sink.blank();
    let config = read_repo_config(git_dir).unwrap_or_default();
    if config.get_bool("core", None, "untrackedCache") == Some(true)
        && config.get_bool("core", None, "fsmonitor") == Some(true)
    {
        sink.line("It took 3.25 seconds to enumerate untracked files,");
        sink.line("but the results were cached, and subsequent runs may be faster.");
    } else {
        sink.line("It took 3.25 seconds to enumerate untracked files.");
    }
    sink.line("See 'git help status' for information on how to improve this.");
    sink.blank();
}

fn status_suppress_staged_unstage_hint(git_dir: &Path) -> bool {
    git_dir.join("MERGE_HEAD").is_file() || git_dir.join("CHERRY_PICK_HEAD").is_file()
}

fn status_unmerged_needs_unstage_hint(git_dir: &Path) -> bool {
    git_dir.join("REVERT_HEAD").is_file()
        || git_dir.join("rebase-apply").is_dir()
        || git_dir.join("rebase-merge").is_dir()
}

fn status_stash_count(git_dir: &Path, format: ObjectFormat) -> Result<usize> {
    let store = FileRefStore::new(git_dir, format);
    Ok(store.read_reflog("refs/stash")?.len())
}

fn status_long_branch_lines(
    git_dir: &Path,
    format: ObjectFormat,
    ahead_behind: bool,
    suppress_divergence_advice: bool,
    sink: &mut StatusLineSink,
) -> Result<bool> {
    let store = FileRefStore::new(git_dir, format);
    match store.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(target)) => {
            if let Some(branch) = target.strip_prefix("refs/heads/") {
                sink.line(format!("On branch {branch}"));
                if let Some(RefTarget::Direct(oid)) = store.read_ref(&target)? {
                    status_long_tracking_lines(
                        git_dir,
                        format,
                        &store,
                        &target,
                        &oid,
                        ahead_behind,
                        suppress_divergence_advice,
                        sink,
                    )?;
                    Ok(false)
                } else {
                    Ok(true)
                }
            } else {
                sink.line(format!("On branch {target}"));
                Ok(store.read_ref(&target)?.is_none())
            }
        }
        Some(RefTarget::Direct(oid)) => {
            if let Some(rebase) = status_rebase_state(git_dir, format)? {
                let onto = rebase
                    .onto
                    .clone()
                    .unwrap_or_else(|| format_log_abbrev_oid(&oid));
                if rebase.interactive {
                    sink.line(format!("interactive rebase in progress; onto {onto}"));
                } else {
                    sink.line(format!("rebase in progress; onto {onto}"));
                }
            } else if git_dir.join("BISECT_LOG").is_file() {
                sink.line(format!("HEAD detached at {}", format_log_abbrev_oid(&oid)));
            } else if let Some(tag) = status_detached_at_tag(git_dir, format, &oid)? {
                sink.line(format!("HEAD detached at {tag}"));
            } else if let Some(tag) = status_detached_from_tag(git_dir, format, &oid)? {
                sink.line(format!("HEAD detached from {tag}"));
            } else {
                sink.line(format!("HEAD detached at {}", format_log_abbrev_oid(&oid)));
            }
            Ok(false)
        }
        None => {
            sink.line("On branch (unknown)");
            Ok(true)
        }
    }
}

fn status_detached_at_tag(
    git_dir: &Path,
    format: ObjectFormat,
    oid: &ObjectId,
) -> Result<Option<String>> {
    for (name, target) in status_loose_tag_oids(git_dir, format)? {
        if target == *oid {
            return Ok(Some(name));
        }
    }
    Ok(None)
}

fn status_detached_from_tag(
    git_dir: &Path,
    format: ObjectFormat,
    oid: &ObjectId,
) -> Result<Option<String>> {
    let reflog = fs::read_to_string(git_dir.join("logs").join("HEAD")).unwrap_or_default();
    for line in reflog.lines().rev() {
        let Some((_, message)) = line.split_once('\t') else {
            continue;
        };
        let Some(tag) = message
            .strip_prefix("checkout: moving from ")
            .and_then(|text| {
                text.rsplit_once(" to ")
                    .map(|(_, target)| target.trim())
                    .filter(|target| !target.is_empty())
            })
        else {
            continue;
        };
        for (name, target) in status_loose_tag_oids(git_dir, format)? {
            if name == tag && target != *oid {
                return Ok(Some(name));
            }
        }
    }
    Ok(None)
}

fn status_loose_tag_oids(git_dir: &Path, format: ObjectFormat) -> Result<Vec<(String, ObjectId)>> {
    let tags_dir = git_dir.join("refs").join("tags");
    let mut tags = Vec::new();
    status_collect_loose_tag_oids(format, &tags_dir, "", &mut tags)?;
    tags.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(tags)
}

fn status_collect_loose_tag_oids(
    format: ObjectFormat,
    dir: &Path,
    prefix: &str,
    tags: &mut Vec<(String, ObjectId)>,
) -> Result<()> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Ok(());
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let full_name = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        let path = entry.path();
        if path.is_dir() {
            status_collect_loose_tag_oids(format, &path, &full_name, tags)?;
        } else if path.is_file()
            && let Ok(text) = fs::read_to_string(&path)
            && let Ok(oid) = ObjectId::from_hex(format, text.trim())
        {
            tags.push((full_name, oid));
        }
    }
    Ok(())
}

pub(crate) fn status_long_tracking_lines(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    branch_ref: &str,
    oid: &ObjectId,
    ahead_behind: bool,
    suppress_divergence_advice: bool,
    sink: &mut StatusLineSink,
) -> Result<()> {
    let config = read_repo_config(git_dir)?;
    let mut seen = HashSet::new();
    if let Some(compare_branches) = config.get("status", None, "compareBranches") {
        for spec in compare_branches.split_whitespace() {
            let Some((refname, advice_mode)) = status_compare_branch_ref(&config, branch_ref, spec)
            else {
                continue;
            };
            if !seen.insert(refname.clone()) {
                continue;
            }
            let tracking = status_branch_tracking_for_ref(
                store,
                git_dir,
                format,
                oid,
                &refname,
                ahead_behind,
            )?;
            status_long_tracking_state_lines(
                &tracking,
                suppress_divergence_advice,
                advice_mode,
                sink,
            );
            sink.blank();
        }
    } else if let Some(tracking) = status_branch_tracking(
        git_dir,
        format,
        store,
        &config,
        branch_ref,
        oid,
        ahead_behind,
    )? {
        status_long_tracking_state_lines(
            &tracking,
            suppress_divergence_advice,
            StatusTrackingAdvice::Default,
            sink,
        );
        sink.blank();
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum StatusTrackingAdvice {
    Default,
    UpstreamCompare,
    PushCompare,
}

fn status_compare_branch_ref(
    config: &GitConfig,
    branch_ref: &str,
    spec: &str,
) -> Option<(String, StatusTrackingAdvice)> {
    if spec.eq_ignore_ascii_case("@{upstream}") {
        return for_each_ref_upstream(config, branch_ref)
            .map(|upstream| (upstream.refname, StatusTrackingAdvice::UpstreamCompare));
    }
    if spec.eq_ignore_ascii_case("@{push}") {
        return for_each_ref_push(config, branch_ref)
            .and_then(|push| push.refname)
            .map(|refname| (refname, StatusTrackingAdvice::PushCompare));
    }
    None
}

fn status_branch_tracking_for_ref(
    store: &FileRefStore,
    git_dir: &Path,
    format: ObjectFormat,
    oid: &ObjectId,
    refname: &str,
    ahead_behind: bool,
) -> Result<StatusBranchTracking> {
    let db = FileObjectDatabase::new(repository_objects_dir(git_dir), format);
    let state = if ahead_behind {
        match store.read_ref(refname)? {
            None => StatusBranchTrackingState::Gone,
            Some(_) => for_each_ref_upstream_track(store, git_dir, &db, format, oid, refname)?
                .map(StatusBranchTrackingState::Counts)
                .unwrap_or(StatusBranchTrackingState::Different),
        }
    } else {
        status_branch_tracking_without_ahead_behind(store, oid, refname)?
    };
    Ok(StatusBranchTracking {
        upstream: for_each_ref_short_name(refname).to_string(),
        state,
    })
}

fn status_long_tracking_state_lines(
    tracking: &StatusBranchTracking,
    suppress_divergence_advice: bool,
    advice_mode: StatusTrackingAdvice,
    sink: &mut StatusLineSink,
) {
    // git's format_tracking_info gates the ahead/behind/diverged *advice* hints on
    // `show_divergence_advice` (false for commit-template previews); the state
    // lines always print. Route the advice hints through this so a dry-run drops
    // only the `(use "git pull" ...)` style guidance.
    let advice = |sink: &mut StatusLineSink, text: &str| {
        if !suppress_divergence_advice {
            sink.hint(text);
        }
    };
    match tracking.state {
        StatusBranchTrackingState::Counts(ForEachRefTrack {
            ahead: 0,
            behind: 0,
            ..
        }) => {
            sink.line(format!(
                "Your branch is up to date with '{}'.",
                tracking.upstream
            ));
        }
        StatusBranchTrackingState::Counts(ForEachRefTrack {
            ahead, behind: 0, ..
        }) => {
            sink.line(format!(
                "Your branch is ahead of '{}' by {ahead} {}.",
                tracking.upstream,
                status_commit_word(ahead)
            ));
            if matches!(
                advice_mode,
                StatusTrackingAdvice::Default | StatusTrackingAdvice::PushCompare
            ) {
                advice(sink, "  (use \"git push\" to publish your local commits)");
            }
        }
        StatusBranchTrackingState::Counts(ForEachRefTrack {
            ahead: 0, behind, ..
        }) => {
            sink.line(format!(
                "Your branch is behind '{}' by {behind} {}, and can be fast-forwarded.",
                tracking.upstream,
                status_commit_word(behind)
            ));
            if !matches!(advice_mode, StatusTrackingAdvice::PushCompare) {
                advice(sink, "  (use \"git pull\" to update your local branch)");
            }
        }
        StatusBranchTrackingState::Counts(ForEachRefTrack { ahead, behind, .. }) => {
            sink.line(format!(
                "Your branch and '{}' have diverged,",
                tracking.upstream
            ));
            sink.line(format!(
                "and have {ahead} and {behind} different commits each, respectively."
            ));
            if !matches!(advice_mode, StatusTrackingAdvice::PushCompare) {
                advice(
                    sink,
                    "  (use \"git pull\" if you want to integrate the remote branch with yours)",
                );
            }
        }
        StatusBranchTrackingState::Different => {
            sink.line(format!(
                "Your branch and '{}' refer to different commits.",
                tracking.upstream
            ));
            if matches!(
                advice_mode,
                StatusTrackingAdvice::Default | StatusTrackingAdvice::PushCompare
            ) {
                advice(sink, "  (use \"git status --ahead-behind\" for details)");
            }
        }
        StatusBranchTrackingState::Gone => {
            sink.line(format!(
                "Your branch is based on '{}', but the upstream is gone.",
                tracking.upstream
            ));
            advice(sink, "  (use \"git branch --unset-upstream\" to fixup)");
        }
    }
}

fn status_commit_word(count: usize) -> &'static str {
    if count == 1 { "commit" } else { "commits" }
}

fn status_long_path_display(path: &[u8], rename_from: Option<&[u8]>) -> String {
    match rename_from {
        Some(rename_from) => format!(
            "{} -> {}",
            status_quote_path(rename_from, false),
            status_quote_path(path, false)
        ),
        None => status_quote_path(path, false),
    }
}

fn status_long_change_label(code: u8) -> Option<&'static str> {
    match code {
        b'A' => Some("new file:"),
        b'M' => Some("modified:"),
        b'T' => Some("typechange:"),
        b'D' => Some("deleted:"),
        b'R' => Some("renamed:"),
        b'C' => Some("copied:"),
        _ => None,
    }
}

/// A short-status code pair identifies an unmerged (conflicted) path. Upstream's
/// HEAD-vs-index diff records these as `DIFF_STATUS_UNMERGED` and they do NOT set
/// `s->committable` — so `commit --dry-run` must not treat the `D`/`A` half of a
/// conflict pair as a real staged change.
fn status_short_is_unmerged(index: u8, worktree: u8) -> bool {
    matches!(
        (index, worktree),
        (b'D', b'D')
            | (b'A', b'U')
            | (b'U', b'D')
            | (b'U', b'A')
            | (b'D', b'U')
            | (b'A', b'A')
            | (b'U', b'U')
    )
}

pub(crate) fn status_entries_have_index_changes(
    entries: &[sley_worktree::ShortStatusEntry],
) -> bool {
    entries.iter().any(|entry| {
        !status_short_is_unmerged(entry.index, entry.worktree)
            && status_long_change_label(entry.index).is_some()
    })
}

fn status_porcelain_v2_code(code: u8) -> char {
    if code == b' ' { '.' } else { code as char }
}

/// Map an unmerged stagemask to the porcelain v2 `u`-record key
/// (wt_porcelain_v2_print_unmerged_entry).
fn status_porcelain_v2_unmerged_key(stagemask: u8) -> &'static str {
    match stagemask {
        1 => "DD",
        2 => "AU",
        3 => "UD",
        4 => "UA",
        5 => "DU",
        6 => "AA",
        _ => "UU",
    }
}

fn status_porcelain_v2_branch_headers(
    git_dir: &Path,
    format: ObjectFormat,
    config: &GitConfig,
    ahead_behind: bool,
) -> Result<Vec<String>> {
    let store = FileRefStore::new(git_dir, format);
    match store.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(target)) => {
            let target_oid = match store.read_ref(&target)? {
                Some(RefTarget::Direct(oid)) => Some(oid),
                _ => None,
            };
            let oid = match target_oid.as_ref() {
                Some(oid) => oid.to_hex(),
                _ => "(initial)".into(),
            };
            let head = target
                .strip_prefix("refs/heads/")
                .unwrap_or(target.as_str())
                .to_string();
            let mut headers = vec![
                format!("# branch.oid {oid}"),
                format!("# branch.head {head}"),
            ];
            if let Some(oid) = target_oid.as_ref()
                && let Some(tracking) = status_branch_tracking(
                    git_dir,
                    format,
                    &store,
                    config,
                    &target,
                    oid,
                    ahead_behind,
                )?
            {
                headers.push(format!("# branch.upstream {}", tracking.upstream));
                match tracking.state {
                    StatusBranchTrackingState::Counts(track) => {
                        headers.push(format!("# branch.ab +{} -{}", track.ahead, track.behind));
                    }
                    StatusBranchTrackingState::Different => {
                        headers.push("# branch.ab +? -?".into());
                    }
                    StatusBranchTrackingState::Gone => {}
                }
            }
            Ok(headers)
        }
        Some(RefTarget::Direct(oid)) => Ok(vec![
            format!("# branch.oid {}", oid.to_hex()),
            "# branch.head (detached)".into(),
        ]),
        None => Ok(vec![
            "# branch.oid (initial)".into(),
            "# branch.head (unknown)".into(),
        ]),
    }
}

fn status_branch_header(
    git_dir: &Path,
    format: ObjectFormat,
    config: &GitConfig,
    ahead_behind: bool,
) -> Result<String> {
    let store = FileRefStore::new(git_dir, format);
    match store.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(target)) => {
            if let Some(branch) = target.strip_prefix("refs/heads/") {
                if let Some(RefTarget::Direct(oid)) = store.read_ref(&target)? {
                    let mut header = format!("## {branch}");
                    if let Some(tracking) = status_branch_tracking(
                        git_dir,
                        format,
                        &store,
                        config,
                        &target,
                        &oid,
                        ahead_behind,
                    )? {
                        header.push_str("...");
                        header.push_str(&tracking.upstream);
                        if let StatusBranchTrackingState::Counts(track) = tracking.state {
                            if track.ahead > 0 || track.behind > 0 {
                                header.push(' ');
                                let mut suffix = Vec::new();
                                write_for_each_ref_track(&mut suffix, track, true)?;
                                header.push_str(&String::from_utf8_lossy(&suffix));
                            }
                        } else if matches!(tracking.state, StatusBranchTrackingState::Gone) {
                            header.push_str(" [gone]");
                        } else {
                            header.push_str(" [different]");
                        }
                    }
                    Ok(header)
                } else {
                    Ok(format!("## No commits yet on {branch}"))
                }
            } else {
                Ok(format!("## {target}"))
            }
        }
        Some(RefTarget::Direct(_)) | None => Ok("## HEAD (no branch)".into()),
    }
}

struct StatusBranchTracking {
    upstream: String,
    state: StatusBranchTrackingState,
}

#[derive(Clone, Copy)]
enum StatusBranchTrackingState {
    Counts(ForEachRefTrack),
    Different,
    Gone,
}

fn status_branch_tracking(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    config: &GitConfig,
    branch_ref: &str,
    oid: &ObjectId,
    ahead_behind: bool,
) -> Result<Option<StatusBranchTracking>> {
    let Some(upstream) = for_each_ref_upstream(config, branch_ref) else {
        return Ok(None);
    };
    let gone_track = ForEachRefTrack {
        ahead: 0,
        behind: 0,
        gone: true,
    };
    let track = if ahead_behind {
        match store.read_ref(&upstream.refname)? {
            None => StatusBranchTrackingState::Gone,
            Some(upstream_target) => {
                let upstream_ref = sley_refs::Ref {
                    name: upstream.refname.clone(),
                    target: upstream_target,
                };
                let Some((upstream_oid, _)) = resolve_for_each_ref_target(store, &upstream_ref)?
                else {
                    return Ok(Some(StatusBranchTracking {
                        upstream: for_each_ref_short_name(&upstream.refname).to_string(),
                        state: StatusBranchTrackingState::Counts(gone_track),
                    }));
                };
                if oid == &upstream_oid {
                    Some(ForEachRefTrack {
                        ahead: 0,
                        behind: 0,
                        gone: false,
                    })
                } else {
                    let db = FileObjectDatabase::new(repository_objects_dir(git_dir), format);
                    for_each_ref_ahead_behind(git_dir, &db, format, oid, &upstream_oid)?
                }
            }
            .map(StatusBranchTrackingState::Counts)
            .unwrap_or(StatusBranchTrackingState::Different),
        }
    } else {
        status_branch_tracking_without_ahead_behind(store, oid, &upstream.refname)?
    };
    Ok(Some(StatusBranchTracking {
        upstream: for_each_ref_short_name(&upstream.refname).to_string(),
        state: track,
    }))
}

fn status_branch_tracking_without_ahead_behind(
    store: &FileRefStore,
    oid: &ObjectId,
    upstream: &str,
) -> Result<StatusBranchTrackingState> {
    let Some(RefTarget::Direct(upstream_oid)) = store.read_ref(upstream)? else {
        return Ok(StatusBranchTrackingState::Gone);
    };
    if oid == &upstream_oid {
        Ok(StatusBranchTrackingState::Counts(ForEachRefTrack {
            ahead: 0,
            behind: 0,
            gone: false,
        }))
    } else {
        Ok(StatusBranchTrackingState::Different)
    }
}
