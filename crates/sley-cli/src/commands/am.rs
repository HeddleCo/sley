//! `git am` — apply a series of patches from a mailbox (porcelain shell).
//!
//! The series engine lives in [`sley_sequencer::am`] (mbox intake, mailinfo,
//! patch application with the 3-way fallback, index/worktree transitions,
//! commit formation, session state under `.git/rebase-apply/`, and the
//! abort/skip/continue/retry transitions). This module owns argv parsing,
//! usage text, exit codes, and the host services the engine cannot
//! (`applypatch-msg` / `pre-applypatch` / `post-applypatch` / `post-rewrite`
//! hook execution, partial-clone hydration, autostash stash primitives, and
//! the rerere seams), plus `--show-current-patch` rendering.
//!
//! Series state layout matches real git (`next`, `last`, `0001`..`NNNN`,
//! `author-script`, `info`, `final-commit`, `msg`, `patch`, `abort-safety`, …)
//! so `--abort`, `--continue`/`--resolved`, and `--skip` can resume an
//! interrupted run.
#![allow(clippy::expect_used, clippy::unwrap_used)]
use crate::*;
use sley_sequencer::am as sam;

/// Parsed command-line configuration for a fresh `git am` invocation.
/// Construction is argv-porcelain (this module); consumption is the engine.
pub(crate) type AmOptions = sam::AmOptions;

// ---------------------------------------------------------------------------
// Option parsing (argv porcelain)
// ---------------------------------------------------------------------------

