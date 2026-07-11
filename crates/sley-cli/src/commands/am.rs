//! `git am` — apply a series of patches from a mailbox.
//!
//! This implements the common subset of `git am`: it reads an mbox (one or more
//! files, or stdin), parses each message's From/Subject/Date headers plus body
//! and unified diff, applies the diff to the worktree and index, and creates one
//! commit per patch that preserves the original author identity, author date,
//! and commit message. The committer is taken from the environment/config the
//! same way `git commit` does, so applying patches produced by `git format-patch`
//! reproduces the original commit object IDs byte-for-byte.
//!
//! Series state is persisted under `.git/rebase-apply/` using the same file
//! layout real git uses (`next`, `last`, `0001`..`NNNN`, `author-script`,
//! `info`, `final-commit`, `msg`, `patch`, `abort-safety`, …) so `--abort`,
//! `--continue`/`--resolved`, and `--skip` can resume an interrupted run.
//!
//! Command modules pull their shared plumbing from the crate root; the glob
//! import reaches every helper, type, and re-export visible there (a submodule
//! can access its ancestor module's private items), so `cli_git_dir`,
//! `repository_object_format`, `FileObjectDatabase`, `three_way_merge_trees`,
//! and friends are all in scope without re-listing them.
#![allow(clippy::expect_used, clippy::unwrap_used)]
use crate::*;
use sley::plumbing::{sley_index, sley_worktree};

/// Parsed command-line configuration for a fresh `git am` invocation.
struct AmOptions {
    /// mbox files to read; empty means read stdin.
    mboxes: Vec<String>,
    /// Suppress the per-patch `Applying:` line (`-q`/`--quiet`).
    quiet: bool,
    /// Append a `Signed-off-by` trailer to each commit (`-s`/`--signoff`).
    signoff: bool,
    /// Fall back to a 3-way merge when straight application fails (`-3`).
    three_way: bool,
    /// Keep non-empty commits whose patch is empty rather than erroring
    /// (`-k`/`--keep` / `--keep-non-patch`).
    keep_non_patch: bool,
    /// What to do with a mail message that has no patch (`--empty=<action>`).
    empty_action: AmEmptyAction,
    /// Keep the subject line verbatim, skipping all mailinfo cleanup
    /// (`-k`/`--keep`).
    keep_subject: bool,
    /// Strip `[PATCH]`-style brackets but keep other `[…]` brackets in the
    /// subject (`-b`/`--keep-non-patch`).
    keep_non_patch_brackets: bool,
    /// Append the patch's `Message-ID:` header to each commit message
    /// (`-m`/`--message-id`, or `am.messageid`).
    message_id: bool,
    /// Use each patch's author date as the committer date too
    /// (`--committer-date-is-author-date`).
    committer_date_is_author_date: bool,
    /// Ignore the patch's `Date:` header, using the current time for the author
    /// (and, with `--committer-date-is-author-date`, the committer) date
    /// (`--ignore-date`).
    ignore_date: bool,
    /// Skip the `applypatch-msg` and `pre-applypatch` hooks (`-n`/`--no-verify`).
    no_verify: bool,
    /// Keep CR at the end of mail lines instead of stripping it (`--keep-cr`).
    /// Default (false / `--no-keep-cr`) strips the CR a CRLF transport added.
    keep_cr: bool,
    /// Match patch context/deleted lines ignoring whitespace differences
    /// (`--ignore-whitespace`). Used by the rebase apply backend.
    ignore_whitespace: bool,
    /// Cut message text at a scissors line before mailinfo header/body parsing.
    scissors: bool,
    /// Recode mail content to the target commit encoding (default; disabled by
    /// `--no-utf8`).
    utf8: bool,
    /// Prompt before applying each patch (`-i`/`--interactive`).
    interactive: bool,
    /// `--rerere-autoupdate` / `--no-rerere-autoupdate`, persisted for resume.
    rerere_autoupdate: Option<bool>,
    /// Prepend this directory to every path in each patch (`--directory=<dir>`,
    /// forwarded to `git apply --directory`).
    directory: Option<String>,
    /// Input patch container format (`--patch-format=<format>`), or auto-detect.
    patch_format: AmPatchFormat,
    /// `-p<n>`: number of leading path components to strip from patch names.
    /// Default 1; `--no-prefix` patches need `-p0`.
    p_value: usize,
    /// The `git apply` options forwarded verbatim, in git's recreate-opt form
    /// (`-C1`, `-p2`, `--whitespace=fix`, `--directory=<dir>`, `--reject`,
    /// `--ignore-whitespace`, `--exclude=<path>`, `--include=<path>`). git's
    /// `OPT_PASSTHRU_ARGV` collects these into `state->git_apply_opts`, sq-quotes
    /// them into `rebase-apply/apply-opt`, and re-passes them to every `git apply`
    /// across the series and on resume (t4252). We persist + replay them the same
    /// way, applying them in-process.
    git_apply_opts: Vec<String>,
}

impl AmOptions {
    fn subject_cleanup(&self) -> SubjectCleanup {
        SubjectCleanup {
            keep_subject: self.keep_subject,
            keep_non_patch_brackets: self.keep_non_patch_brackets,
            scissors: self.scissors,
        }
    }
}

/// `git am --empty=<action>` handling for messages without a patch.
#[derive(Clone, Copy, PartialEq)]
enum AmEmptyAction {
    Stop,
    Drop,
    Keep,
}

/// Supported `git am --patch-format=<format>` modes.
#[derive(Clone, Copy, PartialEq)]
enum AmPatchFormat {
    Auto,
    Mbox,
    Stgit,
    StgitSeries,
    Hg,
    Mboxrd,
}

/// How a patch's `Subject:` header should be cleaned, mirroring git's mailinfo
/// `keep_subject` (`-k`) and `keep_non_patch_brackets_in_subject` (`-b`).
#[derive(Clone, Copy, Default)]
struct SubjectCleanup {
    /// `-k`/`--keep`: keep the subject verbatim, no cleanup at all.
    keep_subject: bool,
    /// `-b`/`--keep-non-patch`: strip `[PATCH]` brackets but keep other `[…]`.
    keep_non_patch_brackets: bool,
    /// `--scissors`: discard everything before the scissors cut line.
    scissors: bool,
}

/// The subset of `AmOptions` that affects how each commit object is built. Read
/// back from the state directory for each patch so `--continue` reproduces the
/// same commits an uninterrupted run would have.
#[derive(Clone, Copy)]
struct AmCommitOpts {
    signoff: bool,
    message_id: bool,
    committer_date_is_author_date: bool,
    ignore_date: bool,
    /// Skip the `applypatch-msg` and `pre-applypatch` hooks (`-n`/`--no-verify`).
    no_verify: bool,
    utf8: bool,
    /// When set, the per-commit reflog uses the rebase apply-backend format
    /// `<action> (pick): <subject>` (git runs am under the rebase backend with
    /// `GIT_REFLOG_ACTION="<action> (pick)"`, builtin/rebase.c run_am), instead
    /// of the bare `am: <subject>` a standalone `git am` writes.
    rebase_pick_reflog: bool,
}

fn write_am_rerere_autoupdate(state_dir: &Path, value: Option<bool>) -> Result<()> {
    match value {
        Some(true) => fs::write(state_dir.join("rerere-autoupdate"), bool_flag(true))?,
        Some(false) => fs::write(state_dir.join("rerere-autoupdate"), bool_flag(false))?,
        None => {}
    }
    Ok(())
}

fn read_am_rerere_autoupdate(state_dir: &Path) -> Option<bool> {
    match fs::read_to_string(state_dir.join("rerere-autoupdate")) {
        Ok(line) if line.trim() == "t" => return Some(true),
        Ok(line) if line.trim() == "f" => return Some(false),
        _ => {}
    }
    match fs::read_to_string(state_dir.join("allow_rerere_autoupdate")) {
        Ok(line) if line.contains("--no-rerere-autoupdate") => Some(false),
        Ok(line) if line.contains("--rerere-autoupdate") => Some(true),
        _ => None,
    }
}

/// A single message extracted from an mbox: identity, message, and raw diff.
struct AmPatch {
    /// Author name from the `From:` header.
    author_name: Vec<u8>,
    /// Author email from the `From:` header.
    author_email: Vec<u8>,
    /// Charset of `author_name` / `author_email`.
    author_encoding: String,
    /// Author date from the `Date:` header, already normalised to
    /// `"<seconds> <±HHMM>"`. `None` when the header was absent or unparsable
    /// (the committer/env date is then used).
    author_date: Option<String>,
    /// Original `Date:` header text, preserved verbatim for the author-script.
    author_date_raw: Option<String>,
    /// Cleaned subject line (with any `[PATCH …]` prefix stripped).
    subject: String,
    /// Full commit message (subject + blank line + body), newline-terminated.
    message: Vec<u8>,
    /// Charset declared by the mail message for the commit message body.
    message_encoding: String,
    /// The raw `Message-ID:` header value (including the surrounding angle
    /// brackets, e.g. `<...@example.com>`), if the message carried one. Appended
    /// to the commit message when `--message-id`/`am.messageid` is set.
    message_id: Option<String>,
    /// The unified diff body (everything from the first `diff`/`---` onward).
    diff: Vec<u8>,
}

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
            let _ = fs::write(state_dir.join("interactive"), bool_flag(interactive));
        }
        return match resume {
            "--abort" => am_abort(&git_dir, &worktree_root, format, &state_dir, &config),
            "--quit" => am_quit(
                &git_dir,
                &common_git_dir,
                &worktree_root,
                format,
                &state_dir,
                &config,
                cli_session.lazy_fetch(),
            ),
            "--skip" => am_skip(
                &git_dir,
                &common_git_dir,
                &worktree_root,
                format,
                &state_dir,
                &config,
                cli_session.lazy_fetch(),
            ),
            "--continue" => am_continue(
                &git_dir,
                &common_git_dir,
                &worktree_root,
                format,
                &state_dir,
                overrides,
                &config,
                cli_session.lazy_fetch(),
            ),
            "--retry" => am_retry(
                &git_dir,
                &common_git_dir,
                &worktree_root,
                format,
                &state_dir,
                overrides,
                &config,
                cli_session.lazy_fetch(),
            ),
            _ => Ok(()),
        };
    }

    let mut options = setup_am_options(&option_args)?;

    if allow_empty_resume && options.mboxes.is_empty() && state_dir.exists() {
        return am_continue_allow_empty(
            &git_dir,
            &common_git_dir,
            &worktree_root,
            format,
            &state_dir,
            &config,
            cli_session.lazy_fetch(),
        );
    }

    // git seeds am.messageid / am.threeWay from config, then lets the
    // command-line flag (handled in setup_am_options) override. setup_am_options
    // leaves an unspecified flag at false, so OR the config default in only when
    // the user did not pass an explicit `--[no-]…` form.
    apply_am_config_defaults(&config, &option_args, &mut options);

    // Starting a new run while one is unfinished is an error in git.
    if state_dir.exists() {
        eprintln!(
            "fatal: previous rebase directory {} still exists but mbox given.",
            display_state_dir(&worktree_root, &state_dir)
        );
        return Err(GitError::Exit(128));
    }

    let mut input_files = read_am_input_files(&options.mboxes)?;
    // git's mailsplit strips a trailing CR from each line by default; only
    // `--keep-cr` keeps it. Normalising CRLF -> LF here lets a CRLF mail apply
    // and commit byte-identically to its LF original.
    if !options.keep_cr {
        for file in &mut input_files {
            *file = strip_cr(file);
        }
    }
    let combined: Vec<u8> = input_files.concat();

    let patch_format = detect_am_patch_format(&options.mboxes, &combined, options.patch_format)?;

    // git treats explicit files and stdin differently. A file must pass
    // patch-format detection. Stdin is assumed to be mbox, so empty stdin is just
    // a silent no-op.
    let from_files = !options.mboxes.is_empty();
    if from_files && patch_format == AmPatchFormat::Auto {
        eprintln!("Patch format detection failed.");
        return Err(GitError::Exit(128));
    }

    let patches = parse_am_patches(&options, patch_format, &input_files, &combined)?;
    // No messages at all (empty/whitespace stdin) — nothing to do.
    if patches.is_empty() {
        return Ok(());
    }

    let refs = repository.references();
    let head_oid = head_commit_oid(&refs)?;
    let state_head_oid = head_oid.unwrap_or_else(|| ObjectId::null(format));

    write_am_state_dir(&state_dir, &patches, &options, &state_head_oid)?;

    // git refuses to start applying onto a dirty index (the index differs from
    // HEAD). The state dir already exists at this point — cell 4 checks it
    // survives — and git records `dirtyindex` before dying.
    if let Some(head_oid) = head_oid
        && am_index_is_dirty(&git_dir, &common_git_dir, format, &head_oid)?
    {
        fs::write(state_dir.join("dirtyindex"), b"t\n")?;
        eprintln!("Dirty index: cannot apply patches (dirty: )");
        return Err(GitError::Exit(128));
    }

    // Record ORIG_HEAD (git's am_setup) so `am --abort` knows where to rewind.
    // An unborn HEAD gets none — abort then drops the branch instead.
    if let Some(head_oid) = head_oid {
        fs::write(state_dir.join("orig-head"), format!("{head_oid}\n"))?;
    }

    run_am_series(
        &git_dir,
        &common_git_dir,
        &worktree_root,
        format,
        &state_dir,
        1,
        AmResumeOverrides::default(),
        &config,
        cli_session.lazy_fetch(),
    )
}

// ===========================================================================
// Rebase apply backend (the `git rebase --apply` / `git-rebase--am` path)
// ===========================================================================
//
// `git rebase --apply` (and the implicit am path triggered by `--ignore-whitespace`
// / `-C` / `--whitespace`) generates a `format-patch` series for
// `upstream..orig_head` and feeds it to `git am`. We do the same here without a
// mbox round-trip: the caller in `rebase.rs` hands us each pick commit's
// author/message + the unified diff, and we drive the existing am series engine.
// The state lives in `.git/rebase-apply/` (NOT `rebase-merge/`), with three extra
// files — `head-name`, `onto`, `orig-head` — that mark this as a rebase so
// `finish_am` returns to the original branch instead of just dropping state.

/// One commit to replay through the apply backend.
pub(crate) struct RebaseApplyCommit {
    pub author_name: Vec<u8>,
    pub author_email: Vec<u8>,
    /// Raw author date as `<seconds> <±HHMM>` (or any `Date:` form the patch
    /// would carry); `None` falls back to the env author date.
    pub author_date: Option<String>,
    /// Full commit message (subject + blank + body), newline-terminated.
    pub message: Vec<u8>,
    /// Unified diff of the commit against its first parent.
    pub diff: Vec<u8>,
    /// The original commit being replayed. Recorded so the apply backend can feed
    /// the `post-rewrite` hook the `<old> <new>` mapping (git's `state->orig_commit`,
    /// from each patch's `From <sha>` line). Drives the per-commit `rewritten` file.
    pub orig_commit: ObjectId,
}

/// Options for a fresh `git rebase --apply` run.
pub(crate) struct RebaseApplyParams {
    pub commits: Vec<RebaseApplyCommit>,
    pub quiet: bool,
    pub signoff: bool,
    pub committer_date_is_author_date: bool,
    pub ignore_date: bool,
    pub ignore_whitespace: bool,
    pub apply_opts: Vec<String>,
    /// `--rerere-autoupdate` / `--no-rerere-autoupdate`, persisted for resume.
    pub rerere_autoupdate: Option<bool>,
    /// `refs/heads/<branch>` to return to, or `None` for a detached HEAD rebase.
    pub head_name: Option<String>,
    /// The commit HEAD/orig branch started at (for `--abort`).
    pub orig_head: ObjectId,
    /// The new base (already checked out as detached HEAD by the caller).
    pub onto: ObjectId,
}

/// Convert a subject line out of a full commit message (first line).
fn subject_of_message(message: &[u8]) -> String {
    let end = message
        .iter()
        .position(|b| *b == b'\n')
        .unwrap_or(message.len());
    String::from_utf8_lossy(&message[..end]).into_owned()
}

/// Start a fresh `git rebase --apply` series. The caller has already detached
/// HEAD onto `onto`; here we write the apply state dir and drive the series.
pub(crate) fn start_rebase_apply(
    git_dir: &Path,
    common_git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
    params: RebaseApplyParams,
    config: &GitConfig,
    lazy_fetch: bool,
) -> Result<()> {
    let state_dir = git_dir.join("rebase-apply");
    let target_encoding = commit_encoding_config(git_dir);
    let patches: Vec<AmPatch> = params
        .commits
        .iter()
        .map(|commit| AmPatch {
            author_name: commit.author_name.clone(),
            author_email: commit.author_email.clone(),
            author_encoding: target_encoding.clone(),
            author_date: commit.author_date.clone(),
            author_date_raw: commit.author_date.clone(),
            subject: subject_of_message(&commit.message),
            message: commit.message.clone(),
            message_encoding: target_encoding.clone(),
            message_id: None,
            diff: commit.diff.clone(),
        })
        .collect();

    let options = AmOptions {
        mboxes: Vec::new(),
        quiet: params.quiet,
        signoff: params.signoff,
        // Upstream `git am --rebasing` forces three-way fallback so add/add
        // conflicts materialize in the index and rerere can replay them.
        three_way: true,
        keep_non_patch: false,
        empty_action: AmEmptyAction::Stop,
        keep_subject: false,
        keep_non_patch_brackets: false,
        message_id: false,
        committer_date_is_author_date: params.committer_date_is_author_date,
        ignore_date: params.ignore_date,
        no_verify: false,
        keep_cr: false,
        ignore_whitespace: params.ignore_whitespace,
        scissors: false,
        utf8: true,
        interactive: false,
        rerere_autoupdate: None,
        directory: None,
        patch_format: AmPatchFormat::Mbox,
        p_value: 1,
        git_apply_opts: params.apply_opts,
    };

    let refs = FileRefStore::new(git_dir, format);
    let head_oid = head_commit_oid(&refs)?
        .ok_or_else(|| GitError::Command("rebase --apply: cannot read HEAD".into()))?;

    write_am_state_dir(&state_dir, &patches, &options, &head_oid)?;
    // git records each patch's original commit (`state->orig_commit`, parsed from
    // the `From <sha>` line format-patch emits) and, in `do_commit`, appends
    // `<orig> <new>` to `rebase-apply/rewritten`. We don't round-trip a `From <sha>`
    // line through the RFC822 patch parser, so persist the per-patch original shas
    // in a parallel `orig-commits` file (line N = patch N's original) that the
    // series driver looks up by patch number when each commit lands.
    let orig_commits: String = params
        .commits
        .iter()
        .map(|commit| format!("{}\n", commit.orig_commit))
        .collect();
    fs::write(state_dir.join("orig-commits"), orig_commits)?;
    // The `rebasing` marker selects git's post-rewrite firing path (am.c keys the
    // whole `rewritten`/post-rewrite mechanism off `state->rebasing`).
    fs::write(state_dir.join("rebasing"), b"")?;
    // Rebase markers: their presence makes `finish_am` / `am --abort` behave as a
    // rebase (return to the original branch) rather than a bare `git am`.
    let head_name = params
        .head_name
        .clone()
        .unwrap_or_else(|| "detached HEAD".to_string());
    fs::write(state_dir.join("head-name"), format!("{head_name}\n"))?;
    fs::write(state_dir.join("onto"), format!("{}\n", params.onto))?;
    fs::write(
        state_dir.join("orig-head"),
        format!("{}\n", params.orig_head),
    )?;
    // The apply backend's `--abort`/finish restores `orig_head`, not the
    // post-detach HEAD, so overwrite abort-safety with orig_head.
    fs::write(
        state_dir.join("abort-safety"),
        format!("{}\n", params.orig_head),
    )?;
    fs::write(state_dir.join("quiet"), bool_flag(params.quiet))?;
    write_am_rerere_autoupdate(&state_dir, params.rerere_autoupdate)?;

    run_am_series(
        git_dir,
        common_git_dir,
        worktree_root,
        format,
        &state_dir,
        1,
        AmResumeOverrides::default(),
        config,
        lazy_fetch,
    )
}

/// Whether a `.git/rebase-apply/` state dir belongs to a `git rebase --apply`
/// (vs a bare `git am`): the rebase backend writes a `head-name` marker.
pub(crate) fn rebase_apply_in_progress(git_dir: &Path) -> bool {
    let state_dir = git_dir.join("rebase-apply");
    state_dir.is_dir() && state_dir.join("head-name").exists()
}

/// `git rebase --apply --continue`: resume the am series, then finish the rebase.
pub(crate) fn rebase_apply_continue(
    git_dir: &Path,
    common_git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
    config: &GitConfig,
    lazy_fetch: bool,
) -> Result<()> {
    let state_dir = git_dir.join("rebase-apply");
    am_continue(
        git_dir,
        common_git_dir,
        worktree_root,
        format,
        &state_dir,
        AmResumeOverrides::default(),
        config,
        lazy_fetch,
    )
}

/// `git rebase --apply --skip`.
pub(crate) fn rebase_apply_skip(
    git_dir: &Path,
    common_git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
    config: &GitConfig,
    lazy_fetch: bool,
) -> Result<()> {
    let state_dir = git_dir.join("rebase-apply");
    am_skip(
        git_dir,
        common_git_dir,
        worktree_root,
        format,
        &state_dir,
        config,
        lazy_fetch,
    )
}

/// `git rebase --apply --abort`: restore the original branch and drop state.
pub(crate) fn rebase_apply_abort(
    git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
    config: &GitConfig,
    _lazy_fetch: bool,
) -> Result<()> {
    let state_dir = git_dir.join("rebase-apply");
    am_abort(git_dir, worktree_root, format, &state_dir, config)
}