/// Parse the non-resume flags of `git am`.
fn setup_am_options(args: &[String]) -> Result<AmOptions> {
    use sam::{AmEmptyAction, AmPatchFormat};
    let mut options = AmOptions {
        mboxes: Vec::new(),
        quiet: false,
        signoff: false,
        three_way: false,
        keep_non_patch: false,
        empty_action: AmEmptyAction::Stop,
        keep_subject: false,
        keep_non_patch_brackets: false,
        message_id: false,
        committer_date_is_author_date: false,
        ignore_date: false,
        no_verify: false,
        keep_cr: false,
        ignore_whitespace: false,
        scissors: false,
        utf8: true,
        interactive: false,
        rerere_autoupdate: None,
        directory: None,
        patch_format: AmPatchFormat::Auto,
        p_value: 1,
        git_apply_opts: Vec::new(),
    };
    let mut positional_only = false;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if positional_only {
            options.mboxes.push(arg.clone());
            index += 1;
            continue;
        }
        match arg.as_str() {
            "--" => positional_only = true,
            "-q" | "--quiet" => options.quiet = true,
            "--no-quiet" => options.quiet = false,
            "-s" | "--signoff" => options.signoff = true,
            "--no-signoff" => options.signoff = false,
            "-3" | "--3way" => options.three_way = true,
            "--no-3way" => options.three_way = false,
            "-i" | "--interactive" => options.interactive = true,
            "--no-interactive" => options.interactive = false,
            "--ignore-whitespace" | "--ignore-space-change" => {
                options.ignore_whitespace = true;
                options.git_apply_opts.push(arg.clone());
            }
            "--no-ignore-whitespace" => options.ignore_whitespace = false,
            "-k" | "--keep" => {
                options.keep_non_patch = true;
                options.keep_subject = true;
            }
            "-b" | "--keep-non-patch" => {
                options.keep_non_patch = true;
                options.keep_non_patch_brackets = true;
            }
            "-m" | "--message-id" => options.message_id = true,
            "--no-message-id" => options.message_id = false,
            "--committer-date-is-author-date" => options.committer_date_is_author_date = true,
            "--no-committer-date-is-author-date" => options.committer_date_is_author_date = false,
            "--ignore-date" => options.ignore_date = true,
            "--no-ignore-date" => options.ignore_date = false,
            "-n" | "--no-verify" => options.no_verify = true,
            "--verify" => options.no_verify = false,
            "--keep-cr" => options.keep_cr = true,
            "--no-keep-cr" => options.keep_cr = false,
            "--patch-format" => {
                let value = args.get(index + 1).map(String::as_str).unwrap_or("");
                options.patch_format = sam::parse_am_patch_format(value)?;
                index += 1;
            }
            value if let Some(format) = value.strip_prefix("--patch-format=") => {
                options.patch_format = sam::parse_am_patch_format(format)?;
            }
            "--empty" => {
                let value = args.get(index + 1).map(String::as_str).unwrap_or("");
                eprintln!("error: invalid value for '--empty': '{value}'");
                return Err(GitError::Exit(129));
            }
            "--empty=drop" => options.empty_action = AmEmptyAction::Drop,
            "--empty=keep" => options.empty_action = AmEmptyAction::Keep,
            "--empty=stop" => options.empty_action = AmEmptyAction::Stop,
            // Accepted no-ops: these affect mail parsing / cosmetics we already
            // handle or that do not change the resulting commits for the inputs
            // `git format-patch` produces.
            "-c" | "--scissors" => options.scissors = true,
            "--no-scissors" => options.scissors = false,
            "-u" | "--utf8" => options.utf8 = true,
            "--no-utf8" => options.utf8 = false,
            "--rerere-autoupdate" => options.rerere_autoupdate = Some(true),
            "--no-rerere-autoupdate" => options.rerere_autoupdate = Some(false),
            "--allow-empty" => {}
            // Forwarded `git apply` options: collected into git_apply_opts in git's
            // recreate-opt form (`--whitespace=fix`, `-C1`, `-p2`, `--reject`, …),
            // persisted to apply-opt, and re-applied for every patch + on resume.
            "--whitespace" => {
                let value = args.get(index + 1).map(String::as_str).unwrap_or("");
                options.git_apply_opts.push(format!("--whitespace={value}"));
                index += 1;
            }
            value if let Some(action) = value.strip_prefix("--whitespace=") => {
                options
                    .git_apply_opts
                    .push(format!("--whitespace={action}"));
            }
            "--reject" => options.git_apply_opts.push("--reject".to_string()),
            "--no-reject" => options.git_apply_opts.push("--no-reject".to_string()),
            value if let Some(invalid) = value.strip_prefix("--empty=") => {
                eprintln!("error: invalid value for '--empty': '{invalid}'");
                return Err(GitError::Exit(129));
            }
            value if let Some(rest) = value.strip_prefix("--exclude=") => {
                options.git_apply_opts.push(format!("--exclude={rest}"));
            }
            value if let Some(rest) = value.strip_prefix("--include=") => {
                options.git_apply_opts.push(format!("--include={rest}"));
            }
            "-C" => {
                let value = args.get(index + 1).map(String::as_str).unwrap_or("");
                options.git_apply_opts.push(format!("-C{value}"));
                index += 1;
            }
            value if let Some(rest) = value.strip_prefix("-C") => {
                options.git_apply_opts.push(format!("-C{rest}"));
            }
            "-p" => {
                let value = args.get(index + 1).map(String::as_str).unwrap_or("");
                options.p_value = value.parse::<usize>().unwrap_or(1);
                options.git_apply_opts.push(format!("-p{value}"));
                index += 1;
            }
            value if let Some(rest) = value.strip_prefix("-p") => {
                options.p_value = rest.parse::<usize>().unwrap_or(1);
                options.git_apply_opts.push(format!("-p{rest}"));
            }
            "--directory" => {
                let value = args.get(index + 1).map(String::as_str).unwrap_or("");
                options.directory = Some(value.to_string());
                options.git_apply_opts.push(format!("--directory={value}"));
                index += 1;
            }
            value if let Some(dir) = value.strip_prefix("--directory=") => {
                options.directory = Some(dir.to_string());
                options.git_apply_opts.push(format!("--directory={dir}"));
            }
            value if value.starts_with('-') && value != "-" => {
                eprintln!("error: unknown option `{}'", value.trim_start_matches('-'));
                am_usage();
                return Err(GitError::Exit(129));
            }
            value => options.mboxes.push(value.to_string()),
        }
        index += 1;
    }
    Ok(options)
}