/// Parse the non-resume flags of `git am`.
fn setup_am_options(args: &[String]) -> Result<AmOptions> {
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
                options.patch_format = parse_am_patch_format(value)?;
                index += 1;
            }
            value if let Some(format) = value.strip_prefix("--patch-format=") => {
                options.patch_format = parse_am_patch_format(format)?;
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
            let next = read_state_usize(state_dir, "next").unwrap_or(1);
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

/// Strip a trailing CR from every CRLF in the buffer (git's default
/// `--no-keep-cr` mailsplit behaviour). Only `\r` immediately before a `\n` is
/// removed, so a lone `\r` mid-line (rare in mail) is preserved.
fn strip_cr(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut iter = input.iter().peekable();
    while let Some(&byte) = iter.next() {
        if byte == b'\r' && iter.peek() == Some(&&b'\n') {
            continue;
        }
        out.push(byte);
    }
    out
}

/// Read every mbox file (or stdin when none are given), keeping one buffer *per
/// file* rather than concatenating them. git's `mailsplit` splits each input
/// file independently, so a file that is a single bare patch (no mbox `From `
/// separator) becomes one message — concatenating first would merge several such
/// files into one garbled message (t4252 `am-test-*-?` globs pass two such
/// files). stdin is returned as a single one-element list.
fn read_am_input_files(mboxes: &[String]) -> Result<Vec<Vec<u8>>> {
    if mboxes.is_empty() {
        let mut input = Vec::new();
        io::stdin().read_to_end(&mut input)?;
        Ok(vec![input])
    } else {
        let mut files = Vec::with_capacity(mboxes.len());
        for mbox in mboxes {
            files.push(fs::read(mbox)?);
        }
        Ok(files)
    }
}

fn parse_am_patch_format(value: &str) -> Result<AmPatchFormat> {
    match value {
        "mbox" => Ok(AmPatchFormat::Mbox),
        "stgit" => Ok(AmPatchFormat::Stgit),
        "stgit-series" => Ok(AmPatchFormat::StgitSeries),
        "hg" => Ok(AmPatchFormat::Hg),
        "mboxrd" => Ok(AmPatchFormat::Mboxrd),
        other => {
            eprintln!("error: invalid value for '--patch-format': '{other}'");
            Err(GitError::Exit(129))
        }
    }
}

fn detect_am_patch_format(
    mboxes: &[String],
    input: &[u8],
    requested: AmPatchFormat,
) -> Result<AmPatchFormat> {
    if requested != AmPatchFormat::Auto {
        return Ok(requested);
    }
    if mboxes.is_empty() || mboxes.first().is_some_and(|path| path == "-") {
        return Ok(AmPatchFormat::Mbox);
    }
    if mboxes.first().is_some_and(|path| Path::new(path).is_dir()) {
        return Ok(AmPatchFormat::Mbox);
    }

    let lines: Vec<&[u8]> = input
        .split(|byte| *byte == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
        .collect();
    let Some(first_idx) = lines.iter().position(|line| !line.is_empty()) else {
        return Ok(AmPatchFormat::Auto);
    };
    let first = lines[first_idx];
    if first.starts_with(b"From ") || first.starts_with(b"From: ") || is_diff_start(first) {
        return Ok(AmPatchFormat::Mbox);
    }
    if first.starts_with(b"# This series applies on GIT commit") {
        return Ok(AmPatchFormat::StgitSeries);
    }
    if first == b"# HG changeset patch" {
        return Ok(AmPatchFormat::Hg);
    }
    let second = lines.get(first_idx + 1).copied().unwrap_or(b"");
    let third = lines.get(first_idx + 2).copied().unwrap_or(b"");
    if !first.is_empty()
        && second.is_empty()
        && (third.starts_with(b"From:")
            || third.starts_with(b"Author:")
            || third.starts_with(b"Date:"))
    {
        return Ok(AmPatchFormat::Stgit);
    }
    if looks_like_patch_input(input) {
        return Ok(AmPatchFormat::Mbox);
    }
    Ok(AmPatchFormat::Auto)
}

fn parse_am_patches(
    options: &AmOptions,
    format: AmPatchFormat,
    input_files: &[Vec<u8>],
    combined: &[u8],
) -> Result<Vec<AmPatch>> {
    match format {
        // git's mailsplit splits each file independently, then am parses every
        // resulting message. Parse each file's buffer on its own and concatenate
        // so two bare single-patch files (no `From ` separator) do not merge into
        // one garbled message (t4252); a single file with several `From `-
        // separated messages still splits correctly inside parse_mbox.
        AmPatchFormat::Mbox | AmPatchFormat::Auto => {
            let mut patches = Vec::new();
            for file in input_files {
                patches.extend(parse_mbox(file, options.subject_cleanup())?);
            }
            Ok(patches)
        }
        AmPatchFormat::Mboxrd => {
            let mut patches = Vec::new();
            for file in input_files {
                patches.extend(parse_mboxrd(file, options.subject_cleanup())?);
            }
            Ok(patches)
        }
        AmPatchFormat::Stgit => parse_foreign_patches(
            &options.mboxes,
            combined,
            options.subject_cleanup(),
            stgit_patch_to_mail,
        ),
        AmPatchFormat::Hg => parse_foreign_patches(
            &options.mboxes,
            combined,
            options.subject_cleanup(),
            hg_patch_to_mail,
        ),
        AmPatchFormat::StgitSeries => parse_stgit_series(options, combined),
    }
}

fn parse_foreign_patches(
    mboxes: &[String],
    stdin_input: &[u8],
    cleanup: SubjectCleanup,
    convert: fn(&[u8]) -> Vec<u8>,
) -> Result<Vec<AmPatch>> {
    if mboxes.is_empty() {
        let mail = convert(stdin_input);
        return Ok(vec![parse_message(&split_keep_newline(&mail), cleanup)?]);
    }
    let mut patches = Vec::new();
    for path in mboxes {
        let input = if path == "-" {
            stdin_input.to_vec()
        } else {
            fs::read(path)?
        };
        let mail = convert(&input);
        patches.push(parse_message(&split_keep_newline(&mail), cleanup)?);
    }
    Ok(patches)
}

fn parse_stgit_series(options: &AmOptions, input: &[u8]) -> Result<Vec<AmPatch>> {
    if options.mboxes.len() != 1 || options.mboxes[0] == "-" {
        eprintln!("Only one StGIT patch series can be applied at once");
        return Err(GitError::Exit(128));
    }
    let series_path = Path::new(&options.mboxes[0]);
    let series_dir = series_path.parent().unwrap_or_else(|| Path::new("."));
    let mut patches = Vec::new();
    for line in split_keep_newline(input) {
        let line = trim_trailing_newline(&line);
        if line.is_empty() || line[0] == b'#' {
            continue;
        }
        let name = String::from_utf8_lossy(line).into_owned();
        let patch_input = fs::read(series_dir.join(name))?;
        let mail = stgit_patch_to_mail(&patch_input);
        patches.push(parse_message(
            &split_keep_newline(&mail),
            options.subject_cleanup(),
        )?);
    }
    Ok(patches)
}

fn stgit_patch_to_mail(input: &[u8]) -> Vec<u8> {
    let lines = split_keep_newline(input);
    let mut out = Vec::new();
    let mut idx = 0;
    let mut subject_printed = false;
    while idx < lines.len() {
        let line = trim_trailing_newline(&lines[idx]);
        let text = String::from_utf8_lossy(line);
        if text.trim().is_empty() {
            idx += 1;
            continue;
        } else if let Some(rest) = text.strip_prefix("Author:") {
            out.extend_from_slice(format!("From:{rest}\n").as_bytes());
        } else if text.starts_with("From") || text.starts_with("Date") {
            out.extend_from_slice(line);
            out.push(b'\n');
        } else if !subject_printed {
            out.extend_from_slice(b"Subject: ");
            out.extend_from_slice(line);
            out.push(b'\n');
            subject_printed = true;
        } else {
            out.push(b'\n');
            out.extend_from_slice(line);
            out.push(b'\n');
            idx += 1;
            break;
        }
        idx += 1;
    }
    for line in &lines[idx..] {
        out.extend_from_slice(line);
    }
    out
}

fn hg_patch_to_mail(input: &[u8]) -> Vec<u8> {
    let lines = split_keep_newline(input);
    let mut out = Vec::new();
    let mut idx = 0;
    while idx < lines.len() {
        let line = trim_trailing_newline(&lines[idx]);
        let text = String::from_utf8_lossy(line);
        if let Some(rest) = text.strip_prefix("# User ") {
            out.extend_from_slice(format!("From: {rest}\n").as_bytes());
        } else if let Some(rest) = text.strip_prefix("# Date ") {
            if let Some(date) = parse_hg_date(rest) {
                out.extend_from_slice(format!("Date: {date}\n").as_bytes());
            }
        } else if text.starts_with("# ") {
            // Mercurial metadata/comment line.
        } else {
            out.push(b'\n');
            out.extend_from_slice(line);
            out.push(b'\n');
            idx += 1;
            break;
        }
        idx += 1;
    }
    for line in &lines[idx..] {
        out.extend_from_slice(line);
    }
    out
}

fn parse_hg_date(value: &str) -> Option<String> {
    let mut parts = value.split_whitespace();
    let seconds = parts.next()?;
    let tz_west: i64 = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    let minutes_east = -tz_west / 60;
    let sign = if minutes_east < 0 { '-' } else { '+' };
    let abs = minutes_east.abs();
    Some(format!("{seconds} {sign}{:02}{:02}", abs / 60, abs % 60))
}

fn unescape_mboxrd(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    for line in split_keep_newline(input) {
        let trimmed = trim_trailing_newline(&line);
        let mut gt = 0usize;
        while trimmed.get(gt) == Some(&b'>') {
            gt += 1;
        }
        if gt > 0 && trimmed[gt..].starts_with(b"From ") {
            out.extend_from_slice(&line[1..]);
        } else {
            out.extend_from_slice(&line);
        }
    }
    out
}

// ===========================================================================
// mbox parsing
// ===========================================================================

/// Heuristic patch-format detection for explicit mbox files, mirroring what git
/// does before splitting: the content must look like a mailbox (`From `), a mail
/// (a `Header: value` line such as `From:`/`Subject:`/`Date:`), or a diff
/// (`diff --git`, `--- `, `Index:`). Empty/whitespace-only content fails.
fn looks_like_patch_input(input: &[u8]) -> bool {
    for line in split_keep_newline(input) {
        let line = trim_trailing_newline(&line);
        // git's mailsplit treats leading all-whitespace lines as blank and skips
        // them before locating the first header (the t4150 "preceding
        // whitespace" patch leads with 255 spaces).
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        if line.starts_with(b"From ") || is_diff_start(line) {
            return true;
        }
        // A mail header line: a non-space token, then a colon (e.g. `Subject:`).
        if let Some(colon) = line.iter().position(|byte| *byte == b':')
            && colon > 0
            && line[..colon].iter().all(|byte| byte.is_ascii_graphic())
        {
            return true;
        }
        // First non-blank line is neither a header nor a diff: not a patch.
        break;
    }
    false
}

/// Split an mbox into individual messages and parse each into an [`AmPatch`].
///
/// Messages are delimited by lines beginning with `From ` (the mbox "From_"
/// separator that `git format-patch` emits as `From <sha> Mon Sep 17 …`). A
/// buffer with no separator at all is treated as a single message, matching
/// git's lenient behaviour for a lone patch. Whitespace-only input yields no
/// messages (the caller treats that as a no-op). A message that turns out to
/// carry no diff is still returned so the series driver can report the exact
/// "Patch is empty." behaviour git uses (including its hint block).
fn parse_mbox(input: &[u8], cleanup: SubjectCleanup) -> Result<Vec<AmPatch>> {
    if input.iter().all(|byte| byte.is_ascii_whitespace()) {
        return Ok(Vec::new());
    }
    let lines = split_keep_newline(input);
    // Identify message-start indices (mbox "From " separators).
    let mut starts = Vec::new();
    for (idx, line) in lines.iter().enumerate() {
        if line.starts_with(b"From ") {
            starts.push(idx);
        }
    }
    if starts.is_empty() {
        // No separator: the whole buffer is one message.
        return Ok(vec![parse_message(&lines, cleanup)?]);
    }
    let mut patches = Vec::new();
    for (position, &start) in starts.iter().enumerate() {
        let end = starts.get(position + 1).copied().unwrap_or(lines.len());
        // Skip the leading "From " separator line itself.
        let body = &lines[start + 1..end];
        let patch = parse_message(body, cleanup)?;
        if is_pine_internal_message(&patch) {
            continue;
        }
        patches.push(patch);
    }
    Ok(patches)
}

fn parse_mboxrd(input: &[u8], cleanup: SubjectCleanup) -> Result<Vec<AmPatch>> {
    if input.iter().all(|byte| byte.is_ascii_whitespace()) {
        return Ok(Vec::new());
    }
    let lines = split_keep_newline(input);
    let mut starts = Vec::new();
    for (idx, line) in lines.iter().enumerate() {
        if line.starts_with(b"From ") {
            starts.push(idx);
        }
    }
    if starts.is_empty() {
        let unescaped = unescape_mboxrd(input);
        return Ok(vec![parse_message(
            &split_keep_newline(&unescaped),
            cleanup,
        )?]);
    }
    let mut patches = Vec::new();
    for (position, &start) in starts.iter().enumerate() {
        let end = starts.get(position + 1).copied().unwrap_or(lines.len());
        let mut message = Vec::new();
        for line in &lines[start + 1..end] {
            message.extend_from_slice(line);
        }
        let unescaped = unescape_mboxrd(&message);
        let patch = parse_message(&split_keep_newline(&unescaped), cleanup)?;
        if is_pine_internal_message(&patch) {
            continue;
        }
        patches.push(patch);
    }
    Ok(patches)
}

fn is_pine_internal_message(patch: &AmPatch) -> bool {
    patch.diff.is_empty()
        && patch.subject == "DON'T DELETE THIS MESSAGE -- FOLDER INTERNAL DATA"
        && patch
            .message_id
            .as_deref()
            .is_some_and(|id| id.contains("foo-0001@example.com"))
}

/// Parse a single message (headers + blank line + body + diff).
fn parse_message(lines: &[Vec<u8>], cleanup: SubjectCleanup) -> Result<AmPatch> {
    let mut author_name = String::new();
    let mut author_email = String::new();
    let mut author_date = None;
    let mut author_date_raw = None;
    let mut subject = String::new();
    let mut message_id = None;
    let mut message_encoding = "UTF-8".to_string();

    // Skip any leading all-whitespace lines before the headers (git's mailinfo
    // ignores blank/whitespace lines preceding the first header; the t4150
    // "preceding whitespace" patch leads with a 255-space line).
    let mut idx = 0;
    while idx < lines.len() {
        let line = trim_trailing_newline(&lines[idx]);
        if line.iter().all(u8::is_ascii_whitespace) {
            idx += 1;
        } else {
            break;
        }
    }

    // Phase 1: RFC822-style headers, ending at the first blank line. Continuation
    // lines (leading whitespace) extend the previous header value.
    let mut last_header: Option<String> = None;
    let mut header_values: Vec<(String, String)> = Vec::new();
    while idx < lines.len() {
        let line = trim_trailing_newline(&lines[idx]);
        if line.is_empty() {
            idx += 1;
            break;
        }
        if (line[0] == b' ' || line[0] == b'\t') && last_header.is_some() {
            if let Some((_, value)) = header_values.last_mut() {
                value.push(' ');
                value.push_str(String::from_utf8_lossy(line).trim());
            }
            idx += 1;
            continue;
        }
        if let Some(colon) = line.iter().position(|byte| *byte == b':') {
            let name = String::from_utf8_lossy(&line[..colon])
                .trim()
                .to_lowercase();
            let value = String::from_utf8_lossy(&line[colon + 1..])
                .trim()
                .to_string();
            last_header = Some(name.clone());
            header_values.push((name, value));
        } else {
            // Not a header line — treat the rest as body (lenient).
            break;
        }
        idx += 1;
    }
    for (name, value) in &header_values {
        match name.as_str() {
            "from" => {
                let (name, email) = parse_from_header(value);
                author_name = name;
                author_email = email;
            }
            "date" => {
                author_date_raw = Some(value.clone());
                // RFC 2822 is the format `git format-patch` emits, but the rebase
                // apply backend stores the commit's raw git date (`<secs> <tz>` /
                // `@<secs> <tz>`) directly; accept that too so the round-trip
                // through the state dir preserves the author date.
                author_date = parse_rfc2822_date(value)
                    .or_else(|| parse_raw_git_date_normalized(value))
                    .or_else(|| parse_git_default_date(value));
            }
            "subject" => subject = clean_subject(value, cleanup),
            "message-id" if !value.is_empty() => message_id = Some(value.clone()),
            "content-type" => {
                if let Some(charset) = content_type_charset(value) {
                    message_encoding = charset;
                }
            }
            _ => {}
        }
    }

    if cleanup.scissors
        && let Some(cut) = lines[idx..]
            .iter()
            .position(|line| is_scissors_line(trim_trailing_newline(line)))
    {
        idx += cut + 1;
        subject.clear();
    }
    consume_in_body_headers(
        lines,
        &mut idx,
        cleanup,
        &mut author_name,
        &mut author_email,
        &mut author_date,
        &mut author_date_raw,
        &mut subject,
    );

    // Phase 2: the rest of the message is one of three regions, in order:
    //   1. the commit body — until a standalone `---` separator or the diff;
    //   2. an optional diffstat — between the `---` separator and the diff,
    //      which `git format-patch` emits and `git am` discards;
    //   3. the diff itself — from the first `diff --git`/`Index:` line onward,
    //      ending at the `-- ` signature footer format-patch appends.
    #[derive(PartialEq)]
    enum Region {
        Body,
        Diffstat,
        Diff,
    }
    let mut body_lines: Vec<&[u8]> = Vec::new();
    let mut diff = Vec::new();
    let mut region = Region::Body;
    while idx < lines.len() {
        let raw = &lines[idx];
        let line = trim_trailing_newline(raw);
        match region {
            Region::Body => {
                if is_diff_start(line) {
                    region = Region::Diff;
                    diff.extend_from_slice(raw);
                } else if line == b"---" {
                    // End of the commit message; a diffstat (or the diff) follows.
                    region = Region::Diffstat;
                } else {
                    body_lines.push(raw);
                }
            }
            Region::Diffstat => {
                // Skip diffstat lines until the patch proper begins.
                if is_diff_start(line) {
                    region = Region::Diff;
                    diff.extend_from_slice(raw);
                }
            }
            Region::Diff => {
                if line == b"-- " {
                    break;
                }
                diff.extend_from_slice(raw);
            }
        }
        idx += 1;
    }

    let message = if subject.is_empty() && !body_lines.is_empty() {
        subject = String::from_utf8_lossy(trim_trailing_newline(body_lines[0]))
            .trim()
            .to_string();
        build_commit_message(&subject, &body_lines[1..])
    } else {
        build_commit_message(&subject, &body_lines)
    };

    Ok(AmPatch {
        author_name: author_name.into_bytes(),
        author_email: author_email.into_bytes(),
        author_encoding: "UTF-8".to_string(),
        author_date,
        author_date_raw,
        subject,
        message,
        message_encoding,
        message_id,
        diff,
    })
}

/// Parse a `From:` value of the form `Name <email>` (or a bare address).
fn parse_from_header(value: &str) -> (String, String) {
    if let Some(open) = value.rfind('<')
        && let Some(close) = value[open..].find('>')
    {
        let email = value[open + 1..open + close].trim().to_string();
        let name = decode_mime_word(value[..open].trim())
            .trim_matches('"')
            .to_string();
        return (name, email);
    }
    // Bare address: use it for both, matching git's fallback for name.
    let addr = value.trim().to_string();
    (addr.clone(), addr)
}

fn content_type_charset(value: &str) -> Option<String> {
    for part in value.split(';').skip(1) {
        if let Some((key, raw_value)) = part.trim().split_once('=')
            && key.trim().eq_ignore_ascii_case("charset")
        {
            let charset = raw_value.trim().trim_matches('"');
            if !charset.is_empty() {
                return Some(charset.to_string());
            }
        }
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn consume_in_body_headers(
    lines: &[Vec<u8>],
    idx: &mut usize,
    cleanup: SubjectCleanup,
    author_name: &mut String,
    author_email: &mut String,
    author_date: &mut Option<String>,
    author_date_raw: &mut Option<String>,
    subject: &mut String,
) {
    let start = *idx;
    let mut scan = start;
    let mut last_header: Option<String> = None;
    let mut header_values: Vec<(String, String)> = Vec::new();
    let mut saw_blank = false;
    while scan < lines.len() {
        let line = trim_trailing_newline(&lines[scan]);
        if line.is_empty() {
            scan += 1;
            saw_blank = true;
            break;
        }
        if (line[0] == b' ' || line[0] == b'\t') && last_header.is_some() {
            if let Some((_, value)) = header_values.last_mut() {
                value.push(' ');
                value.push_str(String::from_utf8_lossy(line).trim());
            }
            scan += 1;
            continue;
        }
        let Some(colon) = line.iter().position(|byte| *byte == b':') else {
            return;
        };
        let name = String::from_utf8_lossy(&line[..colon])
            .trim()
            .to_lowercase();
        if !matches!(name.as_str(), "from" | "date" | "subject") {
            return;
        }
        let value = String::from_utf8_lossy(&line[colon + 1..])
            .trim()
            .to_string();
        last_header = Some(name.clone());
        header_values.push((name, value));
        scan += 1;
    }
    if !saw_blank || header_values.is_empty() {
        return;
    }
    for (name, value) in &header_values {
        match name.as_str() {
            "from" => {
                let (name, email) = parse_from_header(value);
                *author_name = name;
                *author_email = email;
            }
            "date" => {
                *author_date_raw = Some(value.clone());
                *author_date = parse_rfc2822_date(value)
                    .or_else(|| parse_raw_git_date_normalized(value))
                    .or_else(|| parse_git_default_date(value));
            }
            "subject" => *subject = clean_subject(value, cleanup),
            _ => {}
        }
    }
    *idx = scan;
}

fn is_scissors_line(line: &[u8]) -> bool {
    let text = String::from_utf8_lossy(line);
    text.contains(">8") && text.contains(" - - ")
}

/// Clean a `Subject:` value the way git's mailinfo `cleanup_subject` does:
/// repeatedly strip a leading `Re:` (case-insensitive), leading spaces / tabs /
/// colons, and `[…]` brackets, then trim. A `[…]` bracket is removed unless
/// `keep_non_patch_brackets` (`-b`/`--keep-non-patch`) is set AND the bracket is
/// ≥7 chars and does NOT contain `PATCH` — those non-patch brackets (e.g.
/// `[foo]`) are kept, along with one following space. With `keep_subject`
/// (`-k`/`--keep`) the subject is kept verbatim (only MIME-decoded + trimmed).
fn clean_subject(value: &str, cleanup: SubjectCleanup) -> String {
    let decoded = decode_mime_word(value);
    if cleanup.keep_subject {
        return decoded.trim().to_string();
    }
    let keep_non_patch = cleanup.keep_non_patch_brackets;
    let mut bytes = decoded.trim().as_bytes().to_vec();
    let mut at = 0usize;
    while at < bytes.len() {
        match bytes[at] {
            b'r' | b'R' => {
                // A leading "Re:" (any case) is dropped.
                if at + 3 <= bytes.len()
                    && (bytes[at + 1] == b'e' || bytes[at + 1] == b'E')
                    && bytes[at + 2] == b':'
                {
                    bytes.drain(at..at + 3);
                    continue;
                }
                break;
            }
            b' ' | b'\t' | b':' => {
                bytes.remove(at);
                continue;
            }
            b'[' => {
                let Some(rel) = bytes[at..].iter().position(|&b| b == b']') else {
                    break;
                };
                let remove = rel + 1; // length of "[...]"
                let contains_patch =
                    remove >= 7 && bytes[at..at + remove].windows(5).any(|w| w == b"PATCH");
                if !keep_non_patch || contains_patch {
                    bytes.drain(at..at + remove);
                } else {
                    at += remove;
                    if bytes.get(at).is_some_and(u8::is_ascii_whitespace) {
                        at += 1;
                    }
                }
                continue;
            }
            _ => break,
        }
    }
    let cleaned = cleanup_space_bytes(&bytes);
    String::from_utf8_lossy(&cleaned).trim().to_string()
}

fn cleanup_space_bytes(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut idx = 0;
    while idx < bytes.len() {
        if bytes[idx].is_ascii_whitespace() {
            out.push(b' ');
            idx += 1;
            while idx < bytes.len() && bytes[idx].is_ascii_whitespace() {
                idx += 1;
            }
        } else {
            out.push(bytes[idx]);
            idx += 1;
        }
    }
    out
}

/// Best-effort decode of RFC 2047 encoded-words for Q or B encodings.
/// Adjacent encoded words separated only by folded whitespace are concatenated,
/// which is what `format-patch -k` uses for multiline subjects.
fn decode_mime_word(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut idx = 0;
    let mut previous_encoded = false;
    while idx < value.len() {
        if let Some((decoded, consumed)) = decode_mime_word_at(&value[idx..]) {
            out.push_str(&decoded);
            idx += consumed;
            previous_encoded = true;
            continue;
        }

        let byte = value.as_bytes()[idx];
        if previous_encoded && byte.is_ascii_whitespace() {
            let whitespace_start = idx;
            while idx < value.len() && value.as_bytes()[idx].is_ascii_whitespace() {
                idx += 1;
            }
            if decode_mime_word_at(&value[idx..]).is_some() {
                continue;
            }
            out.push_str(&value[whitespace_start..idx]);
            previous_encoded = false;
            continue;
        }

        let ch = value[idx..].chars().next().unwrap();
        out.push(ch);
        idx += ch.len_utf8();
        previous_encoded = false;
    }
    out
}

fn decode_mime_word_at(value: &str) -> Option<(String, usize)> {
    let rest = value.strip_prefix("=?")?;
    let charset_end = rest.find('?')?;
    let charset = &rest[..charset_end];
    let after_charset = &rest[charset_end + 1..];
    let encoding_end = after_charset.find('?')?;
    let encoding = &after_charset[..encoding_end];
    let payload = &after_charset[encoding_end + 1..];
    let end = payload.find("?=")?;
    let encoded = &payload[..end];
    let consumed = 2 + charset_end + 1 + encoding_end + 1 + end + 2;

    let decoded = match encoding.to_ascii_uppercase().as_str() {
        "Q" => decode_quoted_printable_word(encoded),
        "B" => decode_base64(encoded),
        _ => return None,
    };
    match decoded {
        Some(bytes) => {
            let encoding = encoding_for_name(charset).unwrap_or(encoding_rs::UTF_8);
            let (decoded, _, _) = encoding.decode(&bytes);
            Some((decoded.into_owned(), consumed))
        }
        None => None,
    }
}

fn decode_quoted_printable_word(input: &str) -> Option<Vec<u8>> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut idx = 0;
    while idx < bytes.len() {
        match bytes[idx] {
            b'_' => {
                out.push(b' ');
                idx += 1;
            }
            b'=' if idx + 2 < bytes.len() => {
                let hi = (bytes[idx + 1] as char).to_digit(16)?;
                let lo = (bytes[idx + 2] as char).to_digit(16)?;
                out.push((hi * 16 + lo) as u8);
                idx += 3;
            }
            other => {
                out.push(other);
                idx += 1;
            }
        }
    }
    Some(out)
}

fn decode_base64(input: &str) -> Option<Vec<u8>> {
    fn value(byte: u8) -> Option<u32> {
        match byte {
            b'A'..=b'Z' => Some((byte - b'A') as u32),
            b'a'..=b'z' => Some((byte - b'a' + 26) as u32),
            b'0'..=b'9' => Some((byte - b'0' + 52) as u32),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let cleaned: Vec<u8> = input.bytes().filter(|byte| *byte != b'=').collect();
    let mut out = Vec::new();
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for byte in cleaned {
        let value = value(byte)?;
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    Some(out)
}

/// Whether `line` begins a unified diff (a git or plain patch).
fn is_diff_start(line: &[u8]) -> bool {
    line.starts_with(b"diff --git ")
        || line.starts_with(b"--- ")
        || line.starts_with(b"diff --cc ")
        || line.starts_with(b"Index: ")
}

/// Build the full commit message: subject, blank line, then trimmed body.
///
/// Mirrors git's `cleanup`: the subject is the first line, followed by a blank
/// line and the body with leading/trailing blank lines removed. The result is
/// newline-terminated. An empty body yields just `subject\n`.
fn build_commit_message(subject: &str, body_lines: &[&[u8]]) -> Vec<u8> {
    // Drop leading and trailing blank lines from the body.
    let mut start = 0;
    while start < body_lines.len() && trim_trailing_newline(body_lines[start]).is_empty() {
        start += 1;
    }
    let mut end = body_lines.len();
    while end > start && trim_trailing_newline(body_lines[end - 1]).is_empty() {
        end -= 1;
    }
    let mut message = Vec::new();
    message.extend_from_slice(subject.as_bytes());
    message.push(b'\n');
    if end > start {
        message.push(b'\n');
        for line in &body_lines[start..end] {
            let trimmed = trim_trailing_newline(line);
            message.extend_from_slice(trimmed);
            message.push(b'\n');
        }
    }
    message
}

// ===========================================================================
// RFC 2822 date parsing → raw git timestamp
// ===========================================================================

/// Parse an RFC 2822 `Date:` value (e.g. `Sun, 27 Sep 2026 11:06:40 +0200`)
/// into git's raw `"<seconds> <±HHMM>"` form. Returns `None` if the value is not
/// in the expected shape, so callers can fall back to the environment date.
fn parse_rfc2822_date(value: &str) -> Option<String> {
    let mut tokens: Vec<&str> = value.split_whitespace().collect();
    // Optional leading weekday with trailing comma: "Sun," or "Sun".
    if let Some(first) = tokens.first() {
        let stripped = first.trim_end_matches(',');
        if WEEKDAYS.contains(&stripped) {
            tokens.remove(0);
        }
    }
    if tokens.len() < 5 {
        return None;
    }
    let day: u32 = tokens[0].parse().ok()?;
    let month = month_index(tokens[1])?;
    let year: i64 = tokens[2].parse().ok()?;
    let (hour, minute, second) = parse_clock(tokens[3])?;
    let timezone = parse_timezone(tokens[4])?;

    let days = days_from_civil(year, month, day as i64);
    let local_seconds = days * 86_400 + (hour as i64) * 3600 + (minute as i64) * 60 + second as i64;
    let seconds = local_seconds - timezone.1;
    Some(format!("{seconds} {}", timezone.0))
}

/// Parse git's "default" (asctime-style) date as `mailinfo` accepts it:
/// `[<DoW>] <Mon> <day> <HH:MM:SS> <year> [<tz>]`, e.g.
/// `Thu Dec 4 16:00:00 2008 -0800`. Distinct from RFC 2822 by the
/// month-before-day token order; the timezone defaults to `+0000` when absent.
fn parse_git_default_date(value: &str) -> Option<String> {
    let mut tokens: Vec<&str> = value.split_whitespace().collect();
    if let Some(first) = tokens.first() {
        if WEEKDAYS.contains(&first.trim_end_matches(',')) {
            tokens.remove(0);
        }
    }
    if tokens.len() < 4 {
        return None;
    }
    let month = month_index(tokens[0])?;
    let day: u32 = tokens[1].parse().ok()?;
    let (hour, minute, second) = parse_clock(tokens[2])?;
    let year: i64 = tokens[3].parse().ok()?;
    let timezone = tokens
        .get(4)
        .and_then(|token| parse_timezone(token))
        .unwrap_or_else(|| ("+0000".to_string(), 0));

    let days = days_from_civil(year, month, day as i64);
    let local_seconds = days * 86_400 + (hour as i64) * 3600 + (minute as i64) * 60 + second as i64;
    let seconds = local_seconds - timezone.1;
    Some(format!("{seconds} {}", timezone.0))
}

/// Parse a raw git date (`<seconds> <±HHMM>` or `@<seconds> <±HHMM>`) into the
/// normalised `<seconds> <±HHMM>` form `author_date` carries. Returns `None` if
/// the value is not exactly two whitespace-separated raw-date fields.
fn parse_raw_git_date_normalized(value: &str) -> Option<String> {
    let mut parts = value.split_whitespace();
    let seconds = parts.next()?;
    let tz = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let seconds = seconds.strip_prefix('@').unwrap_or(seconds);
    if seconds.is_empty() || !seconds.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if tz.len() != 5
        || !matches!(tz.as_bytes()[0], b'+' | b'-')
        || !tz.as_bytes()[1..].iter().all(u8::is_ascii_digit)
    {
        return None;
    }
    Some(format!("{seconds} {tz}"))
}

const WEEKDAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

fn month_index(token: &str) -> Option<u32> {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    MONTHS
        .iter()
        .position(|month| month.eq_ignore_ascii_case(token))
        .map(|index| index as u32 + 1)
}

fn parse_clock(token: &str) -> Option<(u32, u32, u32)> {
    let mut parts = token.split(':');
    let hour: u32 = parts.next()?.parse().ok()?;
    let minute: u32 = parts.next()?.parse().ok()?;
    let second: u32 = match parts.next() {
        Some(value) => value.parse().ok()?,
        None => 0,
    };
    if parts.next().is_some() || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    Some((hour, minute, second))
}

/// Parse a timezone token (`+0200`, `-0500`, or a named zone) into its
/// canonical `±HHMM` string plus offset in seconds east of UTC.
fn parse_timezone(token: &str) -> Option<(String, i64)> {
    let bytes = token.as_bytes();
    if bytes.len() == 5
        && matches!(bytes[0], b'+' | b'-')
        && bytes[1..].iter().all(u8::is_ascii_digit)
    {
        let sign = if bytes[0] == b'+' { 1 } else { -1 };
        let hours: i64 = token[1..3].parse().ok()?;
        let minutes: i64 = token[3..5].parse().ok()?;
        let offset = sign * (hours * 3600 + minutes * 60);
        return Some((token.to_string(), offset));
    }
    // A handful of named zones from old mail (mostly UTC-equivalents).
    let offset = match token {
        "UT" | "GMT" | "UTC" | "Z" => 0,
        "EST" => -5 * 3600,
        "EDT" => -4 * 3600,
        "CST" => -6 * 3600,
        "CDT" => -5 * 3600,
        "MST" => -7 * 3600,
        "MDT" => -6 * 3600,
        "PST" => -8 * 3600,
        "PDT" => -7 * 3600,
        _ => return None,
    };
    Some((format_offset(offset), offset))
}

fn format_offset(offset: i64) -> String {
    let sign = if offset < 0 { '-' } else { '+' };
    let magnitude = offset.abs();
    format!(
        "{sign}{:02}{:02}",
        magnitude / 3600,
        (magnitude % 3600) / 60
    )
}

/// Days from 1970-01-01 to the given civil date (Howard Hinnant's algorithm).
/// Valid for the full proleptic Gregorian range; matches git's date arithmetic.
fn days_from_civil(year: i64, month: u32, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month = month as i64;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

// ===========================================================================
// State directory (.git/rebase-apply/)
// ===========================================================================

/// Create `.git/rebase-apply/` and populate it with the per-series control
/// files and one numbered file (`0001`, `0002`, …) per patch.
fn write_am_state_dir(
    state_dir: &Path,
    patches: &[AmPatch],
    options: &AmOptions,
    head_oid: &ObjectId,
) -> Result<()> {
    fs::create_dir_all(state_dir)?;
    fs::write(state_dir.join("next"), b"1\n")?;
    fs::write(state_dir.join("last"), format!("{}\n", patches.len()))?;
    fs::write(state_dir.join("quiet"), bool_flag(options.quiet))?;
    fs::write(state_dir.join("sign"), bool_flag(options.signoff))?;
    fs::write(state_dir.join("threeway"), bool_flag(options.three_way))?;
    fs::write(state_dir.join("keep"), bool_flag(options.keep_non_patch))?;
    fs::write(
        state_dir.join("empty"),
        empty_action_name(options.empty_action),
    )?;
    fs::write(state_dir.join("messageid"), bool_flag(options.message_id))?;
    fs::write(
        state_dir.join("committer-date-is-author-date"),
        bool_flag(options.committer_date_is_author_date),
    )?;
    fs::write(
        state_dir.join("ignore-date"),
        bool_flag(options.ignore_date),
    )?;
    fs::write(state_dir.join("no-verify"), bool_flag(options.no_verify))?;
    fs::write(state_dir.join("utf8"), bool_flag(options.utf8))?;
    fs::write(
        state_dir.join("interactive"),
        bool_flag(options.interactive),
    )?;
    write_am_rerere_autoupdate(state_dir, options.rerere_autoupdate)?;
    fs::write(state_dir.join("applying"), b"")?;
    // git records the forwarded `git apply` options here, sq-quoted as a single
    // line (`sq_quote_argv`), and re-passes them to `git apply` for every patch
    // and on resume (am.c `am_setup`/`am_load`). We persist them the same way and
    // re-apply them in-process — `-C`, `-p`, `--whitespace`, `--directory`,
    // `--reject`, `--ignore-whitespace` all round-trip through apply-opt.
    fs::write(
        state_dir.join("apply-opt"),
        format!("{}\n", sq_quote_argv(&options.git_apply_opts)),
    )?;
    // abort-safety records the HEAD the series is currently sitting on so
    // --abort can detect a HEAD the user moved out from under us. An unborn HEAD
    // records the empty string (git's am_setup writes "" for no HEAD).
    if head_oid.is_null() {
        fs::write(state_dir.join("abort-safety"), b"")?;
    } else {
        fs::write(state_dir.join("abort-safety"), format!("{head_oid}\n"))?;
    }
    for (index, patch) in patches.iter().enumerate() {
        let name = format!("{:04}", index + 1);
        fs::write(state_dir.join(name), encode_patch_file(patch))?;
    }
    Ok(())
}

fn bool_flag(value: bool) -> &'static [u8] {
    if value { b"t\n" } else { b"f\n" }
}

/// Reconstruct the numbered mbox-ish file for one patch (headers + body + diff),
/// matching the shape git stores so a human or `--show-current-patch` can read it.
fn encode_patch_file(patch: &AmPatch) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"From: ");
    out.extend_from_slice(&patch.author_name);
    out.extend_from_slice(b" <");
    out.extend_from_slice(&patch.author_email);
    out.extend_from_slice(b">\n");
    if let Some(date) = &patch.author_date_raw {
        out.extend_from_slice(b"Date: ");
        out.extend_from_slice(date.as_bytes());
        out.push(b'\n');
    }
    if let Some(message_id) = &patch.message_id {
        out.extend_from_slice(b"Message-ID: ");
        out.extend_from_slice(message_id.as_bytes());
        out.push(b'\n');
    }
    out.extend_from_slice(b"Content-Type: text/plain; charset=");
    out.extend_from_slice(patch.message_encoding.as_bytes());
    out.push(b'\n');
    // Store the already-cleaned subject verbatim (no `[PATCH]` re-prefix): it has
    // had mailinfo cleanup applied once at parse time, so `read_patch_file`
    // re-parses with `keep_subject` to keep it byte-identical across the
    // store/resume round-trip.
    write_stored_subject_header(&mut out, &patch.subject);
    out.push(b'\n');
    // Body (message minus the full stored subject).
    let body = commit_message_body_after_subject(&patch.message, &patch.subject);
    out.extend_from_slice(&body);
    out.extend_from_slice(b"---\n\n");
    out.extend_from_slice(&patch.diff);
    out
}

fn write_stored_subject_header(out: &mut Vec<u8>, subject: &str) {
    out.extend_from_slice(b"Subject: ");
    if subject.bytes().any(stored_subject_needs_rfc2047) {
        out.extend_from_slice(b"=?UTF-8?Q?");
        for byte in subject.bytes() {
            if stored_subject_q_safe(byte) {
                out.push(byte);
            } else {
                out.extend_from_slice(format!("={byte:02X}").as_bytes());
            }
        }
        out.extend_from_slice(b"?=");
    } else {
        out.extend_from_slice(subject.as_bytes());
    }
    out.push(b'\n');
}

fn stored_subject_needs_rfc2047(byte: u8) -> bool {
    byte == b'\n' || byte == b'\r' || byte >= 0x80 || byte == b'='
}

fn stored_subject_q_safe(byte: u8) -> bool {
    byte.is_ascii_graphic() && byte != b'=' && byte != b'?' && byte != b'_' && byte < 0x80
}

/// Return the commit body (everything after the subject line and its trailing
/// blank line). Empty when the message is subject-only.
fn commit_message_body_after_subject(message: &[u8], subject: &str) -> Vec<u8> {
    let subject = subject.as_bytes();
    if message.starts_with(subject) && message.get(subject.len()) == Some(&b'\n') {
        let mut start = subject.len() + 1;
        if message.get(start) == Some(&b'\n') {
            start += 1;
        }
        return message[start..].to_vec();
    }

    let Some(first_lf) = message.iter().position(|byte| *byte == b'\n') else {
        return Vec::new();
    };
    let mut start = first_lf + 1;
    if message.get(start) == Some(&b'\n') {
        start += 1;
    }
    message[start..].to_vec()
}

/// Write the per-patch control files git consults while a patch is current:
/// `author-script`, `info`, `final-commit`, `msg`, and `patch`.
fn write_current_patch_state(state_dir: &Path, patch: &AmPatch) -> Result<()> {
    let author_date = patch
        .author_date_raw
        .clone()
        .unwrap_or_else(default_author_date);
    let mut author_script = b"GIT_AUTHOR_NAME=".to_vec();
    author_script.extend_from_slice(&shell_quote_bytes(&patch.author_name));
    author_script.extend_from_slice(b"\nGIT_AUTHOR_EMAIL=");
    author_script.extend_from_slice(&shell_quote_bytes(&patch.author_email));
    author_script.extend_from_slice(b"\nGIT_AUTHOR_DATE=");
    author_script.extend_from_slice(shell_quote(&author_date).as_bytes());
    author_script.push(b'\n');
    fs::write(state_dir.join("author-script"), author_script)?;

    let author_name = String::from_utf8_lossy(&patch.author_name);
    let author_email = String::from_utf8_lossy(&patch.author_email);
    let info = format!(
        "Author: {}\nEmail: {}\nSubject: {}\nDate: {}\n\n",
        author_name, author_email, patch.subject, author_date,
    );
    fs::write(state_dir.join("info"), info)?;

    fs::write(state_dir.join("final-commit"), &patch.message)?;
    fs::write(state_dir.join("msg"), &patch.message)?;
    fs::write(state_dir.join("patch"), &patch.diff)?;
    Ok(())
}

fn default_author_date() -> String {
    env::var("GIT_AUTHOR_DATE").unwrap_or_else(|_| "@0 +0000".into())
}

/// Single-quote a value for the POSIX-sh `author-script`, escaping embedded
/// quotes the way git does (`'\''`).
fn shell_quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for ch in value.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

fn shell_quote_bytes(value: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(value.len() + 2);
    out.push(b'\'');
    for byte in value {
        if *byte == b'\'' {
            out.extend_from_slice(b"'\\''");
        } else {
            out.push(*byte);
        }
    }
    out.push(b'\'');
    out
}

/// Read the numbered patch file `<n>` back into an [`AmPatch`] when resuming.
fn read_patch_file(state_dir: &Path, number: usize) -> Result<AmPatch> {
    let path = state_dir.join(format!("{number:04}"));
    let content = fs::read(&path)?;
    let lines = split_keep_newline(&content);
    // The stored subject was already cleaned at the first parse (see
    // `encode_patch_file`), so re-parse it verbatim — re-running cleanup would
    // double-strip brackets the original `-k`/`-b` run deliberately kept.
    parse_message(
        &lines,
        SubjectCleanup {
            keep_subject: true,
            keep_non_patch_brackets: false,
            scissors: false,
        },
    )
}

fn read_state_usize(state_dir: &Path, name: &str) -> Result<usize> {
    let content = fs::read_to_string(state_dir.join(name))?;
    content
        .trim()
        .parse::<usize>()
        .map_err(|_| GitError::InvalidFormat(format!("invalid rebase-apply/{name}")))
}

fn read_state_bool(state_dir: &Path, name: &str) -> bool {
    fs::read_to_string(state_dir.join(name))
        .map(|content| content.trim() == "t")
        .unwrap_or(false)
}

fn empty_action_name(action: AmEmptyAction) -> &'static str {
    match action {
        AmEmptyAction::Stop => "stop\n",
        AmEmptyAction::Drop => "drop\n",
        AmEmptyAction::Keep => "keep\n",
    }
}

fn read_empty_action(state_dir: &Path) -> AmEmptyAction {
    match fs::read_to_string(state_dir.join("empty"))
        .unwrap_or_default()
        .trim()
    {
        "drop" => AmEmptyAction::Drop,
        "keep" => AmEmptyAction::Keep,
        _ => AmEmptyAction::Stop,
    }
}

/// Read the commit-affecting flags back from the state directory so a resumed
/// (`--continue`/`--skip`) run builds identical commits to an uninterrupted one.
fn read_am_commit_opts(state_dir: &Path) -> AmCommitOpts {
    AmCommitOpts {
        signoff: read_state_bool(state_dir, "sign"),
        message_id: read_state_bool(state_dir, "messageid"),
        committer_date_is_author_date: read_state_bool(state_dir, "committer-date-is-author-date"),
        ignore_date: read_state_bool(state_dir, "ignore-date"),
        no_verify: read_state_bool(state_dir, "no-verify"),
        utf8: !state_dir.join("utf8").exists() || read_state_bool(state_dir, "utf8"),
        // The `head-name` marker is written only by the rebase apply backend
        // (start_rebase_apply); a bare `git am` never writes it. Its presence
        // selects the rebase per-pick reflog format.
        rebase_pick_reflog: state_dir.join("head-name").exists(),
    }
}

// ===========================================================================
// Series driver
// ===========================================================================

/// Command-line options that, when a session is *resumed* (`--retry`/
/// `--continue`/`--skip`), override the saved session options — but only for the
/// single patch being resumed. git applies the override in-memory for that
/// patch, then `am_load`s the saved state for every subsequent patch (am.c's
/// `am_run` resume loop), so e.g. `am --signoff --continue` signs off only the
/// resumed commit (t4153).
#[derive(Clone, Copy, Default)]
struct AmResumeOverrides {
    three_way: Option<bool>,
    quiet: Option<bool>,
    signoff: Option<bool>,
    reject: Option<bool>,
}

impl AmResumeOverrides {
    fn any(&self) -> bool {
        self.three_way.is_some()
            || self.quiet.is_some()
            || self.signoff.is_some()
            || self.reject.is_some()
    }
}

/// The forwarded `git apply` options, parsed out of the `apply-opt` state line
/// (or the command line at start). Mirrors the subset of `git apply` flags that
/// `git am` passes through (`state->git_apply_opts`) and that change how a
/// fragment is placed or materialised.
#[derive(Clone, Default)]
struct AmApplyOpts {
    /// `-p<n>`: leading path components to strip. `None` means the default (1).
    p_value: Option<usize>,
    /// `-C<n>`: the context-fuzz floor (git's `p_context`). `None` means no fuzz
    /// (git's default `UINT_MAX`): a hunk whose full context fails is rejected.
    context: Option<usize>,
    /// `--whitespace=<action>`: whitespace-error handling. Only `fix` changes the
    /// applied bytes (it strips trailing whitespace from added lines).
    whitespace: Option<String>,
    /// `--directory=<dir>`: prepend `<dir>/` to every patched path.
    directory: Option<String>,
    /// `--reject`: apply the hunks that match and write `.rej` files for the rest.
    reject: bool,
    /// `--ignore-whitespace` / `--ignore-space-change`: match context/deleted
    /// lines with a whitespace-collapsing comparison.
    ignore_whitespace: bool,
}

impl AmApplyOpts {
    /// `-p<n>` strip level, defaulting to git's 1.
    fn p_value(&self) -> usize {
        self.p_value.unwrap_or(1)
    }
    /// Whether `--whitespace=fix` is in effect (the only action that rewrites the
    /// applied content).
    fn whitespace_fix(&self) -> bool {
        self.whitespace.as_deref() == Some("fix")
    }
}

/// git's `sq_quote_argv`: emit each argument prefixed by a space and wrapped in
/// single quotes, rendering an embedded `'` (or `!`) as the `'\''` escape. This
/// is the exact byte layout `git am` writes to `rebase-apply/apply-opt`.
fn sq_quote_argv(args: &[String]) -> String {
    let mut out = String::new();
    for arg in args {
        out.push(' ');
        out.push('\'');
        for ch in arg.chars() {
            if ch == '\'' || ch == '!' {
                out.push('\'');
                out.push('\\');
                out.push(ch);
                out.push('\'');
            } else {
                out.push(ch);
            }
        }
        out.push('\'');
    }
    out
}

/// git's `sq_dequote` (quote.c `sq_dequote_step`): split an sq-quoted line back
/// into the original argument tokens. Each token is wrapped in `'…'`; a literal
/// `'` (or `!`) inside is the `'\''` escape — close-quote, backslash-escaped
/// char, reopen-quote — which git decodes by emitting the escaped char and
/// stepping over the reopening quote. Tokens are separated by unquoted
/// whitespace; an unquoted run is tolerated and copied verbatim.
fn sq_dequote_to_vec(input: &str) -> Vec<String> {
    let bytes = input.as_bytes();
    let n = bytes.len();
    let mut tokens = Vec::new();
    let mut i = 0;
    loop {
        while i < n && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= n {
            break;
        }
        if bytes[i] != b'\'' {
            // Not sq-quoted: copy the bare run verbatim (defensive — git's am
            // always quotes, but we never want to drop a token).
            let start = i;
            while i < n && !bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            tokens.push(String::from_utf8_lossy(&bytes[start..i]).into_owned());
            continue;
        }
        // `src` mirrors git's pointer; the loop reads with a leading `++src`.
        let mut token = Vec::new();
        let mut src = i;
        loop {
            src += 1;
            if src >= n {
                break; // unterminated quote; take what we have
            }
            let c = bytes[src];
            if c != b'\'' {
                token.push(c);
                continue;
            }
            // Stepped out of the single-quoted span; inspect the next char.
            src += 1;
            if src >= n {
                break;
            }
            if bytes[src] == b'\\'
                && src + 2 < n
                && (bytes[src + 1] == b'\'' || bytes[src + 1] == b'!')
                && bytes[src + 2] == b'\''
            {
                // `'\''` / `'\!'`: emit the escaped char and step over the
                // reopening quote (the next loop iteration's `++src` skips it).
                token.push(bytes[src + 1]);
                src += 2;
                continue;
            }
            // Otherwise the token ended at the closing quote.
            break;
        }
        i = src;
        tokens.push(String::from_utf8_lossy(&token).into_owned());
    }
    tokens
}

/// Parse the forwarded `git apply` option tokens (in git's recreate-opt form)
/// into the structured [`AmApplyOpts`] the in-process apply consumes.
fn parse_am_apply_opts(tokens: &[String]) -> AmApplyOpts {
    let mut opts = AmApplyOpts::default();
    let mut i = 0;
    while i < tokens.len() {
        let token = tokens[i].as_str();
        match token {
            "--reject" => opts.reject = true,
            "--no-reject" => opts.reject = false,
            "--ignore-whitespace" | "--ignore-space-change" => opts.ignore_whitespace = true,
            "--whitespace" => {
                if let Some(value) = tokens.get(i + 1) {
                    opts.whitespace = Some(value.clone());
                    i += 1;
                }
            }
            "-C" => {
                if let Some(value) = tokens.get(i + 1) {
                    opts.context = value.parse().ok();
                    i += 1;
                }
            }
            "-p" => {
                if let Some(value) = tokens.get(i + 1) {
                    opts.p_value = value.parse().ok();
                    i += 1;
                }
            }
            "--directory" => {
                if let Some(value) = tokens.get(i + 1) {
                    opts.directory = Some(value.clone());
                    i += 1;
                }
            }
            _ => {
                if let Some(action) = token.strip_prefix("--whitespace=") {
                    opts.whitespace = Some(action.to_string());
                } else if let Some(rest) = token.strip_prefix("-C") {
                    opts.context = rest.parse().ok();
                } else if let Some(rest) = token.strip_prefix("-p") {
                    opts.p_value = rest.parse().ok();
                } else if let Some(dir) = token.strip_prefix("--directory=") {
                    opts.directory = Some(dir.to_string());
                }
            }
        }
        i += 1;
    }
    opts
}

/// Read the forwarded `git apply` options back from `rebase-apply/apply-opt` so a
/// resumed `--skip`/`--continue`/`--retry` reproduces them for every patch (the
/// `git am` "do not lose the options" guarantee — t4252).
fn read_am_apply_opts(state_dir: &Path) -> AmApplyOpts {
    let raw = fs::read_to_string(state_dir.join("apply-opt")).unwrap_or_default();
    parse_am_apply_opts(&sq_dequote_to_vec(&raw))
}

/// Prepend `--directory=<dir>` to every path in the parsed patch (git's
/// `git apply --directory`): each `old_path`/`new_path` becomes `<dir>/<path>`.
fn am_prepend_directory(file_patches: &mut [sley_diff_merge::FilePatch], dir: &str) {
    let prefix = {
        let mut p = dir.as_bytes().to_vec();
        if !p.ends_with(b"/") {
            p.push(b'/');
        }
        p
    };
    for file in file_patches.iter_mut() {
        for path in [file.old_path.as_mut(), file.new_path.as_mut()]
            .into_iter()
            .flatten()
        {
            let mut joined = prefix.clone();
            joined.extend_from_slice(path);
            *path = joined;
        }
    }
}

/// The user's choice at the `am -i` prompt for one patch.
enum AmInteractiveDecision {
    /// Apply this patch (`y`).
    Apply,
    /// Skip this patch (`n`).
    Skip,
    /// Apply this and every remaining patch without prompting (`a`).
    AcceptAll,
}

/// git's `do_interactive`: show the commit body and prompt whether to apply the
/// current patch. Loops on `e`/`v` (edit/view — no-ops here, no editor/pager in
/// the non-interactive test harness) until a decisive `y`/`n`/`a` is read.
fn am_do_interactive(message: &[u8]) -> Result<AmInteractiveDecision> {
    use std::io::Write;
    let message = String::from_utf8_lossy(message);
    loop {
        println!("Commit Body is:");
        println!("--------------------------");
        print!("{message}");
        if !message.ends_with('\n') {
            println!();
        }
        println!("--------------------------");
        print!("Apply? [y]es/[n]o/[e]dit/[v]iew patch/[a]ccept all: ");
        std::io::stdout().flush().ok();
        let mut reply = String::new();
        if std::io::stdin().read_line(&mut reply)? == 0 {
            return Err(GitError::Command(
                "unable to read from stdin; aborting".into(),
            ));
        }
        match reply.chars().next() {
            Some('y') | Some('Y') => return Ok(AmInteractiveDecision::Apply),
            Some('a') | Some('A') => return Ok(AmInteractiveDecision::AcceptAll),
            Some('n') | Some('N') => return Ok(AmInteractiveDecision::Skip),
            // 'e' (edit) / 'v' (view) — re-prompt.
            _ => continue,
        }
    }
}

/// Parse the option overrides a resume verb (`--retry`/`--continue`) may carry.
/// Only options that change saved session state are tracked; others are ignored
/// (the resume path does not run the full `setup_am_options`).
fn parse_am_resume_overrides(option_args: &[String]) -> AmResumeOverrides {
    let mut overrides = AmResumeOverrides::default();
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

/// Apply patches `start..=last` from the state directory, committing each.
///
/// On a clean apply this advances HEAD per patch and, after the final patch,
/// removes the state directory. On a conflict it leaves the state in place,
/// prints git's hint block, and exits 128 so the user can resolve and
/// `--continue` / `--skip` / `--abort`.
///
/// `overrides` (non-empty only on `--retry`) override the saved options for the
/// resumed patch at `start`; subsequent patches use the saved session options.
fn run_am_series(
    git_dir: &Path,
    common_git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
    state_dir: &Path,
    start: usize,
    overrides: AmResumeOverrides,
    config: &GitConfig,
    lazy_fetch: bool,
) -> Result<()> {
    let last = read_state_usize(state_dir, "last")?;
    let saved_quiet = read_state_bool(state_dir, "quiet");
    let saved_three_way = read_state_bool(state_dir, "threeway");
    let empty_action = read_empty_action(state_dir);
    let saved_apply_opts = read_am_apply_opts(state_dir);
    let saved_commit_opts = read_am_commit_opts(state_dir);
    let mut interactive = read_state_bool(state_dir, "interactive");

    let mut number = start;
    while number <= last {
        // The resumed patch (only `start`, only under `--retry`) honours the
        // command-line overrides; everything after it reverts to saved options.
        let resumed = overrides.any() && number == start;
        let quiet = if resumed {
            overrides.quiet.unwrap_or(saved_quiet)
        } else {
            saved_quiet
        };
        let three_way = if resumed {
            overrides.three_way.unwrap_or(saved_three_way)
        } else {
            saved_three_way
        };
        let mut commit_opts = saved_commit_opts;
        if resumed && let Some(signoff) = overrides.signoff {
            commit_opts.signoff = signoff;
        }
        // The resumed patch may also override `--reject`/`--no-reject` (t4153 #6);
        // the rest of the forwarded apply options come straight from apply-opt.
        let mut apply_opts = saved_apply_opts.clone();
        if resumed && let Some(reject) = overrides.reject {
            apply_opts.reject = reject;
        }
        fs::write(state_dir.join("next"), format!("{number}\n"))?;
        let mut patch = read_patch_file(state_dir, number)?;
        write_current_patch_state(state_dir, &patch)?;

        // git's `am_run`: in interactive mode, prompt before each patch. `n`
        // skips it (HEAD unchanged), `a` applies this and stops prompting.
        if interactive {
            match am_do_interactive(&patch.message)? {
                AmInteractiveDecision::Apply => {}
                AmInteractiveDecision::Skip => {
                    number += 1;
                    continue;
                }
                AmInteractiveDecision::AcceptAll => {
                    interactive = false;
                    fs::write(state_dir.join("interactive"), bool_flag(false))?;
                }
            }
        }

        if patch.diff.is_empty() {
            match empty_action {
                AmEmptyAction::Stop => {
                    am_print_empty_patch_hints();
                    println!("Patch is empty.");
                    return Err(GitError::Exit(128));
                }
                AmEmptyAction::Drop => {
                    if !quiet {
                        println!("Skipping: {}", patch.subject);
                    }
                    number += 1;
                    continue;
                }
                AmEmptyAction::Keep => {
                    patch.message = prepare_am_commit_message(git_dir, &patch, commit_opts)?;
                    if !quiet {
                        println!("Creating an empty commit: {}", patch.subject);
                    }
                    let new_oid = create_am_commit(
                        git_dir,
                        common_git_dir,
                        worktree_root,
                        format,
                        &patch,
                        commit_opts,
                        config,
                    )?;
                    record_rebase_rewrite(state_dir, format, number, &new_oid)?;
                    number += 1;
                    continue;
                }
            }
        }

        // git runs the applypatch-msg hook BEFORE applying the patch, so a
        // failing hook leaves HEAD and the worktree untouched (the patch never
        // lands). The hook may rewrite the message in `final-commit`; we re-read
        // it so the resulting commit reflects the edit. `--no-verify` skips it.
        patch.message = prepare_am_commit_message(git_dir, &patch, commit_opts)?;

        if !quiet {
            println!("Applying: {}", patch.subject);
        }

        match apply_one_patch(
            config,
            git_dir,
            common_git_dir,
            worktree_root,
            format,
            state_dir,
            number,
            &patch,
            commit_opts,
            three_way,
            &apply_opts,
            quiet,
            lazy_fetch,
        )? {
            ApplyResult::Committed => number += 1,
            ApplyResult::Skipped => number += 1,
            ApplyResult::Conflict => {
                am_print_conflict_hints();
                println!("Patch failed at {number:04} {}", patch.subject);
                // Record the stop tip as the abort-safety point (git's am_next)
                // so `am --abort` can tell whether the user moved HEAD after the
                // failure. The rebase backend keeps its rewind target here, so
                // only a bare `git am` (no head-name marker) updates it.
                if !state_dir.join("head-name").exists() {
                    let refs = FileRefStore::new(git_dir, format);
                    match head_commit_oid(&refs)? {
                        Some(oid) => fs::write(state_dir.join("abort-safety"), format!("{oid}\n"))?,
                        None => fs::write(state_dir.join("abort-safety"), b"")?,
                    }
                }
                return Err(GitError::Exit(128));
            }
        }
    }

    finish_am(
        git_dir,
        common_git_dir,
        worktree_root,
        format,
        state_dir,
        config,
        lazy_fetch,
    )
}

/// Build the commit message for a patch and run the `applypatch-msg` hook the
/// way git does — BEFORE the patch is applied. Appends the `Message-ID:` trailer
/// (when `--message-id`/`am.messageid` is set), writes it to
/// `.git/rebase-apply/final-commit`, runs `applypatch-msg` (unless
/// `--no-verify`), and returns the possibly hook-edited message read back from
/// the file. A non-zero hook exit aborts the whole am run (exit 1) with the
/// state dir and HEAD left intact, exactly like git's `run_applypatch_msg_hook`.
fn prepare_am_commit_message(
    git_dir: &Path,
    patch: &AmPatch,
    commit_opts: AmCommitOpts,
) -> Result<Vec<u8>> {
    let mut message = patch.message.clone();
    if commit_opts.message_id
        && let Some(message_id) = &patch.message_id
    {
        am_append_message_id(&mut message, message_id);
    }
    let final_commit = git_dir.join("rebase-apply").join("final-commit");
    fs::write(&final_commit, &message)?;
    if !commit_opts.no_verify {
        let arg = final_commit.to_string_lossy().into_owned();
        // A failing applypatch-msg hook aborts the series; git exits 1 and leaves
        // the state dir in place so the user can fix the hook and resume.
        if commands::hooks::run_hook_l_at(git_dir, "applypatch-msg", &[arg.as_str()]).is_err() {
            return Err(GitError::Exit(1));
        }
    }
    // Re-read: the hook may have rewritten the message in `final-commit`.
    Ok(fs::read(&final_commit)?)
}

/// Outcome of attempting to apply (and commit) a single patch.
enum ApplyResult {
    Committed,
    Skipped,
    Conflict,
}

/// Whether the index differs from HEAD (git's `repo_index_has_changes`): the
/// index tree is written and compared to HEAD's tree. `git am` refuses to begin
/// applying onto such a dirty index. Returns `false` when the index is clean or
/// cannot be written to a tree (e.g. unmerged entries are handled elsewhere).
fn am_index_is_dirty(
    git_dir: &Path,
    common_git_dir: &Path,
    format: ObjectFormat,
    head_oid: &ObjectId,
) -> Result<bool> {
    let index_tree = match sley_worktree::write_tree_from_index(git_dir, format) {
        Ok(tree) => tree,
        // An index that cannot be written to a tree is not the "dirty" case this
        // guard targets; let the normal apply path surface any real problem.
        Err(_) => return Ok(false),
    };
    let db = FileObjectDatabase::from_git_dir(common_git_dir, format);
    let head_tree = commit_tree_oid(&db, format, head_oid)?;
    Ok(index_tree != head_tree)
}

/// Apply one patch's diff to the worktree+index and create the commit.
///
/// First tries straight application (the same engine `git apply` uses). If that
/// fails and `-3` was requested, falls back to a 3-way merge against the index's
/// recorded blobs. A clean result is committed and HEAD advanced; an unresolved
/// 3-way leaves conflict markers in the worktree and a conflicted index.
#[allow(clippy::too_many_arguments)]
fn apply_one_patch(
    config: &GitConfig,
    git_dir: &Path,
    common_git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
    state_dir: &Path,
    number: usize,
    patch: &AmPatch,
    commit_opts: AmCommitOpts,
    three_way: bool,
    apply_opts: &AmApplyOpts,
    quiet: bool,
    lazy_fetch: bool,
) -> Result<ApplyResult> {
    let mut file_patches = sley_diff_merge::parse_unified_patch_with_options(
        &patch.diff,
        false,
        &sley_diff_merge::PatchPathOptions {
            p_value: apply_opts.p_value(),
            p_value_known: true,
            root: Vec::new(),
            prefix: Vec::new(),
        },
    )?;
    // `--directory=<dir>` (persisted in apply-opt) prepends <dir>/ to every path
    // before the patch is applied (t4252 "apply to a funny path").
    if let Some(dir) = &apply_opts.directory {
        am_prepend_directory(&mut file_patches, dir);
    }

    // `git am --reject` deliberately gives up the atomic semantics of the
    // normal apply path: every hunk is tried, the ones that apply are written
    // to the worktree, and every failed hunk is copied to `<path>.rej`.  The
    // index is left untouched and the am session stops so the user can inspect
    // or resolve the partial result.  Keep this ahead of the normal straight
    // apply / 3-way decision; `--reject` and `--3way` are mutually exclusive in
    // Git's apply machinery and a rejected hunk must never fall through to the
    // 3-way backend.
    if apply_opts.reject {
        let (actions, rejects) = try_reject_apply(
            common_git_dir,
            worktree_root,
            format,
            &file_patches,
            apply_opts,
        )?;
        apply_actions(git_dir, worktree_root, format, &actions)?;
        write_am_rejects(worktree_root, &rejects)?;
        if !rejects.is_empty() {
            return Ok(ApplyResult::Conflict);
        }
        let new_oid = stage_and_commit(
            git_dir,
            common_git_dir,
            worktree_root,
            format,
            patch,
            &actions,
            commit_opts,
            config,
        )?;
        record_rebase_rewrite(state_dir, format, number, &new_oid)?;
        return Ok(ApplyResult::Committed);
    }

    match try_straight_apply(
        common_git_dir,
        worktree_root,
        format,
        &file_patches,
        apply_opts,
    )? {
        Some(actions) => {
            apply_actions(git_dir, worktree_root, format, &actions)?;
            let new_oid = stage_and_commit(
                git_dir,
                common_git_dir,
                worktree_root,
                format,
                patch,
                &actions,
                commit_opts,
                config,
            )?;
            record_rebase_rewrite(state_dir, format, number, &new_oid)?;
            Ok(ApplyResult::Committed)
        }
        None => {
            if three_way {
                if !quiet {
                    println!("Using index info to reconstruct a base tree...");
                }
                return apply_three_way(
                    config,
                    git_dir,
                    common_git_dir,
                    worktree_root,
                    format,
                    state_dir,
                    number,
                    patch,
                    &file_patches,
                    commit_opts,
                    quiet,
                    lazy_fetch,
                );
            }
            for file in &file_patches {
                let name = file
                    .new_path
                    .as_deref()
                    .or(file.old_path.as_deref())
                    .unwrap_or(b"");
                eprintln!("error: patch failed: {}:1", String::from_utf8_lossy(name));
                eprintln!(
                    "error: {}: patch does not apply",
                    String::from_utf8_lossy(name)
                );
            }
            Ok(ApplyResult::Conflict)
        }
    }
}

/// One reject file produced by the hunk-by-hunk `--reject` apply path.
struct AmReject {
    path: Vec<u8>,
    contents: Vec<u8>,
}

/// Apply every textual hunk independently, retaining successful worktree
/// changes and collecting the failed hunks for `.rej` materialisation.
///
/// This is intentionally an am-level adapter over the shared diff engine: the
/// engine owns fragment placement and reject rendering, while am owns its
/// worktree-only stop state (the index must remain at HEAD until `am
/// --continue`).
fn try_reject_apply(
    common_git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
    file_patches: &[sley_diff_merge::FilePatch],
    apply_opts: &AmApplyOpts,
) -> Result<(Vec<ApplyFileAction>, Vec<AmReject>)> {
    let db = FileObjectDatabase::from_git_dir(common_git_dir, format);
    let mut actions = Vec::new();
    let mut rejects = Vec::new();

    for patch in file_patches {
        let target = patch
            .new_path
            .clone()
            .or_else(|| patch.old_path.clone())
            .ok_or_else(|| GitError::InvalidFormat("patch missing target path".into()))?;
        let old_path = patch.old_path.as_deref().unwrap_or(&target);
        let base = if patch.is_new {
            Vec::new()
        } else if patch.old_mode == Some(0o160000) {
            am_gitlink_preimage_from_patch(patch)
        } else {
            let rel = std::str::from_utf8(old_path)
                .map_err(|_| GitError::InvalidFormat("non-utf8 patch path".into()))?;
            fs::read(worktree_root.join(rel)).unwrap_or_default()
        };

        // Binary patches have no textual fragments to salvage.  A clean binary
        // apply is still accepted under `--reject`; a failed one is represented
        // as a normal apply failure because Git cannot create a textual reject
        // hunk for it either.
        if patch.is_binary {
            match commands::plumbing::apply_binary_outcome(&db, format, patch, &base)? {
                commands::plumbing::BinaryApply::Deletion => {
                    if let Some(old) = &patch.old_path {
                        actions.push(ApplyFileAction::Remove { path: old.clone() });
                    }
                }
                commands::plumbing::BinaryApply::Content(content) => {
                    actions.push(ApplyFileAction::Write {
                        path: target,
                        mode: patch.new_mode.or(patch.old_mode).unwrap_or(0o100644),
                        content,
                    });
                    if patch.is_rename
                        && let Some(old) = &patch.old_path
                    {
                        actions.push(ApplyFileAction::Remove { path: old.clone() });
                    }
                }
            }
            continue;
        }

        let fixed_patch;
        let patch_to_apply = if apply_opts.whitespace_fix() {
            fixed_patch = am_whitespace_fix_patch(patch);
            &fixed_patch
        } else {
            patch
        };
        let result = sley_diff_merge::apply_file_patch_rejecting(
            &base,
            patch_to_apply,
            &sley_diff_merge::ApplyFileOptions::default(),
        );

        if !result.rejected.is_empty() {
            let mut contents = Vec::new();
            contents.extend_from_slice(b"diff a/");
            contents.extend_from_slice(&target);
            contents.extend_from_slice(b" b/");
            contents.extend_from_slice(&target);
            contents.extend_from_slice(b"\t(rejected hunks)\n");
            for &index in &result.rejected {
                contents
                    .extend_from_slice(&sley_diff_merge::render_reject_hunk(&patch.hunks[index]));
            }
            rejects.push(AmReject {
                path: target.clone(),
                contents,
            });
        }

        if patch.is_delete && result.rejected.is_empty() {
            if let Some(old) = &patch.old_path {
                actions.push(ApplyFileAction::Remove { path: old.clone() });
            }
        } else {
            actions.push(ApplyFileAction::Write {
                path: target,
                mode: patch.new_mode.or(patch.old_mode).unwrap_or(0o100644),
                content: result.content,
            });
            if patch.is_rename
                && let Some(old) = &patch.old_path
            {
                actions.push(ApplyFileAction::Remove { path: old.clone() });
            }
        }
    }

    Ok((actions, rejects))
}

fn write_am_rejects(worktree_root: &Path, rejects: &[AmReject]) -> Result<()> {
    for reject in rejects {
        let rel = std::str::from_utf8(&reject.path)
            .map_err(|err| GitError::InvalidPath(err.to_string()))?;
        let mut path = worktree_root.join(rel).into_os_string();
        path.push(".rej");
        let path = PathBuf::from(path);
        let _ = fs::remove_file(&path);
        fs::write(path, &reject.contents)?;
    }
    Ok(())
}

/// A single materialisation step computed from a patch (write or remove a file).
enum ApplyFileAction {
    Write {
        path: Vec<u8>,
        mode: u32,
        content: Vec<u8>,
    },
    Remove {
        path: Vec<u8>,
    },
}

/// Compute the file actions for every hunk against the current worktree, or
/// `None` if any hunk fails to apply (so the whole patch is atomic, like git).
fn try_straight_apply(
    common_git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
    file_patches: &[sley_diff_merge::FilePatch],
    apply_opts: &AmApplyOpts,
) -> Result<Option<Vec<ApplyFileAction>>> {
    let mut actions = Vec::new();
    let db = FileObjectDatabase::from_git_dir(common_git_dir, format);
    for patch in file_patches {
        // git apply (`check_to_create` in apply.c) rejects a create patch when
        // the target already exists in the working tree — the whole patch fails
        // ("already exists in working directory"), which the am/rebase caller
        // turns into a conflict. Without this, a "new file" patch silently
        // clobbers an existing file instead of conflicting.
        // A working-tree *directory* at the target is fine (git's check_to_create
        // returns 0 for S_ISDIR): an added gitlink populates an existing empty
        // submodule directory rather than conflicting. Any other existing entry
        // still fails the create (→ conflict / `git am` aborts).
        if patch.is_new
            && let Some(target) = patch.new_path.as_deref().or(patch.old_path.as_deref())
            && let Ok(rel) = std::str::from_utf8(target)
            && let Ok(meta) = std::fs::symlink_metadata(worktree_root.join(rel))
            && !(meta.is_dir() && patch.new_mode == Some(0o160000))
        {
            return Ok(None);
        }
        let base = if patch.is_new {
            Vec::new()
        } else if patch.old_mode == Some(0o160000) {
            // Gitlink (submodule) preimage: the working tree is a submodule
            // directory, not a readable blob, so synthesize `Subproject commit
            // <old>\n` from the patch's own old-side lines (git's
            // `SUBMODULE_PATCH_WITHOUT_INDEX` path).
            am_gitlink_preimage_from_patch(patch)
        } else if let Some(old) = patch.old_path.as_deref().or(patch.new_path.as_deref()) {
            let rel = std::str::from_utf8(old)
                .map_err(|_| GitError::InvalidFormat("non-utf8 patch path".into()))?;
            fs::read(worktree_root.join(rel)).unwrap_or_default()
        } else {
            Vec::new()
        };

        if patch.is_binary {
            match commands::plumbing::apply_binary_outcome(&db, format, patch, &base)? {
                commands::plumbing::BinaryApply::Deletion => {
                    if let Some(old) = &patch.old_path {
                        actions.push(ApplyFileAction::Remove { path: old.clone() });
                    }
                }
                commands::plumbing::BinaryApply::Content(content) => {
                    let mode = patch.new_mode.or(patch.old_mode).unwrap_or(0o100644);
                    let Some(target) = patch.new_path.clone().or_else(|| patch.old_path.clone())
                    else {
                        return Err(GitError::InvalidFormat("patch missing target path".into()));
                    };
                    actions.push(ApplyFileAction::Write {
                        path: target,
                        mode,
                        content,
                    });
                    if patch.is_rename
                        && let Some(old) = &patch.old_path
                    {
                        actions.push(ApplyFileAction::Remove { path: old.clone() });
                    }
                }
            }
            continue;
        }

        // `--whitespace=fix` rewrites added lines (strip trailing whitespace)
        // before they are applied, so the materialised file loses the whitespace
        // errors (t4252 "interrupted am --whitespace=fix" loses the space after
        // "Six"). The preimage (context/deleted lines) is untouched, so matching
        // is unaffected.
        let fixed_patch;
        let patch_to_apply: &sley_diff_merge::FilePatch = if apply_opts.whitespace_fix() {
            fixed_patch = am_whitespace_fix_patch(patch);
            &fixed_patch
        } else {
            patch
        };
        let content = match sley_diff_merge::apply_file_patch(&base, patch_to_apply) {
            sley_diff_merge::ApplyOutcome::Applied(content) => content,
            sley_diff_merge::ApplyOutcome::Rejected => {
                // `-C<n>` (apply.c `p_context`): retry with the context-fuzz loop,
                // dropping leading/trailing context down to the floor `n` so a
                // hunk whose outer context does not match still lands (t4252
                // "interrupted am -C1"). git lowers `p_context` only when `-C`
                // was given; the default is no fuzz at all.
                if let Some(p_context) = apply_opts.context {
                    match am_apply_context_fuzz(&base, patch_to_apply, p_context) {
                        Some(content) => content,
                        None => return Ok(None),
                    }
                } else if apply_opts.ignore_whitespace {
                    // `git am --ignore-whitespace` (apply.c `ignore_ws_change`):
                    // when a hunk fails to match byte-for-byte, retry with a
                    // whitespace-collapsing line matcher, keeping the *target's*
                    // context lines so only the patch's real change lands.
                    match apply_file_patch_ignore_ws(&base, patch_to_apply) {
                        Some(content) => content,
                        None => return Ok(None),
                    }
                } else {
                    return Ok(None);
                }
            }
        };
        if patch.is_delete {
            if let Some(old) = &patch.old_path {
                actions.push(ApplyFileAction::Remove { path: old.clone() });
            }
        } else {
            let mode = patch.new_mode.or(patch.old_mode).unwrap_or(0o100644);
            let Some(target) = patch.new_path.clone().or_else(|| patch.old_path.clone()) else {
                return Err(GitError::InvalidFormat("patch missing target path".into()));
            };
            actions.push(ApplyFileAction::Write {
                path: target,
                mode,
                content,
            });
            if patch.is_rename
                && let Some(old) = &patch.old_path
            {
                actions.push(ApplyFileAction::Remove { path: old.clone() });
            }
        }
    }
    Ok(Some(actions))
}

/// `git apply --whitespace=fix`: rewrite each *added* line to remove the
/// whitespace errors git fixes by default. We implement the "blank-at-eol" fix
/// (strip trailing spaces/tabs), which is the one the materialised content
/// depends on (t4252 "interrupted am --whitespace=fix" drops the space after
/// "Six"). Context and deleted lines are left untouched, so hunk matching is
/// unaffected.
fn am_whitespace_fix_patch(patch: &sley_diff_merge::FilePatch) -> sley_diff_merge::FilePatch {
    use sley::plumbing::sley_diff_merge::HunkLine;
    let mut fixed = patch.clone();
    for hunk in &mut fixed.hunks {
        for line in &mut hunk.lines {
            if let HunkLine::Insert(bytes) = line {
                while matches!(bytes.last(), Some(b' ') | Some(b'\t')) {
                    bytes.pop();
                }
            }
        }
    }
    fixed
}

/// Apply a file patch with `-C<n>` context fuzz (apply.c `apply_one_fragment`):
/// each hunk's full preimage is matched byte-for-byte against the running image,
/// and on failure the begin/end anchors are relaxed and then leading/trailing
/// context lines are dropped — down to the floor `p_context` — until a position
/// matches. Returns `None` if any hunk cannot be placed even after reducing
/// context to the floor (the whole patch then fails, like git).
fn am_apply_context_fuzz(
    base: &[u8],
    patch: &sley_diff_merge::FilePatch,
    p_context: usize,
) -> Option<Vec<u8>> {
    use sley::plumbing::sley_diff_merge::HunkLine;
    if patch.is_delete && patch.hunks.is_empty() {
        return Some(Vec::new());
    }
    let base_for_match: &[u8] = if patch.is_new { b"" } else { base };
    let mut image: Vec<Vec<u8>> = ws_split_lines(base_for_match);
    let mut running_offset: isize = 0;

    for hunk in &patch.hunks {
        // Build the preimage (context + deleted) and postimage (context +
        // inserted), each line carrying its trailing newline so they splice into
        // the line image. Track the leading/trailing context-run lengths git's
        // fuzz loop reduces. Exact matching means a context line in the postimage
        // equals the image line it overwrites, so the patch's bytes can be spliced
        // in directly.
        let mut preimage: Vec<Vec<u8>> = Vec::new();
        let mut postimage: Vec<Vec<u8>> = Vec::new();
        let mut leading = 0usize;
        let mut trailing = 0usize;
        let mut seen_change = false;
        for line in &hunk.lines {
            match line {
                HunkLine::Context(b) => {
                    if !seen_change {
                        leading += 1;
                    }
                    trailing += 1;
                    let mut with_nl = b.clone();
                    with_nl.push(b'\n');
                    preimage.push(with_nl.clone());
                    postimage.push(with_nl);
                }
                HunkLine::Delete(b) => {
                    seen_change = true;
                    trailing = 0;
                    let mut with_nl = b.clone();
                    with_nl.push(b'\n');
                    preimage.push(with_nl);
                }
                HunkLine::Insert(b) => {
                    seen_change = true;
                    trailing = 0;
                    let mut with_nl = b.clone();
                    with_nl.push(b'\n');
                    postimage.push(with_nl);
                }
            }
        }
        // Honour a missing final newline on either side of the hunk.
        if hunk.old_no_newline
            && let Some(last) = preimage.last_mut()
            && last.last() == Some(&b'\n')
        {
            last.pop();
        }
        if hunk.new_no_newline
            && let Some(last) = postimage.last_mut()
            && last.last() == Some(&b'\n')
        {
            last.pop();
        }

        let mut match_beginning = hunk.old_start <= 1;
        let mut match_end = trailing == 0;
        let base_pos: isize = if hunk.new_start > 0 {
            hunk.new_start as isize - 1
        } else {
            0
        };
        let mut pos = base_pos + running_offset;

        let applied_pos = loop {
            if let Some(found) = am_find_pos(&image, &preimage, pos, match_beginning, match_end) {
                break found;
            }
            // At the context floor with no match: reject the hunk (and patch).
            if leading <= p_context && trailing <= p_context {
                return None;
            }
            // Relax the begin/end anchors before reducing context (git's order).
            if match_beginning || match_end {
                match_beginning = false;
                match_end = false;
                continue;
            }
            // Reduce context: drop the larger of leading/trailing (both if equal).
            if leading >= trailing {
                if preimage.is_empty() || postimage.is_empty() {
                    return None;
                }
                preimage.remove(0);
                postimage.remove(0);
                pos -= 1;
                leading -= 1;
            }
            if trailing > leading {
                if preimage.is_empty() || postimage.is_empty() {
                    return None;
                }
                preimage.pop();
                postimage.pop();
                trailing -= 1;
            }
        };

        let pre_len = preimage.len();
        let post_len = postimage.len();
        image.splice(applied_pos..applied_pos + pre_len, postimage);
        running_offset += post_len as isize - pre_len as isize;
    }
    Some(image.concat())
}

/// Find a position where `preimage` matches `image` exactly (apply.c
/// `find_pos`). `match_beginning` forces the file start; `match_end` forces the
/// file end; otherwise the search ping-pongs outward from `pos`.
fn am_find_pos(
    image: &[Vec<u8>],
    preimage: &[Vec<u8>],
    pos: isize,
    match_beginning: bool,
    match_end: bool,
) -> Option<usize> {
    if preimage.len() > image.len() {
        return None;
    }
    let max_start = image.len() - preimage.len();
    let matches = |start: usize| -> bool {
        preimage
            .iter()
            .enumerate()
            .all(|(i, line)| &image[start + i] == line)
    };
    if match_beginning {
        return matches(0).then_some(0);
    }
    if match_end {
        return matches(max_start).then_some(max_start);
    }
    let hint = pos.clamp(0, max_start as isize) as usize;
    for delta in 0..=max_start {
        let forward = hint + delta;
        if forward <= max_start && matches(forward) {
            return Some(forward);
        }
        if delta > 0 && hint >= delta && matches(hint - delta) {
            return Some(hint - delta);
        }
    }
    None
}

/// Apply a file patch with `--ignore-whitespace` (apply.c `ignore_ws_change`):
/// each hunk's preimage is matched against the base using a whitespace-collapsing
/// line comparison, and on a match the matched base region is replaced by the
/// hunk's *new* lines while context lines keep the base's existing whitespace.
/// Returns `None` if any hunk cannot be located even with whitespace ignored.
fn apply_file_patch_ignore_ws(base: &[u8], patch: &sley_diff_merge::FilePatch) -> Option<Vec<u8>> {
    use sley::plumbing::sley_diff_merge::HunkLine;

    if patch.is_delete && patch.hunks.is_empty() {
        return Some(Vec::new());
    }
    let base_for_match: &[u8] = if patch.is_new { b"" } else { base };
    // Lines of the running image, each retaining its trailing `\n` (the last
    // line keeps whatever terminator it had).
    let mut image: Vec<Vec<u8>> = ws_split_lines(base_for_match);

    for hunk in &patch.hunks {
        // preimage = context + deletes (old side, matched fuzzily against image).
        // postimage = context + inserts (new side). For ignore-ws, context lines
        // in the result come from the *image* (target) so its whitespace wins.
        let mut preimage: Vec<&[u8]> = Vec::new();
        // postimage entries: either a literal line (Insert) or a marker pointing
        // at the i-th matched image line (Context, kept verbatim from target).
        enum Post<'a> {
            Context(usize), // index into the matched preimage run
            Insert(&'a [u8]),
        }
        let mut postimage: Vec<Post> = Vec::new();
        let mut pre_idx = 0usize;
        for line in &hunk.lines {
            match line {
                HunkLine::Context(bytes) => {
                    preimage.push(bytes.as_slice());
                    postimage.push(Post::Context(pre_idx));
                    pre_idx += 1;
                }
                HunkLine::Delete(bytes) => {
                    preimage.push(bytes.as_slice());
                    pre_idx += 1;
                }
                HunkLine::Insert(bytes) => {
                    postimage.push(Post::Insert(bytes.as_slice()));
                }
            }
        }

        // Locate the preimage in the image with whitespace-fuzzy line matching.
        let pos = ws_find_preimage(&image, &preimage)?;

        // Build the replacement, taking context lines from the matched image
        // region (so the result keeps the target's whitespace) and inserted
        // lines verbatim from the patch.
        let mut replacement: Vec<Vec<u8>> = Vec::new();
        for post in &postimage {
            match post {
                Post::Context(i) => replacement.push(image[pos + i].clone()),
                Post::Insert(bytes) => {
                    let mut line = bytes.to_vec();
                    line.push(b'\n');
                    replacement.push(line);
                }
            }
        }
        // The hunk's final new line may lack a terminator; honour it.
        if hunk.new_no_newline
            && let Some(last) = replacement.last_mut()
            && last.last() == Some(&b'\n')
        {
            last.pop();
        }
        image.splice(pos..pos + preimage.len(), replacement);
    }

    Some(image.concat())
}

/// Split a blob into lines, each keeping its trailing `\n`. The final line keeps
/// whatever terminator it had (none if the file lacked a trailing newline).
fn ws_split_lines(input: &[u8]) -> Vec<Vec<u8>> {
    let mut lines = Vec::new();
    let mut start = 0usize;
    for (idx, byte) in input.iter().enumerate() {
        if *byte == b'\n' {
            lines.push(input[start..=idx].to_vec());
            start = idx + 1;
        }
    }
    if start < input.len() {
        lines.push(input[start..].to_vec());
    }
    lines
}

/// Find a position in `image` where every preimage line whitespace-fuzzy-matches
/// the corresponding image line (apply.c `line_by_line_fuzzy_match`). Returns the
/// 0-based start index, or `None`.
fn ws_find_preimage(image: &[Vec<u8>], preimage: &[&[u8]]) -> Option<usize> {
    if preimage.is_empty() {
        return Some(0);
    }
    if preimage.len() > image.len() {
        return None;
    }
    'outer: for start in 0..=(image.len() - preimage.len()) {
        for (i, pre) in preimage.iter().enumerate() {
            if !ws_fuzzy_matchlines(&image[start + i], pre) {
                continue 'outer;
            }
        }
        return Some(start);
    }
    None
}

/// Port of apply.c `fuzzy_matchlines`: two lines match if, after ignoring line
/// endings, they are equal once each run of whitespace is collapsed (whitespace
/// must appear on both sides at the same logical position, so "a b" != "ab").
fn ws_fuzzy_matchlines(a: &[u8], b: &[u8]) -> bool {
    let trim_eol = |s: &[u8]| -> usize {
        let mut end = s.len();
        while end > 0 && (s[end - 1] == b'\r' || s[end - 1] == b'\n') {
            end -= 1;
        }
        end
    };
    let a = &a[..trim_eol(a)];
    let b = &b[..trim_eol(b)];
    let (mut i, mut j) = (0usize, 0usize);
    while i < a.len() && j < b.len() {
        if a[i].is_ascii_whitespace() {
            if !b[j].is_ascii_whitespace() {
                return false;
            }
            while i < a.len() && a[i].is_ascii_whitespace() {
                i += 1;
            }
            while j < b.len() && b[j].is_ascii_whitespace() {
                j += 1;
            }
        } else if a[i] != b[j] {
            return false;
        } else {
            i += 1;
            j += 1;
        }
    }
    i == a.len() && j == b.len()
}

fn apply_actions(
    git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
    actions: &[ApplyFileAction],
) -> Result<()> {
    let config = commands::remote::read_repo_config(git_dir).unwrap_or_default();
    // git's `write_out_results` materializes in two phases: every removal happens
    // before any write. This matters when a directory's tracked children are
    // removed and the directory is then (re)created as a gitlink — single-phase
    // ordering would prune the just-emptied directory after the create.
    for action in actions {
        if let ApplyFileAction::Remove { path } = action {
            merge_remove_worktree_file(worktree_root, path)?;
        }
    }
    for action in actions {
        if let ApplyFileAction::Write {
            path,
            mode,
            content,
        } = action
        {
            let worktree_content = if (*mode & 0o170000) == 0o100000 {
                Cow::Owned(sley_worktree::apply_smudge_filter(
                    worktree_root,
                    git_dir,
                    format,
                    &config,
                    path,
                    content,
                )?)
            } else {
                Cow::Borrowed(content.as_slice())
            };
            merge_write_worktree_file(worktree_root, path, &worktree_content, *mode)?;
        }
    }
    Ok(())
}

/// Reconstruct a gitlink patch's preimage (`Subproject commit <old>\n`) from its
/// first hunk's old-side lines, used when the submodule has no readable blob in
/// the working tree (git's `SUBMODULE_PATCH_WITHOUT_INDEX`).
fn am_gitlink_preimage_from_patch(patch: &sley_diff_merge::FilePatch) -> Vec<u8> {
    let mut base = Vec::new();
    if let Some(hunk) = patch.hunks.first() {
        for line in &hunk.lines {
            match line {
                sley_diff_merge::HunkLine::Context(bytes)
                | sley_diff_merge::HunkLine::Delete(bytes) => {
                    base.extend_from_slice(bytes);
                    base.push(b'\n');
                }
                sley_diff_merge::HunkLine::Insert(_) => {}
            }
        }
    }
    base
}

/// Stage the files this patch touched into the index and create the commit,
/// advancing HEAD (or the branch HEAD points at) with an `am` reflog entry.
fn stage_and_commit(
    git_dir: &Path,
    common_git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
    patch: &AmPatch,
    actions: &[ApplyFileAction],
    commit_opts: AmCommitOpts,
    config: &GitConfig,
) -> Result<ObjectId> {
    let db = FileObjectDatabase::from_git_dir(common_git_dir, format);
    let mut index = read_repository_index(git_dir, format)?.unwrap_or(Index {
        version: 2,
        entries: Vec::new(),
        extensions: Vec::new(),
        checksum: None,
    });

    for action in actions {
        match action {
            ApplyFileAction::Write {
                path,
                mode,
                content,
            } => {
                let oid = if sley_index::is_gitlink(*mode) {
                    gitlink_oid_from_subproject_content(format, content)?.ok_or_else(|| {
                        GitError::InvalidFormat(format!(
                            "patch for gitlink {} did not name a subproject commit",
                            String::from_utf8_lossy(path)
                        ))
                    })?
                } else {
                    db.write_object(EncodedObject::new(ObjectType::Blob, content.clone()))?
                };
                upsert_index_entry(&mut index, worktree_root, path, *mode, oid);
            }
            ApplyFileAction::Remove { path } => {
                index.entries.retain(|entry| &entry.path != path);
            }
        }
    }
    index
        .entries
        .sort_by(|left, right| left.path.cmp(&right.path));
    // We mutated entry OIDs above; the cache-tree extension carried over from
    // the parsed index is now stale. `write_tree_from_index` trusts a present
    // cache-tree by entry-count, so leaving a stale `TREE` here makes the
    // commit reuse the OLD root tree (wrong OID; modified-file content lost).
    // Invalidate it, matching every entry-mutating writer in sley-worktree.
    index.set_cache_tree(None)?;
    fs::write(
        sley_worktree::repository_index_path(git_dir),
        index.write(format)?,
    )?;

    create_am_commit(
        git_dir,
        common_git_dir,
        worktree_root,
        format,
        patch,
        commit_opts,
        config,
    )
}

/// Insert or replace the stage-0 index entry for `path`.
///
/// `apply_actions` has already written the file to the worktree, so record its
/// on-disk stat (git stages via add_file_to_index / fill_stat_cache_info); a
/// zeroed stat would make `git diff-files` report the just-applied path as
/// modified. Gitlinks keep zero stat, as git does.
fn upsert_index_entry(
    index: &mut Index,
    worktree_root: &Path,
    path: &[u8],
    mode: u32,
    oid: ObjectId,
) {
    let mut entry = merge_index_entry(path, mode, oid, 0);
    if !sley_index::is_gitlink(mode)
        && let Ok(rel) = std::str::from_utf8(path)
        && let Ok(metadata) = fs::symlink_metadata(worktree_root.join(rel))
    {
        sley_worktree::fill_index_entry_stat_cache(&mut entry, &metadata);
    }
    if let Some(existing) = index
        .entries
        .iter_mut()
        .find(|candidate| candidate.path == path)
    {
        *existing = entry;
    } else {
        index.entries.push(entry);
    }
}

/// Build the commit from the current index tree, using the patch's author
/// identity/date and the environment committer, then advance HEAD.
fn create_am_commit(
    git_dir: &Path,
    common_git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
    patch: &AmPatch,
    commit_opts: AmCommitOpts,
    config: &GitConfig,
) -> Result<ObjectId> {
    let refs = FileRefStore::new(git_dir, format);
    let head_oid = head_commit_oid(&refs)?;
    let tree = sley_worktree::write_tree_from_index(git_dir, format)?;

    let target_encoding = commit_encoding_config(git_dir);
    let (author, committer) = am_commit_identities(patch, commit_opts, &target_encoding, config)?;
    // The message has already been finalised (Message-ID appended + the
    // applypatch-msg hook run) in `prepare_am_commit_message`, BEFORE the patch
    // was applied — git's ordering. Here we only append the sign-off, which git
    // does on `state->msg` just before writing the commit object.
    let mut message = if commit_opts.utf8 {
        log_reencode_message(&patch.message, &patch.message_encoding, &target_encoding).into_owned()
    } else {
        patch.message.clone()
    };
    if commit_opts.signoff {
        message = am_append_signoff(message, &commit_signoff_from_env(config)?);
    }
    if encoding_is_utf8(&target_encoding) && commit_message_has_invalid_utf8(&message) {
        eprintln!("Warning: commit message did not conform to UTF-8.");
    }
    // pre-applypatch runs after staging, before the commit; a failure aborts the
    // run (git exits 1). `--no-verify` skips it.
    if !commit_opts.no_verify
        && commands::hooks::run_hook_at(
            git_dir,
            "pre-applypatch",
            commands::hooks::HookRun::default(),
        )
        .is_err()
    {
        return Err(GitError::Exit(1));
    }

    let mut db = FileObjectDatabase::from_git_dir(common_git_dir, format);
    let parents: Vec<ObjectId> = head_oid.into_iter().collect();
    let encoding = commit_encoding_header_from_config(git_dir);
    let new_oid = sley_sequencer::create_commit(
        &mut db,
        sley_sequencer::CommitCreate {
            tree,
            parents,
            author,
            committer: committer.clone(),
            message,
            encoding,
            signature: None,
        },
    )?;

    let target_ref = match refs.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(branch)) => branch,
        _ => "HEAD".to_string(),
    };
    let old_oid = head_oid.unwrap_or_else(|| ObjectId::null(format));
    // Standalone `git am` writes `am: <subject>`; the rebase apply backend runs
    // am with GIT_REFLOG_ACTION="<action> (pick)" so each commit lands a
    // `<action> (pick): <subject>` entry (builtin/rebase.c run_am + am.c).
    let reflog_message = if commit_opts.rebase_pick_reflog {
        let action = env::var("GIT_REFLOG_ACTION").unwrap_or_else(|_| "rebase".to_string());
        format!("{action} (pick): {}", patch.subject)
    } else {
        format!("am: {}", patch.subject)
    };
    let mut tx = refs.transaction();
    tx.update(RefUpdate {
        name: target_ref,
        expected: head_oid.map(RefTarget::Direct),
        new: RefTarget::Direct(new_oid),
        reflog: Some(ReflogEntry {
            old_oid,
            new_oid,
            committer,
            message: reflog_message.into_bytes(),
        }),
    });
    tx.commit()?;
    // git's `am` applies each patch with `git apply --index`, which updates the
    // index *and* worktree only for the patched paths, then commits the tree —
    // it never resets the whole worktree. `apply_actions`/`stage_and_commit`
    // already wrote the patched files and index; refresh the index stat so the
    // just-applied paths read back clean, while leaving every OTHER worktree
    // file untouched (so a local edit to an unrelated file survives the series,
    // t4151 "am --skip continue after failed am").
    sley_worktree::refresh_index_paths_with_options(
        worktree_root,
        git_dir,
        format,
        &[],
        /* quiet */ true,
        /* ignore_missing */ true,
        /* ignore_submodules */ false,
        /* allow_unmerged */ false,
        /* really_refresh */ false,
    )?;
    // git runs post-applypatch but ignores its exit status — it is purely
    // informational, run after the commit has already landed (builtin/am.c
    // calls `run_hooks` without checking the result). Swallow any failure.
    let _ = commands::hooks::run_hook_at(
        git_dir,
        "post-applypatch",
        commands::hooks::HookRun::default(),
    );
    Ok(new_oid)
}

/// Build the author and committer identity bytes for an am commit, honouring
/// `--ignore-date` and `--committer-date-is-author-date` exactly as builtin/am.c
/// does:
///
///   - author date: the patch's `Date:`; with `--ignore-date`, the current time
///     (git passes `NULL` to `fmt_ident`, which uses "now").
///   - committer: normally the environment committer (name/email/date the same
///     way `git commit` resolves them). With `--committer-date-is-author-date`,
///     the committer *date* is set to the author date (or "now" under
///     `--ignore-date`), keeping the committer name/email from the environment.
fn am_commit_identities(
    patch: &AmPatch,
    opts: AmCommitOpts,
    target_encoding: &str,
    config: &GitConfig,
) -> Result<(Vec<u8>, Vec<u8>)> {
    // The author date: the patch's Date:, the env author date, or "now".
    let author_date = if opts.ignore_date {
        am_now_date()
    } else {
        patch
            .author_date
            .clone()
            .unwrap_or_else(|| env::var("GIT_AUTHOR_DATE").unwrap_or_else(|_| am_now_date()))
    };
    let author_name =
        log_reencode_message(&patch.author_name, &patch.author_encoding, target_encoding)
            .into_owned();
    let author_email =
        log_reencode_message(&patch.author_email, &patch.author_encoding, target_encoding)
            .into_owned();
    let author =
        sley_sequencer::format_commit_identity_bytes(&author_name, &author_email, &author_date)?;

    let committer = if opts.committer_date_is_author_date {
        commit_identity_from_env_with_date("COMMITTER", &author_date, config)?
    } else {
        commit_identity_from_env("COMMITTER", config)?
    };
    Ok((author, committer))
}

/// The current wall-clock time formatted as git's raw `<seconds> <±HHMM>`. Used
/// when `--ignore-date` discards the patch's `Date:` (git passes `NULL` to
/// `fmt_ident`, which fills in "now"). Mirrors git's behaviour in the t4150
/// `--ignore-date` test, which runs with a `+0000` (UTC) environment.
fn am_now_date() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("@{secs} +0000")
}

/// Append a `Message-ID: <value>` line to the commit message body, matching
/// git's mailinfo: the header value is emitted verbatim as the final body line.
fn am_append_message_id(message: &mut Vec<u8>, message_id: &str) {
    if !message.is_empty() && !message.ends_with(b"\n") {
        message.push(b'\n');
    }
    message.extend_from_slice(b"Message-ID: ");
    message.extend_from_slice(message_id.as_bytes());
    message.push(b'\n');
}

/// Append a `Signed-off-by:` trailer to an am commit message, faithfully
/// mirroring git's `append_signoff(msgbuf, 0, 0)` as called from
/// `am_append_signoff` in builtin/am.c. Two behaviours distinguish this from
/// `commit -s` (which passes `APPEND_SIGNOFF_DEDUP`):
///
///   1. **No de-duplication.** `git am --signoff` always appends the trailer
///      even when an identical `Signed-off-by:` already appears earlier in the
///      message (t4150 cells "duplicates Signed-off-by: if it is not the last
///      one" / "adds Signed-off-by: if another author is preset").
///   2. **Conforming-footer-aware blank line.** A blank line is inserted before
///      the trailer only when the message does NOT already end with a trailer
///      block; when the last paragraph is a conforming footer (e.g. ends with a
///      `Reported-by:`/`Signed-off-by:` line), the new trailer is appended
///      directly, with no separating blank line.
fn am_append_signoff(mut message: Vec<u8>, signoff: &[u8]) -> Vec<u8> {
    // `signoff` is the full "Signed-off-by: name <email>" line (no newline).
    let mut sob = signoff.to_vec();
    sob.push(b'\n');

    // git's strbuf_complete_line: ensure the buffer ends with a newline.
    if !message.is_empty() && !message.ends_with(b"\n") {
        message.push(b'\n');
    }

    // git's append_signoff: if the whole buffer equals the sob, has_footer is 3;
    // otherwise classify the trailing trailer block (0/1/2/3).
    let has_footer = if message == sob {
        3
    } else {
        am_conforming_footer_state(&message, &sob)
    };

    if has_footer == 0 {
        let len = message.len();
        if len == 0 {
            message.extend_from_slice(b"\n\n");
        } else if len == 1 {
            // Buffer is a single newline.
            message.push(b'\n');
        } else if message[len - 2] != b'\n' {
            // Buffer ends with a single newline; add another for the blank line.
            message.push(b'\n');
        } // else already ends with two newlines.
    }

    // builtin/am.c passes flag 0 (no DEDUP), so git's gate reduces to
    // `has_footer != 3`: append unless the sob is already the LAST trailer.
    if has_footer != 3 {
        message.extend_from_slice(&sob);
    }
    message
}