/// Apply `am.messageid` / `am.threeWay` config defaults, but only for a flag the
/// command line did not explicitly set (an explicit `--[no-]message-id` /
/// `--[no-]3way` wins over config, matching git's parse order: config first,
/// then the command-line override).
fn apply_am_config_defaults(config: &GitConfig, args: &[String], options: &mut AmOptions) {
    let has = |needles: &[&str]| args.iter().any(|a| needles.contains(&a.as_str()));
    if !has(&["-m", "--message-id", "--no-message-id"])
        && let Some(value) = am_config_bool(config, "messageid")
    {
        options.message_id = value;
    }
    if !has(&["-3", "--3way", "--no-3way"])
        && let Some(value) = am_config_bool(config, "threeWay")
    {
        options.three_way = value;
    }
}

/// Read a boolean `am.<key>` value from the effective config (repo + global +
/// `-c`/env overrides), returning `None` when unset or unparsable.
fn am_config_bool(config: &GitConfig, key: &str) -> Option<bool> {
    let value = config.get("am", None, key)?.to_string();
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" | "" => Some(true),
        "false" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

fn am_usage() {
    eprintln!("usage: git am [--signoff] [--keep] [-q | --quiet] [-3 | --3way] [<mbox>...]");
    eprintln!("   or: git am (--continue | --skip | --abort | --quit)");
}

fn am_incompatible_resume_error(existing: &str, new: &str) -> Result<()> {
    eprintln!("fatal: options '{existing}' and '{new}' cannot be used together");
    Err(GitError::Exit(128))
}

// ---------------------------------------------------------------------------
// --show-current-patch rendering
// ---------------------------------------------------------------------------

/// Which artifact `git am --show-current-patch` dumps to stdout.
#[derive(Clone, Copy, PartialEq)]
enum ShowPatchMode {
    /// `--show-current-patch` (default) / `=raw`: the raw mbox message
    /// (`.git/rebase-apply/NNNN`).
    Raw,
    /// `--show-current-patch=diff`: the extracted diff (`.git/rebase-apply/patch`).
    Diff,
}

/// Record a `--show-current-patch` command-mode like git's `OPT_CMDMODE`:
/// repeating the *same* mode is accepted; selecting a second *different* mode is
/// an error (matching git's "... is incompatible with ..." command-mode check).
fn set_show_patch_mode(slot: &mut Option<ShowPatchMode>, mode: ShowPatchMode) -> Result<()> {
    match slot {
        Some(existing) if *existing != mode => {
            eprintln!(
                "error: --show-current-patch={} is incompatible with --show-current-patch={}",
                show_patch_arg(mode),
                show_patch_arg(*existing),
            );
            Err(GitError::Exit(129))
        }
        _ => {
            *slot = Some(mode);
            Ok(())
        }
    }
}

fn show_patch_arg(mode: ShowPatchMode) -> &'static str {
    match mode {
        ShowPatchMode::Raw => "raw",
        ShowPatchMode::Diff => "diff",
    }
}

/// Implement `git am --show-current-patch[=raw|=diff]`: dump the current paused
/// patch to stdout. `raw` prints the raw mbox message for the current patch
/// number (`.git/rebase-apply/NNNN`); `diff` prints the extracted diff
/// (`.git/rebase-apply/patch`). With no resolve in progress git fails.
fn am_show_current_patch(state_dir: &Path, mode: ShowPatchMode) -> Result<()> {
    if !state_dir.exists() {
        eprintln!("fatal: Resolve operation not in progress, we are not resuming.");
        return Err(GitError::Exit(128));
    }
    let path = match mode {
        ShowPatchMode::Raw => {
            // The current patch number is recorded in `next` (1-based), stored
            // as the zero-padded `NNNN` filename git uses (e.g. `0001`). A
            // missing/garbled `next` falls back to the first patch.
            let next = sam::read_state_usize(state_dir, "next").unwrap_or(1);
            state_dir.join(format!("{next:04}"))
        }
        ShowPatchMode::Diff => state_dir.join("patch"),
    };
    let data = fs::read(&path).map_err(|err| {
        eprintln!("fatal: failed to read '{}': {err}", path.display());
        GitError::Exit(128)
    })?;
    io::stdout().write_all(&data)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Resume option overrides
// ---------------------------------------------------------------------------

/// Parse the option overrides a resume verb (`--retry`/`--continue`) may carry.
/// Only options that change saved session state are tracked; others are ignored
/// (the resume path does not run the full `setup_am_options`).
fn parse_am_resume_overrides(option_args: &[String]) -> sam::AmResumeOverrides {
    let mut overrides = sam::AmResumeOverrides::default();
    for arg in option_args {
        match arg.as_str() {
            "-3" | "--3way" => overrides.three_way = Some(true),
            "--no-3way" => overrides.three_way = Some(false),
            "-q" | "--quiet" => overrides.quiet = Some(true),
            "--no-quiet" => overrides.quiet = Some(false),
            "-s" | "--signoff" => overrides.signoff = Some(true),
            "--no-signoff" => overrides.signoff = Some(false),
            "--reject" => overrides.reject = Some(true),
            "--no-reject" => overrides.reject = Some(false),
            _ => {}
        }
    }
    overrides
}

// ---------------------------------------------------------------------------
// Engine hand-off: repository context + host services
// ---------------------------------------------------------------------------

struct AmPrefetch;

impl sley_sequencer::apply::PromisorObjectFetch for AmPrefetch {
    fn read_object_maybe_prefetch(
        &self,
        db: &FileObjectDatabase,
        oid: &ObjectId,
    ) -> Result<std::sync::Arc<sley_object::EncodedObject>> {
        crate::read_object_maybe_prefetch_promisor(db, oid, true)
    }
}

fn am_prefetch() -> Option<&'static dyn sley_sequencer::apply::PromisorObjectFetch> {
    static PREFETCH: AmPrefetch = AmPrefetch;
    Some(&PREFETCH)
}