/// Port of git's `has_conforming_footer` 3-state result for the final paragraph
/// of `message`, given the target `sob` line (with trailing newline):
///   0 — the last paragraph is not a conforming trailer block;
///   1 — it is a trailer block, but contains no line equal to `sob`;
///   2 — it contains `sob`, but `sob` is not the last trailer;
///   3 — `sob` is the last trailer line.
fn am_conforming_footer_state(message: &[u8], sob: &[u8]) -> u8 {
    let text = String::from_utf8_lossy(message);
    let trimmed = text.trim_end_matches('\n');
    let last_para = match trimmed.rfind("\n\n") {
        Some(pos) => &trimmed[pos + 2..],
        None => trimmed,
    };
    if last_para.is_empty() {
        return 0;
    }
    let lines: Vec<&str> = last_para.lines().collect();
    if !lines.iter().all(|line| is_trailer_line(line)) {
        return 0;
    }
    let sob_line = String::from_utf8_lossy(sob);
    let sob_line = sob_line.trim_end_matches('\n');
    let mut found_sob = None;
    for (i, line) in lines.iter().enumerate() {
        if *line == sob_line {
            found_sob = Some(i);
        }
    }
    match found_sob {
        None => 1,
        Some(i) if i + 1 == lines.len() => 3,
        Some(_) => 2,
    }
}

/// A single trailer line: `Token<sep> value`, where the token is non-empty and
/// contains no whitespace, and the separator is `:` or `#` (git's default
/// trailer separators), optionally followed by whitespace + value. Also accepts
/// the cherry-pick footer line git's trailer parser tolerates.
fn is_trailer_line(line: &str) -> bool {
    if line.starts_with("(cherry picked from commit ") {
        return true;
    }
    let Some(sep) = line.find([':', '#']) else {
        return false;
    };
    let key = &line[..sep];
    if key.is_empty() || key.contains(char::is_whitespace) {
        return false;
    }
    // For a `:` separator git requires the value to be space-separated; `#`
    // (e.g. `Bug #1234`) is also a recognised trailer separator.
    let rest = &line[sep + 1..];
    line.as_bytes()[sep] == b'#' || rest.is_empty() || rest.starts_with(' ')
}

/// Best-effort 3-way application: reconstruct the pre-image from the index's
/// blobs, apply the patch to that to form "theirs", and 3-way merge against the
/// worktree state ("ours"). Reuses the shared tree-merge engine.
#[allow(clippy::too_many_arguments)]
fn apply_three_way(
    config: &GitConfig,
    git_dir: &Path,
    common_git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
    state_dir: &Path,
    number: usize,
    patch: &AmPatch,
    file_patches: &[sley_diff_merge::FilePatch],
    commit_opts: AmCommitOpts,
    quiet: bool,
    lazy_fetch: bool,
) -> Result<ApplyResult> {
    let refs = FileRefStore::new(git_dir, format);
    // The "ours" side of the 3-way is the current HEAD tree, or the empty tree
    // when applying onto an unborn branch (git's `am -3` reconstructs against an
    // empty index there — t4151 "am -3 stops on conflict on unborn branch").
    let head_oid = head_commit_oid(&refs)?;
    let db = FileObjectDatabase::from_git_dir(common_git_dir, format);
    let head_tree = match &head_oid {
        Some(oid) => commit_tree_oid(&db, format, oid)?,
        None => ObjectId::empty_tree(format),
    };
    let ours_map = sley_diff_merge::flatten_tree(&db, format, &head_tree)?;

    // The merge base for each file is the patch's *pre-image* blob, named by the
    // old side of its `index <old>..<new>` line. Looking those blobs up in the
    // object store (the same thing git does for `am -3`) reconstructs a base tree
    // that may differ from HEAD, which is exactly what lets a 3-way merge succeed
    // when straight application failed.
    let index_oids = parse_patch_index_oids(&patch.diff);

    // git's `build_fake_ancestor`: the synthetic merge base and the "theirs" tree
    // contain ONLY the files the patch touches (preimage / postimage), NOT a copy
    // of HEAD. Seeding them from `ours_map` would leave a renamed-away file's new
    // location present in the base, defeating the rename detection merge-recursive
    // relies on (t4153 "--3way overrides --no-3way": file→file2 rename must follow
    // so side1's change lands on file2). Files HEAD has but the patch does not
    // touch appear only in "ours" and the merge keeps them as add-in-ours.
    let mut base_map = MergeTreeMap::new();
    let mut theirs_map = MergeTreeMap::new();
    for file in file_patches {
        let path = file
            .new_path
            .clone()
            .or_else(|| file.old_path.clone())
            .ok_or_else(|| GitError::InvalidFormat("patch missing target path".into()))?;
        let old_path = file.old_path.clone().unwrap_or_else(|| path.clone());

        if file_patch_touches_gitlink(file) {
            let (old_oid, new_oid) = gitlink_oids_from_patch(file, format)?;
            let base_mode = file.old_mode.unwrap_or(0o160000);
            let mode = file.new_mode.or(file.old_mode).unwrap_or(0o160000);
            if file.is_new {
                base_map.remove(&path);
            } else if let Some(old_oid) = old_oid.or_else(|| {
                ours_map
                    .get(&old_path)
                    .or_else(|| ours_map.get(&path))
                    .map(|(_, oid)| *oid)
            }) {
                base_map.insert(old_path.clone(), (base_mode, old_oid));
            }
            if file.is_delete {
                theirs_map.remove(&path);
            } else if let Some(new_oid) = new_oid {
                theirs_map.insert(path.clone(), (mode, new_oid));
                if file.is_rename {
                    theirs_map.remove(&old_path);
                }
            }
            continue;
        }

        let base_bytes = if file.is_new {
            Vec::new()
        } else if let Some(bytes) =
            lookup_patch_base_blob(&db, &index_oids, &path, &old_path, &ours_map, lazy_fetch)?
        {
            bytes
        } else {
            // We cannot reconstruct a base for this path: fail the 3-way.
            eprintln!("error: repository lacks the necessary blob to fall back on 3-way merge.");
            eprintln!("error: Failed to merge in the changes.");
            return Ok(ApplyResult::Conflict);
        };

        // Default modes to the current HEAD entry's mode (or 644) when the patch
        // carries no explicit mode header, so an unchanged mode never looks like
        // a mode conflict to the tree merge.
        let inherited_mode = ours_map
            .get(&old_path)
            .or_else(|| ours_map.get(&path))
            .map(|(mode, _)| *mode)
            .unwrap_or(0o100644);
        match sley_diff_merge::apply_file_patch(&base_bytes, file) {
            sley_diff_merge::ApplyOutcome::Applied(post) => {
                let mode = file.new_mode.or(file.old_mode).unwrap_or(inherited_mode);
                let base_mode = file.old_mode.unwrap_or(inherited_mode);
                if file.is_new {
                    base_map.remove(&path);
                } else {
                    let base_oid =
                        db.write_object(EncodedObject::new(ObjectType::Blob, base_bytes))?;
                    base_map.insert(old_path.clone(), (base_mode, base_oid));
                }
                if file.is_delete {
                    theirs_map.remove(&path);
                } else {
                    let post_oid = db.write_object(EncodedObject::new(ObjectType::Blob, post))?;
                    theirs_map.insert(path.clone(), (mode, post_oid));
                    if file.is_rename {
                        theirs_map.remove(&old_path);
                    }
                }
            }
            sley_diff_merge::ApplyOutcome::Rejected => {
                eprintln!("error: Failed to merge in the changes.");
                return Ok(ApplyResult::Conflict);
            }
        }
    }

    // Report the paths that differ between the reconstructed base and HEAD, the
    // way git's "reconstruct a base tree" step does (`<status>\t<path>`).
    if !quiet {
        print_three_way_base_status(&base_map, &ours_map, &theirs_map);
    }

    if !quiet {
        println!("Falling back to patching base and 3-way merge...");
    }
    // git's apply/am 3-way uses a synthesized base, labelled "constructed fake
    // ancestor" in diff3 conflict markers (builtin/am.c sets o.ancestor). Honour
    // merge.conflictStyle so `-c merge.conflictstyle=diff3` (and rebase --apply)
    // emit the `|||||||` ancestor section.
    let conflict_style = config
        .get("merge", None, "conflictstyle")
        .map(str::to_string)
        .map(|value| match value.as_str() {
            "diff3" | "zdiff3" => sley_diff_merge::ConflictStyle::Diff3,
            _ => sley_diff_merge::ConflictStyle::Merge,
        })
        .unwrap_or(sley_diff_merge::ConflictStyle::Merge);
    let marker_attrs = vec![b"conflict-marker-size".to_vec()];
    let path_marker_size = |path: &[u8]| {
        am_conflict_marker_size_for_path(git_dir, worktree_root, format, path, &marker_attrs)
    };
    let (results, conflicts, _info) =
        commands::merge_rebase::three_way_merge_trees_inner_with_info_opts_and_path_resolvers(
            &db,
            format,
            &base_map,
            &ours_map,
            &theirs_map,
            "HEAD",
            &patch.subject,
            "constructed fake ancestor",
            sley_diff_merge::MergeFavor::None,
            conflict_style,
            sley_diff_merge::WsIgnore::EMPTY,
            commands::merge_rebase::RenameMergeConfig {
                detect_renames: true,
                rename_threshold: sley_diff_merge::DEFAULT_RENAME_THRESHOLD,
                rename_limit: commands::merge_rebase::merge_rename_limit_config(config),
                // The apply/am 3-way backend uses Git's constructed-fake-
                // ancestor merge path, which does not perform implicit
                // directory renames (the upstream suite records positive
                // directory-rename handling here as a known breakage).  File
                // renames still participate normally; only the directory-level
                // inference must be disabled so an unrelated x/a and x/b are
                // not spuriously moved when the patch renames x/c to y/c.
                directory_renames: sley_diff_merge::DirectoryRenames::False,
                lazy_fetch,
            },
            None,
            Some(&path_marker_size),
        )?;

    // git's merge refuses to clobber untracked working-tree files: a path the
    // merge would create that is absent from "ours" (HEAD) but present in the
    // working tree as an untracked non-directory aborts the whole merge before
    // anything is written. This is what makes "replace submodule with a directory
    // must fail" and the untracked-file-in-the-way cases fail like git. Scoped to
    // submodule batches so plain `am -3` keeps its existing behaviour.
    if file_patches.iter().any(file_patch_touches_gitlink) {
        let mut overwritten: Vec<&Vec<u8>> = Vec::new();
        for (path, result) in &results {
            let writes = matches!(
                result,
                MergePathResult::Resolved(Some(_))
                    | MergePathResult::Conflict {
                        worktree: Some(_),
                        ..
                    }
            );
            if !writes || ours_map.contains_key(path) {
                continue;
            }
            if let Ok(rel) = std::str::from_utf8(path)
                && std::fs::symlink_metadata(worktree_root.join(rel))
                    .is_ok_and(|meta| !meta.is_dir())
            {
                overwritten.push(path);
            }
        }
        if !overwritten.is_empty() {
            eprintln!(
                "error: The following untracked working tree files would be overwritten by merge:"
            );
            for path in &overwritten {
                eprintln!("\t{}", String::from_utf8_lossy(path));
            }
            eprintln!("Please move or remove them before you merge.");
            eprintln!("Aborting");
            eprintln!("error: Failed to merge in the changes.");
            return Ok(ApplyResult::Conflict);
        }
    }

    // git prints "Auto-merging <path>" for every file changed on both sides.
    if !quiet {
        for path in three_way_auto_merged_paths(&base_map, &ours_map, &theirs_map) {
            println!("Auto-merging {}", String::from_utf8_lossy(&path));
        }
    }

    write_merge_index_and_worktree(
        git_dir,
        worktree_root,
        format,
        &db,
        &ours_map,
        &results,
        lazy_fetch,
    )?;

    if conflicts.is_empty() {
        // A successful constructed-ancestor merge can resolve exactly to the
        // current HEAD tree when the patch's change is already present. Git
        // treats that as an already-applied patch and advances the mailbox
        // without manufacturing an empty commit.
        if let Some(head_oid) = head_oid.as_ref()
            && !am_index_is_dirty(git_dir, common_git_dir, format, head_oid)?
        {
            if !quiet {
                println!("No changes -- Patch already applied.");
            }
            record_rebase_rewrite(state_dir, format, number, head_oid)?;
            return Ok(ApplyResult::Skipped);
        }
        let new_oid = create_am_commit(
            git_dir,
            common_git_dir,
            worktree_root,
            format,
            patch,
            commit_opts,
            config,
        )?;
        record_rebase_rewrite(state_dir, format, number, &new_oid)?;
        Ok(ApplyResult::Committed)
    } else {
        for path in &conflicts {
            println!(
                "CONFLICT (content): Merge conflict in {}",
                String::from_utf8_lossy(path)
            );
        }
        // git's `fall_back_threeway` runs rerere on the conflicted result: it
        // records the preimage and, when a matching resolution was recorded
        // earlier, replays it into the worktree (t4150 "am -3 works with
        // rerere"). A no-op unless rerere.enabled.
        commands::rerere::repo_rerere(
            git_dir,
            worktree_root,
            format,
            read_am_rerere_autoupdate(state_dir),
        )?;
        eprintln!("error: Failed to merge in the changes.");
        Ok(ApplyResult::Conflict)
    }
}