/// Build the engine's repository view. The paths/config come from the
/// already-opened session.
pub(crate) fn am_engine_context(
    git_dir: &Path,
    common_git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
    config: &GitConfig,
    lazy_fetch: bool,
) -> sam::AmContext<'static> {
    sam::AmContext::new(
        git_dir.to_path_buf(),
        common_git_dir.to_path_buf(),
        worktree_root.to_path_buf(),
        format,
        config.clone(),
        if lazy_fetch { am_prefetch() } else { None },
    )
}

/// Host services for the am engine: hook execution, autostash stash
/// primitives, and rerere seams. Each closure is a verbatim relocation of the
/// call site it used to serve in this module.
pub(crate) fn am_engine_hosts(
    git_dir: &Path,
    common_git_dir: &Path,
    worktree_root: &Path,
    lazy_fetch: bool,
) -> sam::AmHosts<'static> {
    let git_dir = git_dir.to_path_buf();
    let common_git_dir = common_git_dir.to_path_buf();
    let worktree_root = worktree_root.to_path_buf();
    let hook_git_dir = git_dir.clone();
    let stash_common = common_git_dir.clone();
    let stash_worktree = worktree_root.clone();
    let rerere_git_dir = git_dir.clone();
    let rerere_worktree = worktree_root.clone();
    let record_format = repository_object_format(&common_git_dir).unwrap_or(ObjectFormat::Sha1);
    let record_git_dir = git_dir.clone();
    let record_worktree = worktree_root;
    let clear_git_dir = git_dir;
    sam::AmHosts {
        run_hook: Box::new(move |name: &str, args, stdin| {
            commands::hooks::run_hook_at(
                &hook_git_dir,
                name,
                commands::hooks::HookRun {
                    args,
                    stdin,
                    ..commands::hooks::HookRun::default()
                },
            )?;
            Ok(())
        }),
        stash_apply_quietly: Box::new(move |oid| {
            commands::stash::apply_stash_commit_quietly_at(
                &stash_common,
                &stash_worktree,
                oid,
                lazy_fetch,
            )
        }),
        stash_store: Box::new(move |oid, message| {
            commands::stash::store_stash_commit_at(&common_git_dir, oid, message)
        }),
        rerere_now: Box::new(move |autoupdate| {
            commands::rerere::repo_rerere(
                &rerere_git_dir,
                &rerere_worktree,
                repository_object_format(&rerere_git_dir).unwrap_or(ObjectFormat::Sha1),
                autoupdate,
            )
        }),
        rerere_record_resolved: Box::new(move || {
            commands::rerere::record_resolved_after_commit(
                &record_git_dir,
                &record_worktree,
                record_format,
            )
        }),
        rerere_clear: Box::new(move || commands::rerere::rerere_clear(&clear_git_dir)),
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Entry point for `git am`.
pub(crate) fn cmd_am(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
    let repository = cli_session.open_repository()?;
    let git_dir = repository.git_dir().to_path_buf();
    let common_git_dir = repository.common_dir().to_path_buf();
    let format = repository.object_format();
    let config = read_repo_config(&git_dir)?;
    let worktree_root = worktree_root_for_git_dir(cli_session, &git_dir)?;
    let state_dir = git_dir.join("rebase-apply");

    // Resume sub-operations are mutually exclusive and take no mbox arguments.
    // `--show-current-patch[=raw|=diff]` is a "command mode" like git's
    // OPT_CMDMODE: setting two *different* modes is an error, but repeating the
    // *same* mode is accepted (t4150 "accepts repeated --show-current-patch").
    let mut resume = None;
    let mut show_patch: Option<ShowPatchMode> = None;
    let mut allow_empty_resume = false;
    let mut option_args = Vec::new();
    for arg in args {
        match arg.as_str() {
            "--abort" | "--quit" | "--continue" | "-r" | "--resolved" | "--skip" | "--retry" => {
                if let Some(existing) = resume {
                    return am_incompatible_resume_error(existing, arg);
                }
                resume = Some(match arg.as_str() {
                    "-r" | "--resolved" => "--continue",
                    other => other,
                });
            }
            "--show-current-patch" => set_show_patch_mode(&mut show_patch, ShowPatchMode::Raw)?,
            "--show-current-patch=raw" => set_show_patch_mode(&mut show_patch, ShowPatchMode::Raw)?,
            "--show-current-patch=diff" => {
                set_show_patch_mode(&mut show_patch, ShowPatchMode::Diff)?
            }
            value if value.starts_with("--show-current-patch=") => {
                let arg = &value["--show-current-patch=".len()..];
                eprintln!("error: invalid value for '--show-current-patch': '{arg}'");
                return Err(GitError::Exit(129));
            }
            "--allow-empty" => {
                allow_empty_resume = true;
                option_args.push(arg.to_string());
            }
            other => option_args.push(other.to_string()),
        }
    }

    if let Some(mode) = show_patch {
        return am_show_current_patch(&state_dir, mode);
    }

    let ctx = am_engine_context(
        &git_dir,
        &common_git_dir,
        &worktree_root,
        format,
        &config,
        cli_session.lazy_fetch(),
    );
    let hosts = am_engine_hosts(
        &git_dir,
        &common_git_dir,
        &worktree_root,
        cli_session.lazy_fetch(),
    );

    if let Some(resume) = resume {
        // Command-line options given alongside a resume verb override the saved
        // session options for the resumed patch (git's `am_run` resume; t4153).
        let overrides = parse_am_resume_overrides(&option_args);
        // `-i`/`--interactive` is a per-invocation flag (git never persists it);
        // record the current value so the resumed driver / am_resolve see it.
        if state_dir.exists() {
            let interactive = option_args
                .iter()
                .any(|arg| arg == "-i" || arg == "--interactive");
            let flag: &[u8] = if interactive { b"t\n" } else { b"f\n" };
            let _ = fs::write(state_dir.join("interactive"), flag);
        }
        return match resume {
            "--abort" => sam::am_abort(&ctx, &hosts, &state_dir),
            "--quit" => sam::am_quit(&ctx, &hosts, &state_dir),
            "--skip" => sam::am_skip(&ctx, &hosts, &state_dir),
            "--continue" => sam::am_continue(&ctx, &hosts, &state_dir, overrides),
            "--retry" => sam::am_retry(&ctx, &hosts, &state_dir, overrides),
            _ => Ok(()),
        };
    }

    let mut options = setup_am_options(&option_args)?;

    // git seeds am.messageid / am.threeWay from config, then lets the
    // command-line flag (handled in setup_am_options) override. setup_am_options
    // leaves an unspecified flag at false, so OR the config default in only when
    // the user did not pass an explicit `--[no-]…` form.
    apply_am_config_defaults(&config, &option_args, &mut options);

    sam::start_am(&ctx, &hosts, &options, allow_empty_resume)
}