/// Print the `<status>\t<path>` lines git emits while reconstructing the base
/// tree for a 3-way merge: `A` added, `D` deleted, `M` modified relative to the
/// reconstructed base.
fn print_three_way_base_status(
    base_map: &MergeTreeMap,
    ours_map: &MergeTreeMap,
    theirs_map: &MergeTreeMap,
) {
    // git shows the diff that reconstructs the (fake-ancestor) base from HEAD,
    // restricted to the paths the patch touches — i.e. the keys of the fake
    // ancestor's base/their trees. The direction is ours→base, so a file HEAD no
    // longer has (renamed away) shows as `A` while a modified file shows `M`.
    let mut paths: BTreeSet<&Vec<u8>> = BTreeSet::new();
    paths.extend(base_map.keys());
    paths.extend(theirs_map.keys());
    for path in paths {
        let status = match (ours_map.get(path), base_map.get(path)) {
            (Some(ours), Some(base)) if ours != base => Some('M'),
            (None, Some(_)) => Some('A'),
            (Some(_), None) => Some('D'),
            _ => None,
        };
        if let Some(status) = status {
            println!("{status}\t{}", String::from_utf8_lossy(path));
        }
    }
}

/// Paths changed on both sides of the merge (base→ours and base→theirs both
/// differ) — the files git announces with "Auto-merging".
fn three_way_auto_merged_paths(
    base_map: &MergeTreeMap,
    ours_map: &MergeTreeMap,
    theirs_map: &MergeTreeMap,
) -> Vec<Vec<u8>> {
    let mut paths: BTreeSet<&Vec<u8>> = BTreeSet::new();
    paths.extend(base_map.keys());
    paths.extend(ours_map.keys());
    paths.extend(theirs_map.keys());
    paths
        .into_iter()
        .filter(|path| {
            base_map.get(*path) != ours_map.get(*path)
                && base_map.get(*path) != theirs_map.get(*path)
        })
        .cloned()
        .collect()
}

/// Map each touched path to the abbreviated old-blob OID from its
/// `index <old>..<new>` header line, keyed by the `b/` (new) path. Used by the
/// 3-way fallback to find the patch's pre-image blob in the object store.
fn parse_patch_index_oids(diff: &[u8]) -> BTreeMap<Vec<u8>, String> {
    let mut map = BTreeMap::new();
    let mut current_path: Option<Vec<u8>> = None;
    for line in split_keep_newline(diff) {
        let line = trim_trailing_newline(&line);
        if let Some(rest) = line.strip_prefix(b"diff --git ") {
            current_path = parse_diff_git_new_path(rest);
        } else if let Some(rest) = line.strip_prefix(b"index ") {
            let text = String::from_utf8_lossy(rest);
            if let Some(path) = current_path.clone()
                && let Some((old, _)) = text.split_once("..")
                && !old.trim().is_empty()
                && old.trim().bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                map.insert(path, old.trim().to_string());
            }
        }
    }
    map
}

fn file_patch_touches_gitlink(file: &sley_diff_merge::FilePatch) -> bool {
    file.old_mode.is_some_and(sley_index::is_gitlink)
        || file.new_mode.is_some_and(sley_index::is_gitlink)
        || file.hunks.iter().any(|hunk| {
            hunk.lines.iter().any(|line| match line {
                sley_diff_merge::HunkLine::Context(bytes)
                | sley_diff_merge::HunkLine::Delete(bytes)
                | sley_diff_merge::HunkLine::Insert(bytes) => {
                    bytes.starts_with(b"Subproject commit ")
                }
            })
        })
}

fn gitlink_oids_from_patch(
    file: &sley_diff_merge::FilePatch,
    format: ObjectFormat,
) -> Result<(Option<ObjectId>, Option<ObjectId>)> {
    let mut old_oid = None;
    let mut new_oid = None;
    for hunk in &file.hunks {
        for line in &hunk.lines {
            match line {
                sley_diff_merge::HunkLine::Delete(bytes) => {
                    old_oid = gitlink_oid_from_subproject_content(format, bytes)?;
                }
                sley_diff_merge::HunkLine::Insert(bytes) => {
                    new_oid = gitlink_oid_from_subproject_content(format, bytes)?;
                }
                sley_diff_merge::HunkLine::Context(bytes) => {
                    let oid = gitlink_oid_from_subproject_content(format, bytes)?;
                    old_oid = old_oid.or(oid);
                    new_oid = new_oid.or(oid);
                }
            }
        }
    }
    Ok((old_oid, new_oid))
}

fn gitlink_oid_from_subproject_content(
    format: ObjectFormat,
    content: &[u8],
) -> Result<Option<ObjectId>> {
    let Some(rest) = content.strip_prefix(b"Subproject commit ") else {
        return Ok(None);
    };
    let text = String::from_utf8_lossy(rest);
    let hex = text.split_whitespace().next().unwrap_or_default().trim();
    if hex.is_empty() {
        return Ok(None);
    }
    Ok(Some(ObjectId::from_hex(format, hex)?))
}

/// Extract the new-side path from a `diff --git <old> <new>` line. The usual
/// form carries `a/<path> b/<path>` prefixes, but a `--no-prefix` patch emits
/// `diff --git <path> <path>` (identical, unprefixed paths) — handle both.
fn parse_diff_git_new_path(rest: &[u8]) -> Option<Vec<u8>> {
    let text = String::from_utf8_lossy(rest);
    // Prefixed form: the new side begins at the last " b/" occurrence (paths may
    // contain spaces but format-patch emits unquoted `a/… b/…` for ordinary
    // names).
    if let Some(marker) = text.rfind(" b/") {
        return Some(text[marker + 3..].as_bytes().to_vec());
    }
    // No-prefix form: `diff --git <path> <path>` with the same path twice. The
    // separator is the exact midpoint space, so the two halves are byte-equal
    // (this disambiguates paths that themselves contain spaces).
    let trimmed = text.trim_end_matches(['\r', '\n']);
    if trimmed.len() >= 3 && trimmed.len() % 2 == 1 {
        let mid = trimmed.len() / 2;
        if trimmed.as_bytes()[mid] == b' ' {
            let (left, right) = (&trimmed[..mid], &trimmed[mid + 1..]);
            if !left.is_empty() && left == right {
                return Some(right.as_bytes().to_vec());
            }
        }
    }
    None
}

/// Read the pre-image blob for `path`: resolve the patch's recorded old OID in
/// the object store, falling back to HEAD's blob for the path. Returns `None`
/// when neither source can supply the base content.
fn lookup_patch_base_blob(
    db: &FileObjectDatabase,
    index_oids: &BTreeMap<Vec<u8>, String>,
    path: &[u8],
    old_path: &[u8],
    ours_map: &MergeTreeMap,
    lazy_fetch: bool,
) -> Result<Option<Vec<u8>>> {
    if let Some(prefix) = index_oids.get(path)
        && let Ok(ObjectPrefixResolution::Unique(oid)) = db.resolve_prefix(prefix)
    {
        let object = db.read_object(&oid)?;
        if object.object_type == ObjectType::Blob {
            return Ok(Some(object.body.clone()));
        }
    }
    if let Some((_, oid)) = ours_map.get(old_path).or_else(|| ours_map.get(path)) {
        return Ok(Some(merge_read_blob(db, oid, lazy_fetch)?));
    }
    Ok(None)
}

/// Materialise a 3-way merge result into the index (with conflict stages) and
/// the worktree (with conflict markers for unresolved paths).
fn write_merge_index_and_worktree(
    git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    ours_map: &MergeTreeMap,
    results: &BTreeMap<Vec<u8>, MergePathResult>,
    lazy_fetch: bool,
) -> Result<()> {
    // Materialize the worktree BEFORE building the index so resolved stage-0
    // entries can record the on-disk stat (git refreshes merged results via
    // fill_stat_cache_info; a zeroed stat makes diff-files report them dirty).
    for (path, result) in results {
        match result {
            MergePathResult::Resolved(Some((mode, oid))) => {
                if ours_map.get(path) != Some(&(*mode, *oid)) {
                    let content = if sley_index::is_gitlink(*mode) {
                        Vec::new()
                    } else {
                        merge_read_blob(db, oid, lazy_fetch)?
                    };
                    merge_write_worktree_file(worktree_root, path, &content, *mode)?;
                }
            }
            MergePathResult::Resolved(None) => merge_remove_worktree_file(worktree_root, path)?,
            MergePathResult::Conflict { worktree, .. } => match worktree {
                Some((mode, content)) => {
                    merge_write_worktree_file(worktree_root, path, content, *mode)?
                }
                None => merge_remove_worktree_file(worktree_root, path)?,
            },
        }
    }

    let mut entries = Vec::new();
    for (path, result) in results {
        match result {
            MergePathResult::Resolved(Some((mode, oid))) => {
                let mut entry = merge_index_entry(path, *mode, *oid, 0);
                if !sley_index::is_gitlink(*mode)
                    && let Ok(rel) = std::str::from_utf8(path)
                    && let Ok(metadata) = fs::symlink_metadata(worktree_root.join(rel))
                {
                    sley_worktree::fill_index_entry_stat_cache(&mut entry, &metadata);
                }
                entries.push(entry);
            }
            MergePathResult::Resolved(None) => {}
            MergePathResult::Conflict {
                base, ours, theirs, ..
            } => {
                if let Some((mode, oid)) = base {
                    entries.push(merge_index_entry(path, *mode, *oid, 1));
                }
                if let Some((mode, oid)) = ours {
                    entries.push(merge_index_entry(path, *mode, *oid, 2));
                }
                if let Some((mode, oid)) = theirs {
                    entries.push(merge_index_entry(path, *mode, *oid, 3));
                }
            }
        }
    }
    entries.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| (left.flags >> 12).cmp(&(right.flags >> 12)))
    });
    let index = Index {
        version: 2,
        entries,
        extensions: Vec::new(),
        checksum: None,
    };
    fs::write(
        sley_worktree::repository_index_path(git_dir),
        index.write(format)?,
    )?;
    Ok(())
}

const AM_DEFAULT_CONFLICT_MARKER_SIZE: usize = 7;

fn am_conflict_marker_size_for_path(
    git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
    path: &[u8],
    requested: &[Vec<u8>],
) -> usize {
    let state = sley_worktree::standard_attributes_for_path_from_index(
        worktree_root,
        git_dir,
        format,
        path,
        requested,
        false,
    )
    .ok()
    .and_then(|checks| checks.into_iter().next().and_then(|check| check.state));
    am_conflict_marker_size_from_attr(state.as_ref())
}

fn am_conflict_marker_size_from_attr(state: Option<&sley_worktree::AttributeState>) -> usize {
    let Some(sley_worktree::AttributeState::Value(value)) = state else {
        return AM_DEFAULT_CONFLICT_MARKER_SIZE;
    };
    let raw = String::from_utf8_lossy(value);
    match raw.parse::<isize>() {
        Ok(size) if size > 0 => size as usize,
        _ => {
            eprintln!("warning: invalid marker-size '{raw}', expecting an integer");
            AM_DEFAULT_CONFLICT_MARKER_SIZE
        }
    }
}

fn am_print_conflict_hints() {
    eprintln!("hint: Use 'git am --show-current-patch=diff' to see the failed patch");
    eprintln!("hint: When you have resolved this problem, run \"git am --continue\".");
    eprintln!("hint: If you prefer to skip this patch, run \"git am --skip\" instead.");
    eprintln!("hint: To restore the original branch and stop patching, run \"git am --abort\".");
    eprintln!("hint: Disable this message with \"git config set advice.mergeConflict false\"");
}

/// The hint block git's `die_user_resolve` prints to stderr when `am
/// --continue`/`--resolved` refuses (no staged changes, or unmerged paths).
/// Same lines as the conflict hints minus the "--show-current-patch" pointer.
fn am_print_resolve_hints() {
    eprintln!("hint: When you have resolved this problem, run \"git am --continue\".");
    eprintln!("hint: If you prefer to skip this patch, run \"git am --skip\" instead.");
    eprintln!("hint: To restore the original branch and stop patching, run \"git am --abort\".");
    eprintln!("hint: Disable this message with \"git config set advice.mergeConflict false\"");
}

fn am_print_empty_patch_hints() {
    eprintln!("hint: When you have resolved this problem, run \"git am --continue\".");
    eprintln!("hint: If you prefer to skip this patch, run \"git am --skip\" instead.");
    eprintln!("hint: To record the empty patch as an empty commit, run \"git am --allow-empty\".");
    eprintln!("hint: To restore the original branch and stop patching, run \"git am --abort\".");
    eprintln!("hint: Disable this message with \"git config set advice.mergeConflict false\"");
}

/// Render the state directory path the way git reports it in the
/// "previous rebase directory … still exists" error: relative to the worktree
/// root when possible (`.git/rebase-apply`), else the absolute path.
fn display_state_dir(worktree_root: &Path, state_dir: &Path) -> String {
    match state_dir.strip_prefix(worktree_root) {
        Ok(relative) => relative.display().to_string(),
        Err(_) => state_dir.display().to_string(),
    }
}

/// Remove the state directory after the last patch lands successfully. When the
/// state dir carries the rebase markers (`head-name`, `onto`, `orig-head`) this
/// was a `git rebase --apply`, so we first return HEAD to the original branch and
/// print the rebase success line before dropping state.
fn finish_am(
    git_dir: &Path,
    common_git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
    state_dir: &Path,
    config: &GitConfig,
    lazy_fetch: bool,
) -> Result<()> {
    if state_dir.join("head-name").exists() {
        finish_rebase_apply(
            git_dir,
            common_git_dir,
            worktree_root,
            format,
            state_dir,
            config,
            lazy_fetch,
        )?;
    }
    if state_dir.exists() {
        fs::remove_dir_all(state_dir)?;
    }
    Ok(())
}

/// Append `<orig> <new>` to `rebase-apply/rewritten` for the patch at index
/// `number`, mirroring `do_commit` in builtin/am.c (which writes the pair off
/// `state->orig_commit` whenever `state->rebasing`). The original commit is read
/// from the parallel `orig-commits` file written at `start_rebase_apply`; a bare
/// `git am` has no such file (and no `rebasing` marker), so this is a no-op there.
fn record_rebase_rewrite(
    state_dir: &Path,
    format: ObjectFormat,
    number: usize,
    new_oid: &ObjectId,
) -> Result<()> {
    if !state_dir.join("rebasing").exists() {
        return Ok(());
    }
    let orig_commits = match fs::read_to_string(state_dir.join("orig-commits")) {
        Ok(text) => text,
        Err(_) => return Ok(()),
    };
    // `number` is 1-based (the patch file is `{number:04}`); the orig-commits file
    // lists one original sha per line in the same order.
    let Some(line) = orig_commits.lines().nth(number.saturating_sub(1)) else {
        return Ok(());
    };
    let Ok(orig) = ObjectId::from_hex(format, line.trim()) else {
        return Ok(());
    };
    let mut rewritten = fs::read_to_string(state_dir.join("rewritten")).unwrap_or_default();
    rewritten.push_str(&format!("{orig} {new_oid}\n"));
    fs::write(state_dir.join("rewritten"), rewritten)?;
    Ok(())
}

/// Run the `post-rewrite` hook with arg `rebase`, feeding the accumulated
/// `rebase-apply/rewritten` (`<old> <new>` per rewritten commit) on stdin —
/// git's `run_post_rewrite_hook` in builtin/am.c, fired once the series finishes.
/// A no-op when nothing was rewritten (an all-skipped or noop run).
fn run_apply_post_rewrite_hook(git_dir: &Path, state_dir: &Path) {
    let input = fs::read(state_dir.join("rewritten")).unwrap_or_default();
    if input.is_empty() {
        return;
    }
    let _ = commands::hooks::run_hook_at(
        git_dir,
        "post-rewrite",
        commands::hooks::HookRun {
            args: vec!["rebase".to_string()],
            stdin: Some(input),
            ..commands::hooks::HookRun::default()
        },
    );
}

/// Move HEAD back to the original branch at the rebased tip and print the rebase
/// success line, mirroring `git-rebase--am`'s `move_to_original_branch` + the
/// "Successfully rebased and updated <branch>." message.
fn finish_rebase_apply(
    git_dir: &Path,
    common_git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
    state_dir: &Path,
    config: &GitConfig,
    lazy_fetch: bool,
) -> Result<()> {
    // git fires the post-rewrite hook at the end of the am loop (am.c:1928),
    // BEFORE the rebase caller moves HEAD back to the original branch. Match that
    // order: feed the accumulated `<old> <new>` map while the state dir still
    // exists (the caller removes it after this returns).
    run_apply_post_rewrite_hook(git_dir, state_dir);
    let refs = FileRefStore::new(git_dir, format);
    let head = head_commit_oid(&refs)?
        .ok_or_else(|| GitError::Command("rebase --apply: cannot read HEAD".into()))?;
    let head_name = fs::read_to_string(state_dir.join("head-name"))
        .unwrap_or_default()
        .trim()
        .to_string();
    let orig_head = fs::read_to_string(state_dir.join("orig-head"))
        .ok()
        .and_then(|raw| ObjectId::from_hex(format, raw.trim()).ok())
        .unwrap_or(head);
    let onto = fs::read_to_string(state_dir.join("onto"))
        .ok()
        .and_then(|raw| ObjectId::from_hex(format, raw.trim()).ok())
        .unwrap_or(head);
    let quiet = read_state_bool(state_dir, "quiet");

    let reflog_action = env::var("GIT_REFLOG_ACTION").unwrap_or_else(|_| "rebase".to_string());
    let head_display = if head_name.starts_with("refs/heads/") {
        let committer = commit_identity_from_env("COMMITTER", config)?;
        let mut tx = refs.transaction();
        tx.update(RefUpdate {
            name: head_name.clone(),
            expected: None,
            new: RefTarget::Direct(head),
            reflog: Some(ReflogEntry {
                old_oid: orig_head,
                new_oid: head,
                committer: committer.clone(),
                message: format!("{reflog_action} (finish): {head_name} onto {onto}").into_bytes(),
            }),
        });
        tx.update(RefUpdate {
            name: "HEAD".into(),
            expected: None,
            new: RefTarget::Symbolic(head_name.clone()),
            reflog: Some(ReflogEntry {
                old_oid: head,
                new_oid: head,
                committer,
                message: format!("{reflog_action} (finish): returning to {head_name}").into_bytes(),
            }),
        });
        tx.commit()?;
        // git's apply backend reports the FULL ref name here
        // (`refs/heads/<branch>`), not the short branch.
        head_name.clone()
    } else {
        "detached HEAD".to_string()
    };

    // Restore any autostash. git's apply backend (git am) prints "Applied
    // autostash." but — unlike the sequencer/merge backend — does NOT print a
    // "Successfully rebased and updated" line, so we don't emit one here. The
    // apply backend records its autostash in `rebase-apply/autostash`; the state
    // dir is removed by the caller's finish, so consume the file before then.
    let _ = head_display;
    apply_rebase_autostash(&common_git_dir, worktree_root, state_dir, lazy_fetch)?;

    Ok(())
}

/// Apply (or store) the autostash recorded in the apply backend's
/// `rebase-apply/autostash`. Mirrors `apply_autostash` in the merge backend
/// (rebase.rs) but reachable from the am finish path. Prints "Applied
/// autostash." on a clean apply, or stores the stash on conflict.
fn apply_rebase_autostash(
    common_git_dir: &Path,
    worktree_root: &Path,
    state_dir: &Path,
    lazy_fetch: bool,
) -> Result<()> {
    let autostash_path = state_dir.join("autostash");
    if let Ok(text) = fs::read_to_string(&autostash_path) {
        let _ = fs::remove_file(&autostash_path);
        let format = repository_object_format(common_git_dir)?;
        if let Ok(oid) = ObjectId::from_hex(format, text.trim()) {
            let applied = commands::stash::apply_stash_commit_quietly_at(
                common_git_dir,
                worktree_root,
                &oid,
                lazy_fetch,
            )
            .unwrap_or(false);
            if applied {
                eprintln!("Applied autostash.");
            } else if commands::stash::store_stash_commit_at(common_git_dir, &oid, "autostash")
                .is_ok()
            {
                print_rebase_autostash_conflict_advice();
            }
        }
    }
    Ok(())
}

fn print_rebase_autostash_conflict_advice() {
    eprintln!("Your local changes are stashed, however applying them");
    eprintln!("resulted in conflicts.  You can either resolve the conflicts");
    eprintln!("and then discard the stash with \"git stash drop\", or, if you");
    eprintln!("do not want to resolve them now, run \"git reset --hard\" and");
    eprintln!("apply the local changes later by running \"git stash pop\".");
}

// ===========================================================================
// Resume sub-operations
// ===========================================================================

fn am_require_in_progress(state_dir: &Path) -> Result<()> {
    if !state_dir.exists() {
        eprintln!("fatal: Resolve operation not in progress, we are not resuming.");
        return Err(GitError::Exit(128));
    }
    Ok(())
}

/// Stage (0–3) of an index entry, read from the on-disk flag bits.
fn am_entry_stage(entry: &IndexEntry) -> u8 {
    ((entry.flags >> 12) & 0x3) as u8
}

/// Worktree path bytes → a relative `PathBuf`.
fn am_bytes_to_pathbuf(bytes: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
    }
}

/// Relative `Path` → Git's slash-separated index bytes.
fn am_pathbuf_to_bytes(path: &Path) -> Vec<u8> {
    let mut bytes = Vec::new();
    for component in path.components() {
        let std::path::Component::Normal(name) = component else {
            continue;
        };
        if !bytes.is_empty() {
            bytes.push(b'/');
        }
        bytes.extend_from_slice(name.as_encoded_bytes());
    }
    bytes
}

/// Path → (mode, oid) leaf map for a commit's tree (empty for an unborn HEAD).
fn am_commit_leaf_map(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    commit: Option<&ObjectId>,
) -> Result<std::collections::BTreeMap<Vec<u8>, (u32, ObjectId)>> {
    let mut map = std::collections::BTreeMap::new();
    if let Some(commit) = commit {
        let tree = commit_tree_oid(db, format, commit)?;
        let index = sley_worktree::index_from_tree(db, format, &tree)?;
        for entry in index.entries {
            map.insert(entry.path.as_bytes().to_vec(), (entry.mode, entry.oid));
        }
    }
    Ok(map)
}

/// Remove the worktree file at `rel` and prune any parent directories left
/// empty, mirroring git's worktree update. A directory in the way (or any other
/// error) is left intact — removal is best-effort.
fn am_remove_worktree_path(worktree_root: &Path, rel: &[u8]) -> Result<()> {
    let path = worktree_root.join(am_bytes_to_pathbuf(rel));
    match fs::remove_file(&path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Ok(()),
    }
    let mut parent = path.parent();
    while let Some(dir) = parent {
        if dir == worktree_root || fs::remove_dir(dir).is_err() {
            break;
        }
        parent = dir.parent();
    }
    Ok(())
}

/// git's `clean_index(curr_head, orig_head)` (builtin/am.c): restore the index
/// and worktree from a partial-apply state back to `orig_head`. Only the paths
/// the apply *touched* (unmerged, or staged away from `curr_head`) and the paths
/// that differ between the two trees are rewritten; every other worktree file is
/// left exactly as-is, so worktree-only modifications to unchanged tracked files
/// and untracked files both survive (t4151 "am --abort cleans relevant files",
/// "am --skip continue after failed am", "leaves index stat info alone").
///
/// `curr_head`/`orig_head` are commit oids, or `None` for an unborn HEAD (the
/// empty tree). Propagates the worktree-write error (e.g. a directory where a
/// restored file must go) so `am --abort` reports a failed exit status (t4151
/// "git am --abort return failed exit status when it fails").
fn am_clean_index(
    git_dir: &Path,
    common_git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
    curr_head: Option<&ObjectId>,
    orig_head: Option<&ObjectId>,
) -> Result<()> {
    let db = FileObjectDatabase::from_git_dir(common_git_dir, format);
    let h_map = am_commit_leaf_map(&db, format, curr_head)?;
    let r_map = am_commit_leaf_map(&db, format, orig_head)?;

    // Current index, split into resolved (stage 0) entries and unmerged paths.
    let index = read_repository_index(git_dir, format)?;
    let mut i0: std::collections::BTreeMap<Vec<u8>, (u32, ObjectId)> =
        std::collections::BTreeMap::new();
    let mut unmerged: std::collections::BTreeSet<Vec<u8>> = std::collections::BTreeSet::new();
    if let Some(index) = &index {
        for entry in &index.entries {
            let path = entry.path.as_bytes().to_vec();
            if am_entry_stage(entry) == 0 {
                i0.insert(path, (entry.mode, entry.oid));
            } else {
                unmerged.insert(path);
            }
        }
    }

    // "Touched" = paths the partial apply changed (unmerged, or index diverged
    // from curr_head) plus paths the rewind itself changes (curr_head vs
    // orig_head). Untouched paths keep their worktree state.
    let mut touched: std::collections::BTreeSet<Vec<u8>> = unmerged;
    for p in i0.keys().chain(h_map.keys()) {
        if i0.get(p) != h_map.get(p) {
            touched.insert(p.clone());
        }
    }
    for p in h_map.keys().chain(r_map.keys()) {
        if h_map.get(p) != r_map.get(p) {
            touched.insert(p.clone());
        }
    }

    if touched.is_empty() {
        // The index already matches orig_head and the worktree is clean for every
        // affected path: leave both (and their cached stat info) untouched.
        return Ok(());
    }

    // Partition the touched paths into restores (present in orig_head) and
    // removals (added by the apply / gone in orig_head).
    let mut checkout_paths: Vec<PathBuf> = Vec::new();
    let mut remove_paths: Vec<&Vec<u8>> = Vec::new();
    for p in &touched {
        if r_map.contains_key(p) {
            checkout_paths.push(am_bytes_to_pathbuf(p));
        } else {
            remove_paths.push(p);
        }
    }

    // D/F precheck (git's `verify_clean_subdirectory`): refuse to restore a
    // tracked file over a directory holding unrelated untracked content, and do
    // so BEFORE touching the index or worktree so `am --abort` fails cleanly
    // with the directory intact (t4151 "return failed exit status when it
    // fails"). Files created by the failed apply and already scheduled for
    // removal are owned by this cleanup, however; they must not make `--skip`
    // reject its own D/F conflict debris (t1015).
    let cleanup_paths = remove_paths
        .iter()
        .map(|path| (*path).clone())
        .collect::<std::collections::BTreeSet<_>>();
    let mut df_conflict = false;
    for path in &checkout_paths {
        let full = worktree_root.join(path);
        if full.is_dir()
            && am_subdirectory_has_unowned_entries(
                &full,
                &am_pathbuf_to_bytes(path),
                &cleanup_paths,
            )?
        {
            eprintln!(
                "error: Updating '{}' would lose untracked files in it",
                path.display()
            );
            df_conflict = true;
        }
    }
    if df_conflict {
        return Err(GitError::Exit(128));
    }

    // The resulting index is exactly orig_head's tree.
    match orig_head {
        Some(orig) => {
            sley_worktree::reset_index_to_commit(worktree_root, git_dir, format, orig)?;
        }
        None => {
            sley_worktree::write_repository_index(
                git_dir,
                format,
                Index {
                    version: 2,
                    entries: Vec::new(),
                    extensions: Vec::new(),
                    checksum: None,
                },
            )?;
        }
    }

    // Worktree: remove the apply-added paths, then rewrite the restores.
    for p in &remove_paths {
        am_remove_worktree_path(worktree_root, p)?;
    }
    if !checkout_paths.is_empty() {
        let config = commands::remote::read_repo_config(git_dir).unwrap_or_default();
        sley_worktree::checkout_index_paths(
            worktree_root,
            git_dir,
            format,
            &checkout_paths,
            sley_worktree::CheckoutIndexPathOptions {
                force: true,
                merge: false,
                overlay: true,
                stage: None,
                conflict_style: sley_worktree::CheckoutConflictStyle::Merge,
                smudge_config: Some(&config),
            },
        )?;
    }
    Ok(())
}

/// Whether a D/F directory contains anything outside the failed apply's cleanup
/// set. Empty directories and files explicitly scheduled for removal are safe;
/// every other leaf is user-owned worktree state and must block replacement.
fn am_subdirectory_has_unowned_entries(
    directory: &Path,
    git_path: &[u8],
    cleanup_paths: &std::collections::BTreeSet<Vec<u8>>,
) -> Result<bool> {
    let mut stack = vec![(directory.to_path_buf(), git_path.to_vec())];
    while let Some((fs_directory, git_directory)) = stack.pop() {
        let entries = match fs::read_dir(&fs_directory) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err.into()),
        };
        for entry in entries {
            let entry = entry?;
            let mut child_git_path = git_directory.clone();
            child_git_path.push(b'/');
            child_git_path.extend_from_slice(entry.file_name().as_encoded_bytes());
            if entry.file_type()?.is_dir() {
                stack.push((entry.path(), child_git_path));
            } else if !cleanup_paths.contains(child_git_path.as_slice()) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// `git am --abort`: restore the branch to where the series started and drop
/// the state directory.
fn am_abort(
    git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
    state_dir: &Path,
    config: &GitConfig,
) -> Result<()> {
    am_require_in_progress(state_dir)?;
    // The rebase apply backend records the branch to return to in `head-name`
    // (a `refs/heads/...` ref when attached, the literal "detached HEAD"
    // otherwise) plus the starting commit in `orig-head`. `git rebase --apply
    // --abort` returns HEAD to where the rebase started; a bare `git am --abort`
    // runs git's `safe_to_abort` + `clean_index` and rewinds to ORIG_HEAD.
    let head_name_raw = fs::read_to_string(state_dir.join("head-name"))
        .ok()
        .map(|raw| raw.trim().to_string());
    let is_rebase = head_name_raw.is_some();

    if is_rebase {
        let rebase_branch = head_name_raw
            .as_deref()
            .filter(|name| name.starts_with("refs/heads/"))
            .map(str::to_string);
        let orig_head = fs::read_to_string(state_dir.join("orig-head"))
            .ok()
            .and_then(|raw| ObjectId::from_hex(format, raw.trim()).ok());
        let safety = fs::read_to_string(state_dir.join("abort-safety")).unwrap_or_default();
        let safety = safety.trim();
        if !safety.is_empty()
            && let Ok(oid) = ObjectId::from_hex(format, safety)
        {
            let refs = FileRefStore::new(git_dir, format);
            let current = head_commit_oid(&refs)?;
            let committer = commit_identity_from_env("COMMITTER", config)?;
            let mut tx = refs.transaction();
            // git builtin/rebase.c abort: `<action> (abort): returning to
            // <head_name OR orig_head_sha>` — the branch ref when attached, the
            // starting commit's hex when the rebase was on a detached HEAD.
            let action = env::var("GIT_REFLOG_ACTION").unwrap_or_else(|_| "rebase".to_string());
            let returning_to = match &rebase_branch {
                Some(branch) => branch.clone(),
                None => orig_head.unwrap_or(oid).to_hex().to_string(),
            };
            let reflog = ReflogEntry {
                old_oid: current.unwrap_or(zero_oid(format)?),
                new_oid: oid,
                committer: committer.clone(),
                message: format!("{action} (abort): returning to {returning_to}").into_bytes(),
            };
            match &rebase_branch {
                Some(branch) => {
                    // The apply backend ran on a DETACHED HEAD (checkout_onto_for_apply
                    // detaches), so the branch ref never moved off orig_head — abort
                    // only re-attaches HEAD to it. Writing the branch ref here would
                    // add a spurious branch-reflog entry; git leaves the branch
                    // reflog untouched (t3406 #25).
                    tx.update(RefUpdate {
                        name: "HEAD".into(),
                        expected: None,
                        new: RefTarget::Symbolic(branch.clone()),
                        reflog: Some(reflog),
                    });
                }
                None => {
                    // Detached rebase: HEAD itself moves back to orig_head.
                    tx.update(RefUpdate {
                        name: "HEAD".into(),
                        expected: None,
                        new: RefTarget::Direct(oid),
                        reflog: Some(reflog),
                    });
                }
            }
            tx.commit()?;
            sley_worktree::reset_index_and_worktree_to_commit(
                worktree_root,
                git_dir,
                format,
                &oid,
            )?;
        }
        // Drop state directly: `finish_am` would re-run the rebase finish (which
        // moves to the rebased tip), but on abort we have already restored
        // orig_head.
        if state_dir.exists() {
            fs::remove_dir_all(state_dir)?;
        }
        return Ok(());
    }

    // Bare `git am --abort` — git's `am_abort` / `safe_to_abort`.
    //
    // A recorded `dirtyindex` means we never started applying (the index was
    // dirty when `am` ran); abort just drops the state and leaves HEAD, index,
    // and worktree exactly as the user left them (t4151 "keep dirty index").
    if state_dir.join("dirtyindex").exists() {
        if state_dir.exists() {
            fs::remove_dir_all(state_dir)?;
        }
        return Ok(());
    }

    let refs = FileRefStore::new(git_dir, format);
    let curr_head = head_commit_oid(&refs)?;
    let safety = fs::read_to_string(state_dir.join("abort-safety")).unwrap_or_default();
    let safety = safety.trim();
    let safety_oid = if safety.is_empty() {
        None
    } else {
        ObjectId::from_hex(format, safety).ok()
    };
    // If HEAD no longer matches the recorded safety point, the user advanced it
    // after the failure: do not rewind (git's safe_to_abort warning). Keep their
    // local commits / dirty index intact and just drop the state.
    if curr_head != safety_oid {
        eprintln!(
            "warning: You seem to have moved HEAD since the last 'am' failure.\n\
             Not rewinding to ORIG_HEAD"
        );
        if state_dir.exists() {
            fs::remove_dir_all(state_dir)?;
        }
        return Ok(());
    }

    // git's `am_abort` clears rerere's merge-resolution metadata once it has
    // decided it is safe to rewind. A no-op unless rerere.enabled.
    commands::rerere::rerere_clear(git_dir)?;

    let common_git_dir = common_git_dir_for_git_dir(git_dir)?;
    let orig_head = fs::read_to_string(state_dir.join("orig-head"))
        .ok()
        .and_then(|raw| ObjectId::from_hex(format, raw.trim()).ok());

    // Restore the index + worktree to ORIG_HEAD BEFORE moving HEAD. A failure
    // here (e.g. a directory where a tracked file must be restored) aborts with
    // a non-zero exit and the state dir intact (t4151 "return failed exit
    // status when it fails").
    if am_clean_index(
        git_dir,
        &common_git_dir,
        worktree_root,
        format,
        curr_head.as_ref(),
        orig_head.as_ref(),
    )
    .is_err()
    {
        // git's `am_abort`: `if (clean_index(...)) die("failed to clean index")`.
        // HEAD is not moved and the state dir is left in place.
        eprintln!("fatal: failed to clean index");
        return Err(GitError::Exit(128));
    }

    match &orig_head {
        Some(orig) => {
            let committer = commit_identity_from_env("COMMITTER", config)?;
            let target_ref = match refs.read_ref("HEAD")? {
                Some(RefTarget::Symbolic(branch)) => branch,
                _ => "HEAD".to_string(),
            };
            let mut tx = refs.transaction();
            tx.update(RefUpdate {
                name: target_ref,
                expected: None,
                new: RefTarget::Direct(*orig),
                reflog: Some(ReflogEntry {
                    old_oid: curr_head.unwrap_or(zero_oid(format)?),
                    new_oid: *orig,
                    committer,
                    message: b"am --abort".to_vec(),
                }),
            });
            tx.commit()?;
        }
        None => {
            // The series started on an unborn branch (no ORIG_HEAD). If `am`
            // created the first commit before stopping, the branch is now born;
            // delete it so HEAD returns to its unborn state (git's
            // `delete_ref(curr_branch)`), leaving the symbolic HEAD intact.
            if curr_head.is_some()
                && let Some(RefTarget::Symbolic(branch)) = refs.read_ref("HEAD")?
            {
                let _ = refs.delete_ref(&branch);
            }
        }
    }

    if state_dir.exists() {
        fs::remove_dir_all(state_dir)?;
    }
    Ok(())
}

/// `git am --quit`: leave HEAD and the worktree as-is, only drop the state.
fn am_quit(
    git_dir: &Path,
    common_git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
    state_dir: &Path,
    config: &GitConfig,
    lazy_fetch: bool,
) -> Result<()> {
    am_require_in_progress(state_dir)?;
    finish_am(
        git_dir,
        common_git_dir,
        worktree_root,
        format,
        state_dir,
        config,
        lazy_fetch,
    )
}

/// `git am --skip`: discard the current patch's partial state, reset the
/// worktree/index to HEAD, and resume with the next patch.
fn am_skip(
    git_dir: &Path,
    common_git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
    state_dir: &Path,
    config: &GitConfig,
    lazy_fetch: bool,
) -> Result<()> {
    am_require_in_progress(state_dir)?;
    // git's `am_skip` clears the in-progress rerere state for the skipped patch
    // (am_rerere_clear) so its unresolved preimage is not left behind (t4151
    // "am --skip ... test ! -f .git/MERGE_RR"). A no-op unless rerere.enabled.
    commands::rerere::rerere_clear(git_dir)?;
    // git's `am_skip` runs `clean_index(HEAD, HEAD)`: discard the current
    // patch's partial application (conflict markers, unmerged entries, files the
    // patch added) and reset to HEAD, while preserving worktree-only changes to
    // unchanged files and untracked files (t4151 "am --skip continue after
    // failed am", "leaves index stat info alone"). HEAD is unborn on an orphan
    // branch, in which case the index is simply cleared.
    let refs = FileRefStore::new(git_dir, format);
    let head_oid = head_commit_oid(&refs)?;
    am_clean_index(
        git_dir,
        common_git_dir,
        worktree_root,
        format,
        head_oid.as_ref(),
        head_oid.as_ref(),
    )?;
    let next = read_state_usize(state_dir, "next")?;
    // git's `am_skip` records the skipped commit in `rewritten` too: `<orig> <HEAD>`,
    // where HEAD is the (cleaned) tip at skip time (am.c:2131). The post-rewrite
    // hook then reports a skipped commit as rewritten to the commit it folded into.
    if let Some(head_oid) = &head_oid {
        record_rebase_rewrite(state_dir, format, next, head_oid)?;
    }
    run_am_series(
        git_dir,
        common_git_dir,
        worktree_root,
        format,
        state_dir,
        next + 1,
        AmResumeOverrides::default(),
        config,
        lazy_fetch,
    )
}

/// `git am --continue`/`--resolved`: commit the staged resolution of the current
/// patch using its preserved author/message, then resume with the next patch.
fn am_continue(
    git_dir: &Path,
    common_git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
    state_dir: &Path,
    overrides: AmResumeOverrides,
    config: &GitConfig,
    lazy_fetch: bool,
) -> Result<()> {
    am_require_in_progress(state_dir)?;
    // Command-line options override the saved session options for the resumed
    // (continued) commit only; the remaining patches use the saved options
    // (am.c's `am_run` reloads them after the first patch). e.g.
    // `am --signoff --continue` signs off only this commit (t4153).
    let mut commit_opts = read_am_commit_opts(state_dir);
    if let Some(signoff) = overrides.signoff {
        commit_opts.signoff = signoff;
    }
    let quiet = overrides
        .quiet
        .unwrap_or_else(|| read_state_bool(state_dir, "quiet"));
    let next = read_state_usize(state_dir, "next")?;
    let patch = read_patch_file(state_dir, next)?;

    if !quiet {
        println!("Applying: {}", patch.subject);
    }

    // git's `am_resolve` validates two preconditions before committing the
    // user's staged resolution. We must distinguish them because the unmerged
    // case and the nothing-staged case carry different messages (and the latter
    // is what cell t4150-am.52 checks).
    let index = read_repository_index(git_dir, format)?;
    let has_unmerged = index.as_ref().is_some_and(|index| {
        index
            .entries
            .iter()
            .any(|entry| (entry.flags >> 12) & 0x3 != 0)
    });

    // (1) Unmerged paths: refuse with git's "still have unmerged paths" message.
    // (`write_tree` would fail on an unmerged index, so this is checked before
    // the no-changes test below — git's `repo_index_has_changes` reports such an
    // index as *changed*, so it reaches the same unmerged branch.)
    if has_unmerged {
        println!("You still have unmerged paths in your index.");
        println!("You should 'git add' each file with resolved conflicts to mark them as such.");
        println!("You might run `git rm` on a file to accept \"deleted by them\" for it.");
        am_print_resolve_hints();
        return Err(GitError::Exit(128));
    }

    // (2) Nothing staged: the index matches HEAD, so there is nothing to commit.
    // git prints "No changes - did you forget to use 'git add'?" and refuses.
    let refs = FileRefStore::new(git_dir, format);
    if let Some(head_oid) = head_commit_oid(&refs)?
        && !am_index_is_dirty(git_dir, common_git_dir, format, &head_oid)?
    {
        println!("No changes - did you forget to use 'git add'?");
        println!("If there is nothing left to stage, chances are that something else");
        println!("already introduced the same changes; you might want to skip this patch.");
        am_print_resolve_hints();
        return Err(GitError::Exit(128));
    }

    // git's `am_resolve`: in interactive mode, prompt before committing the
    // resolution. `n` advances past this patch without committing it (t4257
    // "interactive am can resolve conflict").
    if read_state_bool(state_dir, "interactive") {
        match am_do_interactive(&patch.message)? {
            AmInteractiveDecision::Apply => {}
            AmInteractiveDecision::AcceptAll => {
                fs::write(state_dir.join("interactive"), bool_flag(false))?;
            }
            AmInteractiveDecision::Skip => {
                return run_am_series(
                    git_dir,
                    common_git_dir,
                    worktree_root,
                    format,
                    state_dir,
                    next + 1,
                    AmResumeOverrides::default(),
                    config,
                    lazy_fetch,
                );
            }
        }
    }

    let new_oid = create_am_commit(
        git_dir,
        common_git_dir,
        worktree_root,
        format,
        &patch,
        commit_opts,
        config,
    )?;
    record_rebase_rewrite(state_dir, format, next, &new_oid)?;
    // git's `am_resolve` runs rerere so a resolved conflict is recorded for
    // future replay (t4150 "am -3 works with rerere"). A no-op unless rerere
    // is enabled and a MERGE_RR is in progress.
    commands::rerere::record_resolved_after_commit(git_dir, worktree_root, format)?;
    run_am_series(
        git_dir,
        common_git_dir,
        worktree_root,
        format,
        state_dir,
        next + 1,
        AmResumeOverrides::default(),
        config,
        lazy_fetch,
    )
}

/// `git am --retry`: re-apply the current (failed) patch from scratch, honouring
/// any command-line option overrides (git's RESUME_APPLY). The override applies
/// to this patch only; subsequent patches use the saved session options.
fn am_retry(
    git_dir: &Path,
    common_git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
    state_dir: &Path,
    overrides: AmResumeOverrides,
    config: &GitConfig,
    lazy_fetch: bool,
) -> Result<()> {
    am_require_in_progress(state_dir)?;
    let next = read_state_usize(state_dir, "next")?;
    run_am_series(
        git_dir,
        common_git_dir,
        worktree_root,
        format,
        state_dir,
        next,
        overrides,
        config,
        lazy_fetch,
    )
}

/// `git am --allow-empty`: when an empty patch stopped the series, record it as
/// an empty commit and continue. For non-empty/conflicted states, use the normal
/// `--continue` validation so clean or unmerged indexes are still rejected.
fn am_continue_allow_empty(
    git_dir: &Path,
    common_git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
    state_dir: &Path,
    config: &GitConfig,
    lazy_fetch: bool,
) -> Result<()> {
    am_require_in_progress(state_dir)?;
    let next = read_state_usize(state_dir, "next")?;
    let mut patch = read_patch_file(state_dir, next)?;
    if !patch.diff.is_empty() {
        return Err(GitError::Exit(128));
    }

    let index = read_repository_index(git_dir, format)?;
    let has_unmerged = index.as_ref().is_some_and(|index| {
        index
            .entries
            .iter()
            .any(|entry| (entry.flags >> 12) & 0x3 != 0)
    });
    if has_unmerged {
        return am_continue(
            git_dir,
            common_git_dir,
            worktree_root,
            format,
            state_dir,
            AmResumeOverrides::default(),
            config,
            lazy_fetch,
        );
    }

    let refs = FileRefStore::new(git_dir, format);
    if let Some(head_oid) = head_commit_oid(&refs)?
        && am_index_is_dirty(git_dir, common_git_dir, format, &head_oid)?
    {
        return am_continue(
            git_dir,
            common_git_dir,
            worktree_root,
            format,
            state_dir,
            AmResumeOverrides::default(),
            config,
            lazy_fetch,
        );
    }

    let commit_opts = read_am_commit_opts(state_dir);
    let quiet = read_state_bool(state_dir, "quiet");
    patch.message = prepare_am_commit_message(git_dir, &patch, commit_opts)?;
    if !quiet {
        println!("Applying: {}", patch.subject);
    }
    let new_oid = create_am_commit(
        git_dir,
        common_git_dir,
        worktree_root,
        format,
        &patch,
        commit_opts,
        config,
    )?;
    record_rebase_rewrite(state_dir, format, next, &new_oid)?;
    if !quiet {
        println!("No changes - recorded it as an empty commit.");
    }
    run_am_series(
        git_dir,
        common_git_dir,
        worktree_root,
        format,
        state_dir,
        next + 1,
        AmResumeOverrides::default(),
        config,
        lazy_fetch,
    )
}

// ===========================================================================
// Small byte helpers
// ===========================================================================

/// Split a buffer into lines, each retaining its trailing `\n` (the final line
/// keeps whatever terminator it had, or none).
fn split_keep_newline(input: &[u8]) -> Vec<Vec<u8>> {
    let mut lines = Vec::new();
    let mut start = 0;
    for (idx, byte) in input.iter().enumerate() {
        if *byte == b'\n' {
            lines.push(input[start..=idx].to_vec());
            start = idx + 1;
        }
    }
    if start < input.len() {
        lines.push(input[start..].to_vec());
    }
    lines
}

/// A line without its trailing `\r?\n`.
fn trim_trailing_newline(line: &[u8]) -> &[u8] {
    let mut end = line.len();
    if end > 0 && line[end - 1] == b'\n' {
        end -= 1;
    }
    if end > 0 && line[end - 1] == b'\r' {
        end -= 1;
    }
    &line[..end]
}
