//! `git rebase` — the merge backend driven by the sequencer todo machine.
//!
//! The on-disk contract (`.git/rebase-merge/`), the todo instruction sheet,
//! and the drive loop (`complete_action`, `pick_commits`, the todo verbs,
//! `--continue`/`--skip`/`--abort`/`--quit`/`--edit-todo`, rewritten-ref
//! tracking, update-refs application, and autostash integration) live in
//! `sley_sequencer::rebase_drive`; this module is the porcelain: option
//! parsing, usage text, todo generation (`sequencer_make_script`,
//! autosquash, `-x`/update-ref decoration), backend selection (apply vs
//! merge), progress/exit-code policy, and the host services injected through
//! [`rdrive::RebaseHosts`] (sequence editor, hooks, status collection, stash
//! and rerere primitives, notes rewrite, commit signing).
#![allow(clippy::expect_used, clippy::unwrap_used)]

use crate::commands::merge_rebase::{
    commit_tree_oid, head_commit_oid, merge_base_fork_point, merge_bases,
    print_branch_commit_summary, print_commit_shortstat_between_trees,
};
use crate::commands::replay::launch_editor;
use crate::*;
use sley_sequencer::rebase as seq;
use sley_sequencer::rebase::{RebaseTodoItem, TodoCommand};
use sley_sequencer::rebase_drive as rdrive;
use sley_sequencer::am as sam;

// ---------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum RebaseAction {
    None,
    Continue,
    Skip,
    Abort,
    Quit,
    EditTodo,
    ShowCurrentPatch,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EmptyMode {
    Unspecified,
    Drop,
    Keep,
    Stop,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RebaseMergesMode {
    NoRebaseCousins,
    RebaseCousins,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RebaseMergesArg {
    Disabled,
    Enabled(RebaseMergesMode),
}

struct RebaseArgs {
    action: RebaseAction,
    interactive: bool,
    merge_backend: bool,
    apply_backend: bool,
    onto_name: Option<String>,
    keep_base: bool,
    quiet: bool,
    verbose: bool,
    stat: Option<bool>,
    autostash: Option<bool>,
    autosquash: Option<bool>,
    keep_empty: Option<bool>,
    empty: EmptyMode,
    force: bool,
    exec: Vec<String>,
    signoff: bool,
    no_verify: bool,
    reschedule_failed_exec: Option<bool>,
    committer_date_is_author_date: bool,
    ignore_date: bool,
    ignore_whitespace: bool,
    whitespace: Option<String>,
    context_lines: Option<u32>,
    root: bool,
    fork_point: Option<bool>,
    reapply_cherry_picks: Option<bool>,
    update_refs: Option<bool>,
    rerere_autoupdate: Option<bool>,
    rebase_merges: Option<RebaseMergesArg>,
    strategy: Option<String>,
    strategy_opts: Vec<String>,
    gpg_sign: Option<String>,
    no_gpg_sign: bool,
    positional: Vec<String>,
    total_args: usize,
    recurse_submodules: bool,
}

fn rebase_usage_error() -> GitError {
    print_rebase_usage();
    GitError::Exit(129)
}

fn option_requires_value(name: &str) -> GitError {
    eprintln!("error: option `{name}' requires a value");
    rebase_usage_error()
}

fn parse_rebase_args(args: &[String]) -> Result<RebaseArgs> {
    let mut out = RebaseArgs {
        action: RebaseAction::None,
        interactive: false,
        merge_backend: false,
        apply_backend: false,
        onto_name: None,
        keep_base: false,
        quiet: false,
        verbose: false,
        stat: None,
        autostash: None,
        autosquash: None,
        keep_empty: None,
        empty: EmptyMode::Unspecified,
        force: false,
        exec: Vec::new(),
        signoff: false,
        no_verify: false,
        reschedule_failed_exec: None,
        committer_date_is_author_date: false,
        ignore_date: false,
        ignore_whitespace: false,
        whitespace: None,
        context_lines: None,
        root: false,
        fork_point: None,
        reapply_cherry_picks: None,
        update_refs: None,
        rerere_autoupdate: None,
        rebase_merges: None,
        strategy: None,
        strategy_opts: Vec::new(),
        gpg_sign: None,
        no_gpg_sign: false,
        positional: Vec::new(),
        total_args: args.len(),
        recurse_submodules: false,
    };
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        let take_value = |index: &mut usize| -> Result<String> {
            if let Some((_, value)) = args[*index].split_once('=') {
                return Ok(value.to_string());
            }
            *index += 1;
            args.get(*index).cloned().ok_or_else(rebase_usage_error)
        };
        match arg {
            "--onto" => {
                out.onto_name = Some(take_value(&mut index)?);
            }
            _ if arg.starts_with("--onto=") => {
                out.onto_name = Some(take_value(&mut index)?);
            }
            "--keep-base" => out.keep_base = true,
            "-i" | "--interactive" => out.interactive = true,
            "-ir" | "-ri" => {
                out.interactive = true;
                out.rebase_merges =
                    Some(RebaseMergesArg::Enabled(RebaseMergesMode::NoRebaseCousins));
            }
            "-ix" | "-xi" => {
                out.interactive = true;
                index += 1;
                let value = args
                    .get(index)
                    .cloned()
                    .ok_or_else(|| option_requires_value("exec"))?;
                out.exec.push(value);
            }
            "-m" | "--merge" => out.merge_backend = true,
            "--apply" => out.apply_backend = true,
            "--continue" => out.action = RebaseAction::Continue,
            "--skip" => out.action = RebaseAction::Skip,
            "--abort" => out.action = RebaseAction::Abort,
            "--quit" => out.action = RebaseAction::Quit,
            "--edit-todo" => out.action = RebaseAction::EditTodo,
            "--show-current-patch" => out.action = RebaseAction::ShowCurrentPatch,
            "-q" | "--quiet" => {
                out.quiet = true;
                out.verbose = false;
                out.stat = Some(false);
            }
            "--no-quiet" => out.quiet = false,
            "-v" | "--verbose" => {
                out.verbose = true;
                out.quiet = false;
                out.stat = Some(true);
            }
            "--no-verbose" => out.verbose = false,
            "-n" | "--no-stat" => out.stat = Some(false),
            "--stat" => out.stat = Some(true),
            "--autostash" => out.autostash = Some(true),
            "--no-autostash" => out.autostash = Some(false),
            "--recurse-submodules" => out.recurse_submodules = true,
            "--no-recurse-submodules" => out.recurse_submodules = false,
            _ if arg.starts_with("--recurse-submodules=") => {
                let value = &arg["--recurse-submodules=".len()..];
                out.recurse_submodules = !matches!(value, "no" | "false" | "off");
            }
            "--autosquash" => out.autosquash = Some(true),
            "--no-autosquash" => out.autosquash = Some(false),
            "-k" | "--keep-empty" => out.keep_empty = Some(true),
            "--no-keep-empty" => out.keep_empty = Some(false),
            _ if arg.starts_with("--empty=") => {
                out.empty = match &arg["--empty=".len()..] {
                    "drop" => EmptyMode::Drop,
                    "keep" => EmptyMode::Keep,
                    "stop" | "ask" => EmptyMode::Stop,
                    other => {
                        eprintln!(
                            "fatal: unrecognized empty type '{other}'; valid values are \"drop\", \"keep\", and \"stop\"."
                        );
                        return Err(GitError::Exit(128));
                    }
                };
            }
            "-f" | "--force-rebase" | "--no-ff" => out.force = true,
            "--ff" => out.force = false,
            "-x" | "--exec" => {
                index += 1;
                let value = args
                    .get(index)
                    .cloned()
                    .ok_or_else(|| option_requires_value("exec"))?;
                out.exec.push(value);
            }
            _ if arg.starts_with("--exec=") => {
                out.exec.push(arg["--exec=".len()..].to_string());
            }
            _ if arg.starts_with("-x") && arg.len() > 2 => {
                out.exec.push(arg[2..].to_string());
            }
            "--signoff" => out.signoff = true,
            "--no-signoff" => out.signoff = false,
            "--reschedule-failed-exec" => out.reschedule_failed_exec = Some(true),
            "--no-reschedule-failed-exec" => out.reschedule_failed_exec = Some(false),
            "--root" => out.root = true,
            "--fork-point" => out.fork_point = Some(true),
            "--no-fork-point" => out.fork_point = Some(false),
            "--reapply-cherry-picks" => out.reapply_cherry_picks = Some(true),
            "--no-reapply-cherry-picks" => out.reapply_cherry_picks = Some(false),
            "--update-refs" => out.update_refs = Some(true),
            "--no-update-refs" => out.update_refs = Some(false),
            "-s" | "--strategy" => {
                out.strategy = Some(take_value(&mut index)?);
            }
            _ if arg.starts_with("--strategy=") => {
                out.strategy = Some(arg["--strategy=".len()..].to_string());
            }
            "-X" | "--strategy-option" => {
                let value = take_value(&mut index)?;
                out.strategy_opts.push(value);
            }
            _ if arg.starts_with("--strategy-option=") => {
                out.strategy_opts
                    .push(arg["--strategy-option=".len()..].to_string());
            }
            _ if arg.starts_with("-X") && arg.len() > 2 => {
                out.strategy_opts.push(arg[2..].to_string());
            }
            "--no-verify" => out.no_verify = true,
            "--verify" => out.no_verify = false,
            "-S" | "--gpg-sign" => {
                out.gpg_sign = Some(String::new());
                out.no_gpg_sign = false;
            }
            _ if arg.starts_with("-S") && arg.len() > 2 => {
                out.gpg_sign = Some(arg[2..].to_string());
                out.no_gpg_sign = false;
            }
            _ if arg.starts_with("--gpg-sign=") => {
                out.gpg_sign = Some(arg["--gpg-sign=".len()..].to_string());
                out.no_gpg_sign = false;
            }
            "--no-gpg-sign" => {
                out.gpg_sign = None;
                out.no_gpg_sign = true;
            }
            "--rerere-autoupdate" => out.rerere_autoupdate = Some(true),
            "--no-rerere-autoupdate" => out.rerere_autoupdate = Some(false),
            "--allow-empty-message" => {}
            "--committer-date-is-author-date" => {
                out.committer_date_is_author_date = true;
                out.force = true;
            }
            "--no-committer-date-is-author-date" => out.committer_date_is_author_date = false,
            "--reset-author-date" | "--ignore-date" => {
                out.ignore_date = true;
                out.force = true;
            }
            "--no-reset-author-date" | "--no-ignore-date" => out.ignore_date = false,
            "--ignore-whitespace" => {
                out.ignore_whitespace = true;
                out.strategy_opts.push("ignore-space-change".to_string());
            }
            "--no-ignore-whitespace" => {
                out.ignore_whitespace = false;
                out.strategy_opts
                    .retain(|opt| opt.as_str() != "ignore-space-change");
            }
            _ if arg.starts_with("-C") => {
                let value = &arg[2..];
                if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                    eprintln!("fatal: switch `C' expects a numerical value");
                    return Err(GitError::Exit(128));
                }
                let Ok(context) = value.parse() else {
                    eprintln!("fatal: switch `C' expects a numerical value");
                    return Err(GitError::Exit(128));
                };
                out.context_lines = Some(context);
            }
            _ if arg.starts_with("--whitespace=") => {
                let value = &arg["--whitespace=".len()..];
                if !matches!(
                    value,
                    "warn" | "nowarn" | "error" | "error-all" | "fix" | "strip"
                ) {
                    eprintln!("fatal: Invalid whitespace option: '{value}'");
                    return Err(GitError::Exit(128));
                }
                out.whitespace = Some(value.to_string());
            }
            "--rebase-merges" | "-r" => {
                out.rebase_merges =
                    Some(RebaseMergesArg::Enabled(RebaseMergesMode::NoRebaseCousins))
            }
            "--no-rebase-merges" => out.rebase_merges = Some(RebaseMergesArg::Disabled),
            _ if arg.starts_with("--rebase-merges=") => {
                out.rebase_merges = match &arg["--rebase-merges=".len()..] {
                    "no-rebase-cousins" => {
                        Some(RebaseMergesArg::Enabled(RebaseMergesMode::NoRebaseCousins))
                    }
                    "rebase-cousins" => {
                        Some(RebaseMergesArg::Enabled(RebaseMergesMode::RebaseCousins))
                    }
                    other => {
                        eprintln!("fatal: Unknown mode: {other}");
                        return Err(GitError::Exit(128));
                    }
                };
            }
            "--" => {
                out.positional.extend(args[index + 1..].iter().cloned());
                break;
            }
            _ if arg.starts_with('-') && arg.len() > 1 => {
                eprintln!("error: unknown option `{}'", arg.trim_start_matches('-'));
                return Err(rebase_usage_error());
            }
            _ => out.positional.push(arg.to_string()),
        }
        index += 1;
    }
    for command in &out.exec {
        // git (builtin/rebase.c) treats a command that is entirely blank
        // (` \t\r\f\v`) as empty, not just the zero-length string.
        if command
            .bytes()
            .all(|byte| matches!(byte, b' ' | b'\t' | b'\r' | 0x0c | 0x0b))
        {
            eprintln!("error: empty exec command");
            return Err(GitError::Exit(1));
        }
        if command.contains('\n') {
            eprintln!("error: exec commands cannot contain newlines");
            return Err(GitError::Exit(1));
        }
    }
    Ok(out)
}

fn rebase_apply_opts(args: &RebaseArgs) -> Vec<String> {
    let mut opts = Vec::new();
    if args.ignore_whitespace {
        opts.push("--ignore-whitespace".to_string());
    }
    if let Some(whitespace) = &args.whitespace {
        opts.push(format!("--whitespace={whitespace}"));
    }
    if let Some(context) = args.context_lines {
        opts.push(format!("-C{context}"));
    }
    opts
}

fn print_rebase_usage() {
    eprintln!(
        "usage: git rebase [-i] [options] [--exec <cmd>] [--onto <newbase> | --keep-base] [<upstream> [<branch>]]"
    );
    eprintln!(
        "   or: git rebase [-i] [options] [--exec <cmd>] [--onto <newbase>] --root [<branch>]"
    );
    eprintln!("   or: git rebase --continue | --abort | --skip | --edit-todo");
    eprintln!();
}

// ---------------------------------------------------------------------------
// Context + persistent machine state
// ---------------------------------------------------------------------------

struct Ctx {
    repository: sley::Repository,
    config: GitConfig,
    refs: FileRefStore,
    common_refs: FileRefStore,
    git_dir: PathBuf,
    common_git_dir: PathBuf,
    worktree_root: PathBuf,
    format: ObjectFormat,
    /// `GIT_REFLOG_ACTION` or `"rebase"`.
    reflog_action: String,
    recurse_submodules: bool,
    lazy_fetch: bool,
    replace_objects: bool,
}

impl Ctx {
    fn from_session(cli_session: &crate::session::CliSession) -> Result<Ctx> {
        let repository = cli_session.open_repository()?;
        let git_dir = repository.git_dir().to_path_buf();
        let common_git_dir = repository.common_dir().to_path_buf();
        let worktree_root = worktree_root_for_git_dir(cli_session, &git_dir)?;
        let format = repository.object_format();
        // Linked worktrees keep rebase state under `$GIT_DIR/worktrees/<id>/`
        // while repository config lives in the common gitdir. Reading config
        // from the per-worktree admin dir silently drops `sequence.editor` and
        // friends (t3430 `refs/rewritten/* is worktree-local` uses
        // `test_config -C wt sequence.editor ...`).
        //
        // Use the full effective cascade (system + global + local) so settings
        // like `user.useConfigOnly` from `~/.gitconfig` refuse non-ff rebases
        // that would invent a committer identity (t7517).
        let config =
            commands::remote::read_effective_repo_config(&common_git_dir, cli_session.cwd())?;
        let refs = repository.references();
        let common_refs = FileRefStore::new(&common_git_dir, format);
        let reflog_action = env::var("GIT_REFLOG_ACTION").unwrap_or_else(|_| "rebase".to_string());
        Ok(Ctx {
            repository,
            config,
            refs,
            common_refs,
            git_dir,
            common_git_dir,
            worktree_root,
            format,
            reflog_action,
            recurse_submodules: false,
            lazy_fetch: cli_session.lazy_fetch(),
            replace_objects: cli_session.replace_objects(),
        })
    }

    fn db(&self) -> FileObjectDatabase {
        self.repository.objects_mut()
    }

    fn refs(&self) -> &FileRefStore {
        &self.refs
    }

    fn state_path(&self, name: &str) -> PathBuf {
        seq::state_path(&self.git_dir, name)
    }

    fn reflog(&self, sub_action: &str, rest: Option<&str>) -> Vec<u8> {
        let mut out = format!("{} ({sub_action})", self.reflog_action);
        if let Some(rest) = rest {
            out.push_str(": ");
            out.push_str(rest);
        }
        out.into_bytes()
    }
}



type MachineOpts = seq::RebaseState;

// ---------------------------------------------------------------------------
// Todo list plumbing
// ---------------------------------------------------------------------------











fn rebase_config_value(ctx: &Ctx, section: &str, key: &str) -> Option<String> {
    // A linked worktree's administrative gitdir contains HEAD/index/rebase
    // state, while repository configuration remains in the common gitdir.
    // Reading `<worktrees/name>/config` silently loses settings such as
    // sequence.editor, causing an interactive rebase in a linked worktree to
    // ignore its configured todo editor entirely.
    ctx.config.get(section, None, key).map(str::to_string)
}

fn rebase_config_bool(ctx: &Ctx, section: &str, key: &str) -> Option<bool> {
    let value = rebase_config_value(ctx, section, key)?;
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" | "" => Some(false),
        _ => None,
    }
}


fn rebase_merges_config(ctx: &Ctx) -> Option<RebaseMergesMode> {
    let value = rebase_config_value(ctx, "rebase", "rebaseMerges")?;
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" | "no-rebase-cousins" => {
            Some(RebaseMergesMode::NoRebaseCousins)
        }
        "rebase-cousins" => Some(RebaseMergesMode::RebaseCousins),
        "false" | "no" | "off" | "0" | "" => None,
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Engine hand-off: repository context + host services
// ---------------------------------------------------------------------------

/// Build the engine's repository view from the CLI session context. The
/// `Repository -> FileObjectDatabase` handle, ref stores, config cascade, and
/// worktree layout all come from the already-opened session.
fn drive_context(ctx: &Ctx) -> rdrive::RebaseContext {
    rdrive::RebaseContext::new(
        ctx.git_dir.clone(),
        ctx.common_git_dir.clone(),
        ctx.worktree_root.clone(),
        ctx.format,
        ctx.config.clone(),
        ctx.refs.clone(),
        ctx.common_refs.clone(),
        ctx.db(),
        ctx.reflog_action.clone(),
        ctx.replace_objects,
        ctx.recurse_submodules,
    )
}

struct RebasePrefetch;

impl sley_sequencer::apply::PromisorObjectFetch for RebasePrefetch {
    fn read_object_maybe_prefetch(
        &self,
        db: &FileObjectDatabase,
        oid: &ObjectId,
    ) -> Result<std::sync::Arc<sley_object::EncodedObject>> {
        crate::read_object_maybe_prefetch_promisor(db, oid, true)
    }
}

/// Host services for the merge-backend drive loop: every process-spawning,
/// renderer, or session-bound operation the engine cannot own. Each closure is
/// a verbatim relocation of the call site it used to serve in this file.
fn rebase_hosts(ctx: &Ctx) -> rdrive::RebaseHosts<'_> {
    rdrive::RebaseHosts {
        promisor_fetch: ctx.lazy_fetch.then_some(prefetch_adapter()),
        short_status: Box::new(|| {
            crate::collect_short_status(&ctx.worktree_root, &ctx.git_dir, ctx.format)
        }),
        abbrev_width: Box::new(|| {
            repository_abbrev(&ctx.git_dir, ctx.format)
                .ok()
                .flatten()
                .unwrap_or(ctx.format.hex_len())
        }),
        reset_submodules: Box::new(|commit| {
            commands::read_tree::reset_index_and_worktree_to_commit(
                &ctx.worktree_root,
                &ctx.git_dir,
                ctx.format,
                commit,
                true,
            )
        }),
        launch_sequence_editor: Box::new(|path| launch_sequence_editor(ctx, path)),
        editor_status_block: Box::new(|comment_string, amend| {
            commands::commit::render_commit_editor_status_for_rebase(
                &ctx.git_dir,
                &ctx.worktree_root,
                ctx.format,
                comment_string,
                amend,
            )
        }),
        prepare_commit_message: Box::new(
            |editmsg: &Path, seed, commit_head, merge, edit| -> Result<Vec<u8>> {
                fs::write(editmsg, &seed)?;
                let source = if commit_head {
                    commands::commit::PrepareCommitMsgSource::Commit("HEAD")
                } else if merge {
                    commands::commit::PrepareCommitMsgSource::Merge
                } else {
                    commands::commit::PrepareCommitMsgSource::Message
                };
                commands::commit::run_prepare_commit_msg_hook(
                    &ctx.git_dir,
                    editmsg,
                    source,
                    Vec::new(),
                    !edit,
                )?;
                if edit {
                    launch_editor(&ctx.git_dir, editmsg)?;
                    let path_arg = editmsg.to_string_lossy().into_owned();
                    commands::hooks::run_hook_l_at(&ctx.git_dir, "commit-msg", &[path_arg.as_str()])?;
                }
                Ok(fs::read(editmsg)?)
            },
        ),
        run_hook: Box::new(|name: &str, args, stdin| {
            commands::hooks::run_hook_at(
                &ctx.git_dir,
                name,
                commands::hooks::HookRun {
                    args,
                    stdin,
                    ..commands::hooks::HookRun::default()
                },
            )?;
            Ok(())
        }),
        tree_patch: Box::new(|old_tree, new_tree| {
            render_tree_to_tree_patch(&ctx.db(), ctx.format, old_tree, new_tree, ctx.lazy_fetch)
        }),
        print_continue_summary: Box::new(|new_oid, message, old_tree, new_tree| {
            let db = ctx.db();
            print_branch_commit_summary(&db, &ctx.git_dir, ctx.format, new_oid, message)?;
            print_commit_shortstat_between_trees(&db, ctx.format, &old_tree, &new_tree, ctx.lazy_fetch)
        }),
        print_diffstat: Box::new(|old_tree, new_tree| {
            print_rebase_diffstat(
                &ctx.db(),
                ctx.format,
                old_tree,
                new_tree,
                &ctx.config,
                ctx.lazy_fetch,
                false,
            )
        }),
        stash_create: Box::new(|| {
            commands::stash::create_stash_for_autostash_at(&ctx.git_dir, &ctx.worktree_root)
        }),
        stash_apply_quietly: Box::new(|oid| {
            commands::stash::apply_stash_commit_quietly_at(
                &ctx.git_dir,
                &ctx.worktree_root,
                oid,
                ctx.lazy_fetch,
            )
        }),
        stash_store: Box::new(|oid, message| {
            commands::stash::store_stash_commit_at(&ctx.git_dir, oid, message)
        }),
        rerere_now: Box::new(|autoupdate| {
            commands::rerere::repo_rerere(&ctx.git_dir, &ctx.worktree_root, ctx.format, autoupdate)
        }),
        rerere_record_resolved: Box::new(|| {
            commands::rerere::record_resolved_after_commit(&ctx.git_dir, &ctx.worktree_root, ctx.format)
        }),
        append_signoff: Box::new(|message, signoff| {
            commands::replay::append_signoff_before_comments(message, signoff)
        }),
        copy_notes_for_rewrite: Box::new(|pairs| copy_notes_for_rewrite(ctx, pairs)),
        commit_signature: Box::new(move |tree, parents, author, committer, message, encoding, opts| {
            let sign = if opts.no_gpg_sign {
                false
            } else {
                opts.gpg_sign.is_some()
                    || ctx.config.get_bool("commit", None, "gpgsign").unwrap_or(false)
            };
            if !sign {
                return Ok(None);
            }
            let unsigned = Commit {
                tree,
                parents: parents.to_vec(),
                author: author.to_vec(),
                committer: committer.to_vec(),
                encoding,
                message: message.to_vec(),
            };
            let key =
                commands::signing::signing_key(Some(&ctx.config), opts.gpg_sign.as_deref(), committer);
            commands::signing::sign_payload(Some(&ctx.config), &unsigned.write(), key.as_deref())
                .map(Some)
        }),
    }
}


/// Shared promisor hydration adapter (one static per process).
fn prefetch_adapter() -> &'static RebasePrefetch {
    static PREFETCH: RebasePrefetch = RebasePrefetch;
    &PREFETCH
}



fn am_engine_ctx(ctx: &Ctx) -> sam::AmContext<'static> {
    commands::am::am_engine_context(
        &ctx.git_dir,
        &ctx.common_git_dir,
        &ctx.worktree_root,
        ctx.format,
        &ctx.config,
        ctx.lazy_fetch,
    )
}

fn am_engine_hosts_for(ctx: &Ctx) -> sam::AmHosts<'static> {
    commands::am::am_engine_hosts(
        &ctx.git_dir,
        &ctx.common_git_dir,
        &ctx.worktree_root,
        ctx.lazy_fetch,
    )
}

pub(crate) fn cmd_rebase(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
    let parsed = parse_rebase_args(args)?;
    let mut ctx = Ctx::from_session(cli_session)?;
    ctx.recurse_submodules = parsed.recurse_submodules;

    if parsed.action != RebaseAction::None && parsed.total_args != 1 {
        return Err(rebase_usage_error());
    }
    if parsed.positional.len() > 2 {
        return Err(rebase_usage_error());
    }
    // With --root there is no upstream argument, so only an optional <branch>
    // is allowed (git: `--root [<branch>]`, errors when argc > 1).
    if parsed.root && parsed.positional.len() > 1 {
        return Err(rebase_usage_error());
    }

    // The apply backend keeps its state in `.git/rebase-apply/` (marked by a
    // `head-name` file). When such a rebase is in progress, the resume verbs
    // route to the am-based driver instead of the merge sequencer.
    let history_action = match parsed.action {
        RebaseAction::None => seq::HistoryEditAction::Start,
        RebaseAction::Continue => seq::HistoryEditAction::Continue,
        RebaseAction::Skip => seq::HistoryEditAction::Skip,
        RebaseAction::Abort => seq::HistoryEditAction::Abort,
        RebaseAction::Quit => seq::HistoryEditAction::Quit,
        RebaseAction::EditTodo => seq::HistoryEditAction::EditTodo,
        RebaseAction::ShowCurrentPatch => seq::HistoryEditAction::ShowCurrentPatch,
    };
    let history_plan = seq::plan_history_edit(seq::HistoryEditPlanOptions {
        action: history_action,
        apply_in_progress: sam::rebase_apply_in_progress(&ctx.git_dir),
        merge_in_progress: seq::in_progress(&ctx.git_dir),
    });
    if history_plan == seq::HistoryEditPlan::MissingState {
        eprintln!("fatal: no rebase in progress");
        return Err(GitError::Exit(128));
    }
    let (rctx, hosts) = (drive_context(&ctx), rebase_hosts(&ctx));
    let apply_in_progress = matches!(
        history_plan,
        seq::HistoryEditPlan::Resume {
            backend: seq::HistoryEditBackend::Apply,
            ..
        }
    );
    let merge_in_progress = matches!(
        history_plan,
        seq::HistoryEditPlan::Resume {
            backend: seq::HistoryEditBackend::Merge,
            ..
        }
    );

    if apply_in_progress {
        match parsed.action {
            RebaseAction::Continue => {
                let result =
                    sam::rebase_apply_continue(&am_engine_ctx(&ctx), &am_engine_hosts_for(&ctx));
                // Ok iff the whole series completed; restore the autostash then
                // (a fresh conflict returns Err and keeps it for the next step).
                if result.is_ok() {
                    rdrive::finish_apply_autostash(&rctx, &hosts);
                }
                return result;
            }
            RebaseAction::Skip => {
                let result =
                    sam::rebase_apply_skip(&am_engine_ctx(&ctx), &am_engine_hosts_for(&ctx));
                if result.is_ok() {
                    rdrive::finish_apply_autostash(&rctx, &hosts);
                }
                return result;
            }
            RebaseAction::Abort => {
                let autostash = rdrive::read_apply_autostash(&ctx.git_dir);
                let result =
                    sam::rebase_apply_abort(&am_engine_ctx(&ctx), &am_engine_hosts_for(&ctx));
                // Abort always ends the rebase; restore the autostash on top of
                // the restored orig_head (git applies it after reset).
                if result.is_ok() {
                    if let Some(text) = autostash {
                        rdrive::apply_save_autostash_text(&rctx, &hosts, &text, true);
                    }
                    seq::remove_merge_state(&ctx.git_dir);
                }
                return result;
            }
            RebaseAction::Quit => {
                if let Some(text) = rdrive::read_apply_autostash(&ctx.git_dir) {
                    rdrive::apply_save_autostash_text(&rctx, &hosts, &text, false);
                }
                let _ = fs::remove_dir_all(ctx.git_dir.join("rebase-apply"));
                seq::remove_merge_state(&ctx.git_dir);
                return Ok(());
            }
            RebaseAction::ShowCurrentPatch => {
                let path = ctx.git_dir.join("rebase-apply").join("patch");
                if let Ok(patch) = fs::read(path) {
                    io::stdout().write_all(&patch)?;
                    return Ok(());
                }
                eprintln!("fatal: there is no current patch");
                return Err(GitError::Exit(128));
            }
            RebaseAction::EditTodo => {
                eprintln!(
                    "error: The --edit-todo action can only be used during interactive rebase."
                );
                return Err(GitError::Exit(1));
            }
            RebaseAction::None => {
                eprintln!("fatal: It looks like 'git am' is in progress. Cannot rebase.");
                return Err(GitError::Exit(128));
            }
        }
    }

    match parsed.action {
        RebaseAction::Continue => return rdrive::rebase_continue(&rctx, &hosts),
        RebaseAction::Skip => return rdrive::rebase_skip(&rctx, &hosts),
        RebaseAction::Abort => return rdrive::rebase_abort(&rctx, &hosts),
        RebaseAction::Quit => return rdrive::rebase_quit(&rctx, &hosts),
        RebaseAction::EditTodo => return rdrive::rebase_edit_todo(&rctx, &hosts),
        RebaseAction::ShowCurrentPatch => {
            let path = ctx.state_path("patch");
            if let Ok(patch) = fs::read(path) {
                if env::var_os("GIT_TRACE").is_some() {
                    eprintln!("trace: built-in: git show REBASE_HEAD");
                }
                io::stdout().write_all(&patch)?;
                return Ok(());
            }
            eprintln!("fatal: there is no current patch");
            return Err(GitError::Exit(128));
        }
        RebaseAction::None => {}
    }

    if merge_in_progress {
        eprintln!(
            "fatal: It seems that there is already a rebase-merge directory, and\nI wonder if you are in the middle of another rebase.  If that is the\ncase, please try\n\tgit rebase (--continue | --abort | --skip)\nIf that is not the case, please\n\trm -fr \"{}\"\nand run me again.  I am stopping in case you still have something\nvaluable there.",
            seq::merge_dir(&ctx.git_dir).display()
        );
        return Err(GitError::Exit(128));
    }

    start_rebase(&ctx, parsed)
}

// ---------------------------------------------------------------------------
// Starting a rebase
// ---------------------------------------------------------------------------

fn start_rebase(ctx: &Ctx, args: RebaseArgs) -> Result<()> {
    let db = ctx.db();
    let refs = ctx.refs();
    // Engine view + host services for the merge backend's tail.
    let rctx = drive_context(ctx);
    let hosts = rebase_hosts(ctx);

    let interactive_explicit = args.interactive;
    let rebase_merges = match args.rebase_merges {
        Some(RebaseMergesArg::Disabled) => None,
        Some(RebaseMergesArg::Enabled(mode)) => Some(mode),
        None => rebase_merges_config(ctx),
    };
    let config_update_refs = args
        .update_refs
        .unwrap_or_else(|| rebase_config_bool(ctx, "rebase", "updateRefs").unwrap_or(false));
    // `--ignore-whitespace` pushes `ignore-space-change` into `strategy_opts` for
    // the merge backend, but it does NOT by itself force a backend (it is honoured
    // on whichever one is selected). So compute merge-implication ignoring that
    // single auto-added opt.
    let has_other_strategy_opts = args
        .strategy_opts
        .iter()
        .any(|opt| !(args.ignore_whitespace && opt.as_str() == "ignore-space-change"));
    let implied_merge = interactive_explicit
        || args.merge_backend
        || !args.exec.is_empty()
        || args.autosquash == Some(true)
        || args.empty != EmptyMode::Unspecified
        || args.keep_empty.is_some()
        || args.reapply_cherry_picks.is_some()
        || (args.root && args.onto_name.is_none())
        || rebase_merges.is_some()
        || config_update_refs
        || args.strategy.is_some()
        || has_other_strategy_opts;

    // The apply backend (`git rebase --apply` / `git-rebase--am`) is selected by
    // an explicit `--apply` and by apply-only knobs. It is incompatible with
    // merge-only options.
    let use_apply_backend =
        args.apply_backend || args.whitespace.is_some() || args.context_lines.is_some();
    if use_apply_backend && implied_merge {
        if args.rebase_merges.is_none() && rebase_merges.is_some() {
            eprintln!(
                "fatal: apply options are incompatible with rebase.rebaseMerges; use --no-rebase-merges"
            );
            return Err(GitError::Exit(128));
        }
        if args.update_refs.is_none() && config_update_refs {
            eprintln!(
                "fatal: apply options are incompatible with rebase.updateRefs; use --no-update-refs"
            );
            return Err(GitError::Exit(128));
        }
        eprintln!("fatal: apply options and merge options cannot be used together");
        return Err(GitError::Exit(128));
    }
    if args.keep_base && args.onto_name.is_some() {
        eprintln!("fatal: options '--keep-base' and '--onto' cannot be used together");
        return Err(GitError::Exit(128));
    }
    if args.keep_base && args.root {
        eprintln!("fatal: options '--keep-base' and '--root' cannot be used together");
        return Err(GitError::Exit(128));
    }
    let reapply_cherry_picks = args.reapply_cherry_picks.unwrap_or(args.keep_base);

    // Resolve upstream.
    // With --root there is no upstream argument: the lone positional (if any) is
    // the <branch> to rebase, so it must not be consumed as the upstream.
    let upstream_name = if args.root {
        String::new()
    } else {
        match args.positional.first() {
            // `git rebase -` is shorthand for the previous branch, like checkout.
            Some(name) if name == "-" => "@{-1}".to_string(),
            Some(name) => name.clone(),
            None => match default_upstream_name(ctx, refs) {
                Some(name) => name,
                None => {
                    print_missing_upstream_advice(ctx, refs);
                    return Err(GitError::Exit(1));
                }
            },
        }
    };
    let mut upstream = if args.root {
        None
    } else {
        let resolved = resolve_revision(
            &ctx.git_dir,
            ctx.format,
            &upstream_name,
            ctx.replace_objects,
        )
        .and_then(|oid| sley_rev::peel_to_commit(&db, ctx.format, &oid));
        match resolved {
            Ok(oid) => Some(oid),
            Err(_) => {
                eprintln!("fatal: invalid upstream '{upstream_name}'");
                return Err(GitError::Exit(128));
            }
        }
    };

    // Branch / orig_head / head_name. With --root the branch is positional 0
    // (no upstream); otherwise it follows the upstream at positional 1.
    let branch_index = if args.root { 0 } else { 1 };
    let (branch_name, head_name, orig_head, switch_to) = match args.positional.get(branch_index) {
        Some(branch) => {
            let full = format!("refs/heads/{branch}");
            if let Ok(Some(RefTarget::Direct(oid))) = refs.read_ref(&full) {
                (branch.clone(), Some(full), oid, Some(branch.clone()))
            } else if let Ok(oid) =
                resolve_revision(&ctx.git_dir, ctx.format, branch, ctx.replace_objects)
                    .and_then(|oid| sley_rev::peel_to_commit(&db, ctx.format, &oid))
            {
                (branch.clone(), None, oid, Some(branch.clone()))
            } else {
                eprintln!("fatal: no such branch/commit '{branch}'");
                return Err(GitError::Exit(128));
            }
        }
        None => {
            let head_target = refs.read_ref("HEAD")?;
            match head_target {
                Some(RefTarget::Symbolic(name)) => {
                    let branch = name
                        .strip_prefix("refs/heads/")
                        .unwrap_or(name.as_str())
                        .to_string();
                    let oid = match refs.read_ref(&name)? {
                        Some(RefTarget::Direct(oid)) => oid,
                        _ => {
                            eprintln!("fatal: Could not resolve HEAD to a commit");
                            return Err(GitError::Exit(128));
                        }
                    };
                    (branch, Some(name), oid, None)
                }
                Some(RefTarget::Direct(oid)) => ("HEAD".to_string(), None, oid, None),
                None => {
                    eprintln!("fatal: No such ref: HEAD");
                    return Err(GitError::Exit(128));
                }
            }
        }
    };

    // git refuses to switch to a branch that is checked out in another worktree
    // (die_if_checked_out). Only fires when a <branch> argument is switched to;
    // the current worktree is ignored, so rebasing the branch checked out here
    // is fine.
    if switch_to.is_some()
        && let Some(head_name) = &head_name
        && let Some(other) = commands::worktree::branch_checked_out_worktree(
            &ctx.common_git_dir,
            head_name,
            Some(&ctx.worktree_root),
        )?
    {
        eprintln!(
            "fatal: '{branch_name}' is already used by worktree at '{}'",
            other.display()
        );
        return Err(GitError::Exit(128));
    }

    let fork_point = match args.fork_point {
        Some(value) => value,
        None => {
            !args.root && args.onto_name.is_none() && !args.keep_base && args.positional.is_empty()
        }
    };
    if fork_point
        && let Some(fork_point) = merge_base_fork_point(
            &ctx.common_git_dir,
            ctx.format,
            &db,
            &upstream_name,
            &orig_head,
        )?
    {
        upstream = Some(fork_point);
    }

    // git creates the autostash BEFORE running the pre-rebase hook, and a hook
    // refusal restores it (builtin/rebase.c: create_autostash → pre-rebase →
    // cleanup_autostash on failure). Resolve the config flag here so the order
    // matches; the value is reused below in place of the later re-read.
    let autostash = args
        .autostash
        .unwrap_or_else(|| rebase_config_bool(ctx, "rebase", "autostash").unwrap_or(false));
    if autostash {
        rdrive::create_autostash(&rctx, &hosts, use_apply_backend)?;
    }

    if !args.no_verify {
        // git passes "--root" as the upstream argument to the pre-rebase hook
        // when rebasing from the root (builtin/rebase.c sets upstream_arg).
        let upstream_arg = if args.root {
            "--root"
        } else {
            upstream_name.as_str()
        };
        let mut hook_args = vec![upstream_arg];
        if args.positional.get(branch_index).is_some() {
            hook_args.push(branch_name.as_str());
        }
        if let Err(err) = commands::hooks::run_hook_l_at(&ctx.git_dir, "pre-rebase", &hook_args) {
            // The hook refused the rebase: restore the autostash and drop any
            // state so no rebase is left in progress (t3420 #18).
            rdrive::apply_autostash(&rctx, &hosts);
            seq::remove_merge_state(&ctx.git_dir);
            return Err(err);
        }
    }

    // Onto.
    let mut squash_onto = None;
    let onto_name = match &args.onto_name {
        Some(name) => name.clone(),
        None if args.root => {
            let oid = create_squash_onto(ctx)?;
            squash_onto = Some(oid);
            oid.to_hex()
        }
        None if args.keep_base => format!("{upstream_name}...{branch_name}"),
        None => upstream_name.clone(),
    };
    let onto = if args.root && args.onto_name.is_none() {
        squash_onto.expect("created squash-onto for --root")
    } else if onto_name.contains("...") {
        let (left, right) = onto_name.split_once("...").expect("contains ...");
        let left_oid = resolve_revision(
            &ctx.git_dir,
            ctx.format,
            if left.is_empty() { "HEAD" } else { left },
            ctx.replace_objects,
        )
        .and_then(|oid| sley_rev::peel_to_commit(&db, ctx.format, &oid));
        let right_oid = resolve_revision(
            &ctx.git_dir,
            ctx.format,
            if right.is_empty() { "HEAD" } else { right },
            ctx.replace_objects,
        )
        .and_then(|oid| sley_rev::peel_to_commit(&db, ctx.format, &oid));
        match (left_oid, right_oid) {
            (Ok(left), Ok(right)) => {
                let bases = merge_bases(&ctx.common_git_dir, &db, ctx.format, &left, &right)?;
                match bases.first() {
                    Some(base) if bases.len() == 1 => *base,
                    _ => {
                        if args.keep_base {
                            eprintln!(
                                "fatal: '{upstream_name}': need exactly one merge base with branch"
                            );
                        } else {
                            eprintln!("fatal: '{onto_name}': need exactly one merge base");
                        }
                        return Err(GitError::Exit(128));
                    }
                }
            }
            _ => {
                if args.keep_base {
                    eprintln!("fatal: '{upstream_name}': need exactly one merge base with branch");
                } else {
                    eprintln!("fatal: '{onto_name}': need exactly one merge base");
                }
                return Err(GitError::Exit(128));
            }
        }
    } else {
        match resolve_revision(&ctx.git_dir, ctx.format, &onto_name, ctx.replace_objects)
            .and_then(|oid| sley_rev::peel_to_commit(&db, ctx.format, &oid))
        {
            Ok(oid) => oid,
            Err(_) => {
                eprintln!("fatal: Does not point to a valid commit '{onto_name}'");
                return Err(GitError::Exit(128));
            }
        }
    };

    let autosquash = args.autosquash.unwrap_or_else(|| {
        interactive_explicit && rebase_config_bool(ctx, "rebase", "autosquash").unwrap_or(false)
    });
    let show_stat = match args.stat {
        Some(value) => value,
        None => rebase_config_bool(ctx, "rebase", "stat").unwrap_or(false) && !args.quiet,
    };
    let empty = if args.empty == EmptyMode::Unspecified {
        if interactive_explicit {
            EmptyMode::Stop
        } else if !args.exec.is_empty() {
            EmptyMode::Keep
        } else {
            EmptyMode::Drop
        }
    } else {
        args.empty
    };
    // git's default is keep_empty=1 (begin-empty commits kept) for the
    // merge/interactive backend; `--no-keep-empty` drops them.
    let keep_empty = args.keep_empty.unwrap_or(true);
    let reschedule_failed_exec = args.reschedule_failed_exec.unwrap_or_else(|| {
        rebase_config_bool(ctx, "rebase", "rescheduleFailedExec").unwrap_or(false)
    });
    let force = args.force || args.signoff;

    // Autostash was already created above (before the pre-rebase hook, matching
    // git's order), so the clean-tree gate below sees the stashed-clean tree.

    // Clean-tree gate.
    if let Err(err) = require_clean_work_tree(ctx, "rebase", true) {
        rdrive::cleanup_autostash_and_state(&rctx, &hosts);
        return Err(err);
    }

    // Preemptive fast-forward / up-to-date handling (non-interactive only).
    let allow_preemptive_ff =
        !interactive_explicit && args.exec.is_empty() && args.autosquash != Some(true);
    let branch_base = merge_bases(&ctx.common_git_dir, &db, ctx.format, &onto, &orig_head)?
        .into_iter()
        .next();
    if allow_preemptive_ff
        && let Some(base) = &branch_base
        && let Some(up) = &upstream
    {
        let upstream_base = merge_bases(&ctx.common_git_dir, &db, ctx.format, up, &orig_head)?;
        let can_ff = *base == onto
            && upstream_base.len() == 1
            && upstream_base[0] == onto
            && is_linear_history(&db, ctx.format, &onto, &orig_head)?;
        if can_ff && !force {
            if let Some(switch_to) = &switch_to {
                if head_name.is_some() {
                    // If switching to the branch fails (e.g. untracked files
                    // would be clobbered), restore the autostash and drop all
                    // state so no rebase is left in progress (`rebase --quit`
                    // must then report "no rebase in progress").
                    if let Err(err) = checkout_up_to_date(ctx, &db, switch_to, &orig_head) {
                        rdrive::apply_autostash(&rctx, &hosts);
                        seq::remove_merge_state(&ctx.git_dir);
                        return Err(err);
                    }
                } else {
                    // The <branch> argument names a non-branch (e.g. a tag): git
                    // still switches to it before reporting up-to-date, so detach
                    // HEAD onto its commit (RESET_HEAD_DETACH path).
                    rdrive::reset_index_and_worktree_to_commit_for_rebase(&rctx, &hosts, &orig_head)?;
                    let refs = ctx.refs();
                    let old = head_commit_oid(refs)?.unwrap_or_else(|| ObjectId::null(ctx.format));
                    let committer = committer_identity_for_reflog(&ctx.config)?;
                    detach_head_with_reflog(
                        ctx,
                        old,
                        orig_head,
                        ctx.reflog("start", Some(&format!("checkout {switch_to}"))),
                        committer,
                    )?;
                    run_rebase_post_checkout_hook(ctx, &old, &orig_head)?;
                }
            }
            if !args.quiet {
                if branch_name == "HEAD" {
                    println!("HEAD is up to date.");
                } else {
                    println!("Current branch {branch_name} is up to date.");
                }
            }
            rdrive::finish_rebase_cleanup(&rctx, &hosts);
            return Ok(());
        } else if can_ff && !args.quiet {
            if branch_name == "HEAD" {
                println!("HEAD is up to date, rebase forced.");
            } else {
                println!("Current branch {branch_name} is up to date, rebase forced.");
            }
        }
    }

    if show_stat {
        if args.verbose {
            match &branch_base {
                Some(base) => println!("Changes from {base} to {onto}:"),
                None => println!("Changes to {onto}:"),
            }
        }
        let old_tree = match &branch_base {
            Some(base) => commit_tree_oid(&db, ctx.format, base)?,
            None => ObjectId::empty_tree(ctx.format),
        };
        let new_tree = commit_tree_oid(&db, ctx.format, &onto)?;
        // Start "Changes from … to …" diffstat: includes the summary lines.
        print_rebase_diffstat(
            &db,
            ctx.format,
            &old_tree,
            &new_tree,
            &ctx.config,
            ctx.lazy_fetch,
            true,
        )?;
    }

    // The apply backend's explicit fast-forward case.
    if allow_preemptive_ff && !force && branch_base.as_ref() == Some(&orig_head) {
        // onto is a descendant of orig_head: fast-forward.
        rdrive::reset_index_and_worktree_to_commit_for_rebase(&rctx, &hosts, &onto)?;
        let committer = committer_identity_for_reflog(&ctx.config)?;
        detach_head_with_reflog(
            ctx,
            orig_head,
            onto,
            ctx.reflog("start", Some(&format!("checkout {onto_name}"))),
            committer.clone(),
        )?;
        run_rebase_post_checkout_hook(ctx, &orig_head, &onto)?;
        if !args.quiet {
            println!("Fast-forwarded {branch_name} to {onto_name}.");
        }
        if let Some(head_name) = &head_name {
            move_to_original_branch(ctx, head_name, orig_head, onto, committer)?;
        }
        rdrive::finish_rebase_cleanup(&rctx, &hosts);
        return Ok(());
    }

    // Apply backend: generate the `upstream..orig_head` patch series and replay
    // it via the am engine into `.git/rebase-apply/`. This is the only path that
    // honours `git am`-style `--ignore-whitespace` patch fuzzing.
    if use_apply_backend {
        return run_apply_backend(
            ctx,
            &db,
            &args,
            upstream.as_ref(),
            &orig_head,
            &onto,
            &onto_name,
            head_name.as_deref(),
        );
    }

    let opts = MachineOpts {
        quiet: args.quiet,
        verbose: args.verbose,
        signoff: args.signoff,
        allow_ff: !force,
        drop_redundant_commits: empty == EmptyMode::Drop,
        keep_redundant_commits: empty == EmptyMode::Keep,
        reschedule_failed_exec,
        committer_date_is_author_date: args.committer_date_is_author_date,
        ignore_date: args.ignore_date,
        gpg_sign: args.gpg_sign.clone(),
        no_gpg_sign: args.no_gpg_sign,
        strategy: args.strategy.clone(),
        strategy_opts: args.strategy_opts.clone(),
        rerere_autoupdate: args.rerere_autoupdate,
        head_name,
        onto,
        orig_head,
        squash_onto,
    };

    // Generate the todo list.
    let mut items: Vec<RebaseTodoItem> = if let Some(mode) = rebase_merges {
        make_script_with_merges(
            ctx,
            &db,
            upstream.as_ref(),
            &orig_head,
            keep_empty,
            reapply_cherry_picks,
            mode,
            args.root && args.onto_name.is_some(),
        )?
    } else {
        // For `--root --onto <newbase>` there is no upstream to exclude, but git
        // still drops commits already present in the onto (cherry-pick
        // detection against the new base). Use the onto as the patch-id base.
        let cherry_base = if (args.root && args.onto_name.is_some()) || fork_point {
            Some(&onto)
        } else {
            upstream.as_ref()
        };
        let records = make_script_commits(
            ctx,
            &db,
            upstream.as_ref(),
            cherry_base,
            &orig_head,
            keep_empty,
            reapply_cherry_picks,
        )?;
        records
            .iter()
            .map(|record| -> Result<RebaseTodoItem> {
                let parent_tree = match record.parents.first() {
                    Some(parent) => commit_tree_oid(&db, ctx.format, parent)?,
                    None => ObjectId::empty_tree(ctx.format),
                };
                let mut arg = format!("# {}", commit_subject(&record.commit.message));
                if record.commit.tree == parent_tree {
                    arg.push_str(" # empty");
                }
                Ok(RebaseTodoItem {
                    command: TodoCommand::Pick,
                    flags: 0,
                    oid: Some(record.oid),
                    arg,
                    raw: String::new(),
                })
            })
            .collect::<Result<Vec<_>>>()?
    };

    seq::write_rebase_state(&ctx.git_dir, &opts)?;
    let _ = fs::remove_file(ctx.git_dir.join("REBASE_HEAD"));

    if items.is_empty() {
        items.push(RebaseTodoItem {
            command: TodoCommand::Noop,
            flags: 0,
            oid: None,
            arg: String::new(),
            raw: "noop".to_string(),
        });
    }

    // Upstream order: insert update-ref commands first (after each decorated
    // pick), THEN autosquash (which slots fixups in just before the trailing
    // update-refs), THEN exec. The state file records the final ref set.
    if config_update_refs {
        items = add_update_ref_commands(ctx, &items)?;
    }

    if autosquash {
        items = rearrange_squash(ctx, &db, items)?;
    }

    if !args.exec.is_empty() {
        items = add_exec_commands(items, &args.exec);
    }

    if config_update_refs {
        write_rebase_update_refs_state(ctx, &items)?;
    }

    if seq::count_commands(&items) == 0 {
        rdrive::apply_autostash(&rctx, &hosts);
        seq::remove_merge_state(&ctx.git_dir);
        eprintln!("error: nothing to do");
        return Err(GitError::Exit(1));
    }

    rdrive::complete_action(
        &rctx,
        &hosts,
        opts,
        items,
        upstream.as_ref(),
        &onto_name,
        interactive_explicit,
    )
}

// ---------------------------------------------------------------------------
// Apply backend (git rebase --apply via the am engine)
// ---------------------------------------------------------------------------

/// Split a raw committer/author identity (`Name <email> <seconds> <tz>`) into
/// `(name, email, "<seconds> <tz>")`. The date piece is `None` when the line has
/// no `< … >` email delimiters.
fn split_identity(identity: &[u8]) -> Option<(Vec<u8>, Vec<u8>, Option<String>)> {
    let fields = sley_core::split_ident_line(identity)?;
    let date = match (fields.date, fields.tz) {
        (Some(date), Some(tz)) => {
            let date = std::str::from_utf8(date).ok()?;
            let tz = std::str::from_utf8(tz).ok()?;
            format!("{date} {tz}")
        }
        _ => String::new(),
    };
    let date = if date.is_empty() { None } else { Some(date) };
    Some((fields.name.to_vec(), fields.email.to_vec(), date))
}

#[allow(clippy::too_many_arguments)]
fn run_apply_backend(
    ctx: &Ctx,
    db: &FileObjectDatabase,
    args: &RebaseArgs,
    upstream: Option<&ObjectId>,
    orig_head: &ObjectId,
    onto: &ObjectId,
    onto_name: &str,
    head_name: Option<&str>,
) -> Result<()> {
    let rctx = drive_context(ctx);
    let hosts = rebase_hosts(ctx);
    // Build the pick series exactly like the merge backend does (skip merges and
    // empty commits unless --keep-empty), then turn each into an apply patch.
    // For `--root --onto <newbase>` there is no upstream to exclude, but commits
    // already present in the onto must still be dropped (cherry-pick detection
    // against the new base), matching the merge backend.
    let cherry_base = if args.root && args.onto_name.is_some() {
        Some(onto)
    } else {
        upstream
    };
    let reapply_cherry_picks = args.reapply_cherry_picks.unwrap_or(args.keep_base);
    let records = make_script_commits(
        ctx,
        db,
        upstream,
        cherry_base,
        orig_head,
        // The apply backend drops begin-empty commits by default (git am skips
        // empty patches); `--keep-empty` would have forced the merge backend.
        args.keep_empty.unwrap_or(false),
        reapply_cherry_picks,
    )?;

    if records.is_empty() {
        // Nothing to replay: detach onto the new base and finish (matches git's
        // "noop" apply-backend run, which still moves the branch to onto).
        checkout_onto_for_apply(ctx, &rctx, &hosts, db, onto, onto_name, orig_head)?;
        if let Some(head_name) = head_name
            && head_name.starts_with("refs/heads/")
        {
            let committer = committer_identity_for_reflog(&ctx.config)?;
            move_to_original_branch(ctx, head_name, *orig_head, *onto, committer)?;
        }
        // A noop rebase still finishes, so restore any autostash now.
        rdrive::finish_apply_autostash(&rctx, &hosts);
        if !args.quiet {
            eprintln!(
                "Successfully rebased and updated {}.",
                head_name
                    .and_then(|name| name.strip_prefix("refs/heads/"))
                    .unwrap_or("detached HEAD")
            );
        }
        return Ok(());
    }

    let target_encoding = commit_encoding_config(&ctx.git_dir);
    let mut commits = Vec::with_capacity(records.len());
    for record in &records {
        let parent_tree = match record.parents.first() {
            Some(parent) => commit_tree_oid(db, ctx.format, parent)?,
            None => ObjectId::empty_tree(ctx.format),
        };
        let diff = render_tree_to_tree_patch(
            db,
            ctx.format,
            &parent_tree,
            &record.commit.tree,
            ctx.lazy_fetch,
        )?;
        let source_encoding = commit_encoding(&record.commit);
        let author =
            log_reencode_message(&record.commit.author, &source_encoding, &target_encoding);
        let (name, email, date) = split_identity(&author)
            .ok_or_else(|| GitError::InvalidObject("commit author has no identity".into()))?;
        let mut message =
            commit_message_for_commit_encoding(&record.commit, &target_encoding).into_owned();
        if !message.ends_with(b"\n") {
            message.push(b'\n');
        }
        commits.push(sam::RebaseApplyCommit {
            author_name: name,
            author_email: email,
            author_date: date,
            message,
            diff,
            orig_commit: record.oid,
        });
    }

    // The apply backend (git-rebase--am) announces the rewind before replaying.
    if !args.quiet {
        eprintln!("First, rewinding head to replay your work on top of it...");
    }

    // Detach HEAD onto the new base (the am series commits onto it). If the
    // checkout is refused (untracked-file clobber), the rebase never starts:
    // restore the autostash and drop all state so no rebase is left in progress
    // (t3420 #5 — `rebase --quit` must then report "no rebase in progress").
    if let Err(err) = checkout_onto_for_apply(ctx, &rctx, &hosts, db, onto, onto_name, orig_head) {
        rdrive::apply_autostash(&rctx, &hosts);
        seq::remove_merge_state(&ctx.git_dir);
        let _ = fs::remove_dir_all(ctx.git_dir.join("rebase-apply"));
        return Err(err);
    }

    let result = sam::start_rebase_apply(
        &am_engine_ctx(ctx),
        &am_engine_hosts_for(ctx),
        sam::RebaseApplyParams {
            commits,
            quiet: args.quiet,
            signoff: args.signoff,
            committer_date_is_author_date: args.committer_date_is_author_date,
            ignore_date: args.ignore_date,
            ignore_whitespace: args.ignore_whitespace,
            apply_opts: rebase_apply_opts(args),
            rerere_autoupdate: args.rerere_autoupdate,
            head_name: head_name.map(str::to_string),
            orig_head: *orig_head,
            onto: *onto,
        },
    );
    // The series finished cleanly (Ok) iff the whole rebase completed; restore
    // the autostash then. A conflict returns Err and leaves the stash in place
    // for the eventual `--continue`/`--abort` to handle.
    if result.is_ok() {
        rdrive::finish_apply_autostash(&rctx, &hosts);
    }
    result
}

/// Detach HEAD onto `base` for the apply backend, refusing if the checkout would
/// clobber untracked files (mirrors the merge backend's `checkout_onto_base`).
fn checkout_onto_for_apply(
    ctx: &Ctx,
    rctx: &rdrive::RebaseContext,
    hosts: &rdrive::RebaseHosts<'_>,
    db: &FileObjectDatabase,
    base: &ObjectId,
    onto_name: &str,
    orig_head: &ObjectId,
) -> Result<()> {
    let refs = ctx.refs();
    let old = head_commit_oid(refs)?.unwrap_or(ObjectId::null(ctx.format));
    let base_tree = commit_tree_oid(db, ctx.format, base)?;
    let overwritten =
        rdrive::checkout_would_overwrite_untracked(&ctx.git_dir, &ctx.worktree_root, ctx.format, db, &base_tree)?;
    if !overwritten.is_empty() {
        eprintln!(
            "error: The following untracked working tree files would be overwritten by checkout:"
        );
        for path in &overwritten {
            eprintln!("\t{}", String::from_utf8_lossy(path));
        }
        eprintln!("Please move or remove them before you switch branches.");
        eprintln!("Aborting");
        eprintln!("error: could not detach HEAD");
        return Err(GitError::Exit(1));
    }
    rdrive::reset_index_and_worktree_to_commit_for_rebase(rctx, hosts, base)?;
    let committer = committer_identity_for_reflog(&ctx.config)?;
    detach_head_with_reflog(
        ctx,
        old,
        *base,
        ctx.reflog("start", Some(&format!("checkout {onto_name}"))),
        committer,
    )?;
    fs::write(ctx.git_dir.join("ORIG_HEAD"), format!("{orig_head}\n"))?;
    run_rebase_post_checkout_hook(ctx, &old, base)?;
    Ok(())
}

fn default_upstream_name(ctx: &Ctx, refs: &FileRefStore) -> Option<String> {
    let branch = match refs.read_ref("HEAD").ok()?? {
        RefTarget::Symbolic(name) => name.strip_prefix("refs/heads/")?.to_string(),
        RefTarget::Direct(_) => return None,
    };
    let merge = ctx
        .config
        .get("branch", Some(branch.as_str()), "merge")
        .map(str::to_string)?;
    let remote = ctx
        .config
        .get("branch", Some(branch.as_str()), "remote")
        .map(str::to_string)
        .unwrap_or_else(|| ".".to_string());
    let merge_branch = merge.strip_prefix("refs/heads/").unwrap_or(&merge);
    if remote == "." {
        Some(merge_branch.to_string())
    } else {
        Some(format!("refs/remotes/{remote}/{merge_branch}"))
    }
}

fn print_missing_upstream_advice(ctx: &Ctx, refs: &FileRefStore) {
    let branch = match refs.read_ref("HEAD") {
        Ok(Some(RefTarget::Symbolic(name))) => name.strip_prefix("refs/heads/").map(str::to_string),
        _ => None,
    };
    let _ = ctx;
    match &branch {
        Some(_) => println!("There is no tracking information for the current branch."),
        None => println!("You are not currently on a branch."),
    }
    println!("Please specify which branch you want to rebase against.");
    println!("See git-rebase(1) for details.");
    println!();
    println!("    git rebase '<branch>'");
    println!();
    if let Some(branch) = branch {
        println!("If you wish to set tracking information for this branch you can do so with:");
        println!();
        println!("    git branch --set-upstream-to=<remote>/<branch> {branch}");
        println!();
    }
}

fn require_clean_work_tree(ctx: &Ctx, action: &str, with_hint: bool) -> Result<()> {
    let status = crate::collect_short_status(&ctx.worktree_root, &ctx.git_dir, ctx.format)?;
    // git's rebase clean-check runs `has_unstaged_changes` / `has_uncommitted_
    // changes` with `ignore_submodules = 1`, so a submodule that has moved its
    // HEAD or is dirty never blocks the rebase (t3426 "rebase interactive ignores
    // modified submodules"). Skip any gitlink (submodule) path on both sides.
    let has_unstaged = status.iter().any(|entry| {
        !rebase_status_is_submodule(entry)
            && entry.worktree != b' '
            && entry.worktree != b'?'
            && entry.index != b'?'
    });
    let has_staged = status.iter().any(|entry| {
        !rebase_status_is_submodule(entry) && entry.index != b' ' && entry.index != b'?'
    });
    if !has_unstaged && !has_staged {
        return Ok(());
    }
    if has_unstaged {
        eprintln!("error: cannot {action}: You have unstaged changes.");
        if has_staged {
            eprintln!("error: additionally, your index contains uncommitted changes.");
        }
    } else {
        eprintln!("error: cannot {action}: Your index contains uncommitted changes.");
    }
    if with_hint {
        eprintln!("error: Please commit or stash them.");
    }
    Err(GitError::Exit(1))
}

fn rebase_status_is_submodule(entry: &sley_worktree::ShortStatusEntry) -> bool {
    entry.submodule.is_some()
        || [entry.head_mode, entry.index_mode, entry.worktree_mode]
            .into_iter()
            .flatten()
            .any(sley_index::is_gitlink)
}

fn is_linear_history(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    from: &ObjectId,
    to: &ObjectId,
) -> Result<bool> {
    let mut current = *to;
    loop {
        if current == *from {
            return Ok(true);
        }
        let record = read_rev_list_commit_record(db, format, current)?;
        if record.parents.len() != 1 {
            // Reached a root (0 parents) without finding `from`, or a merge
            // (>1 parents) — either way the history to `from` is not linear.
            return Ok(false);
        }
        current = record.parents[0];
    }
}

fn checkout_up_to_date(
    ctx: &Ctx,
    db: &FileObjectDatabase,
    branch: &str,
    oid: &ObjectId,
) -> Result<()> {
    let target_tree = commit_tree_oid(db, ctx.format, oid)?;
    let overwritten = rdrive::checkout_would_overwrite_untracked(
        &ctx.git_dir,
        &ctx.worktree_root,
        ctx.format,
        db,
        &target_tree,
    )?;
    if !overwritten.is_empty() {
        eprintln!(
            "error: The following untracked working tree files would be overwritten by checkout:"
        );
        for path in &overwritten {
            eprintln!("\t{}", String::from_utf8_lossy(path));
        }
        eprintln!("Please move or remove them before you switch branches.");
        eprintln!("Aborting");
        eprintln!("error: could not switch to branch '{branch}'");
        return Err(GitError::Exit(1));
    }
    sley_worktree::reset_index_and_worktree_to_commit_with_process_filter_metadata(
        &ctx.worktree_root,
        &ctx.git_dir,
        ctx.format,
        oid,
        Some(vec![
            ("ref".to_string(), format!("refs/heads/{branch}")),
            ("treeish".to_string(), oid.to_hex()),
        ]),
    )?;
    let refs = ctx.refs();
    let committer = committer_identity_for_reflog(&ctx.config)?;
    let old = head_commit_oid(refs)?.unwrap_or(ObjectId::null(ctx.format));
    let mut tx = refs.transaction();
    tx.update(RefUpdate {
        name: "HEAD".into(),
        expected: None,
        new: RefTarget::Symbolic(format!("refs/heads/{branch}")),
        reflog: Some(ReflogEntry {
            old_oid: old,
            new_oid: *oid,
            committer,
            message: format!("{}: checkout {branch}", ctx.reflog_action).into_bytes(),
        }),
    });
    tx.commit()?;
    run_rebase_post_checkout_hook(ctx, &old, oid)
}

fn move_to_original_branch(
    ctx: &Ctx,
    head_name: &str,
    old_branch_oid: ObjectId,
    new_oid: ObjectId,
    committer: Vec<u8>,
) -> Result<()> {
    let refs = ctx.refs();
    let mut tx = refs.transaction();
    tx.update(RefUpdate {
        name: head_name.into(),
        expected: None,
        new: RefTarget::Direct(new_oid),
        reflog: Some(ReflogEntry {
            old_oid: old_branch_oid,
            new_oid,
            committer: committer.clone(),
            message: ctx.reflog("finish", Some(&format!("{head_name} onto {new_oid}"))),
        }),
    });
    tx.update(RefUpdate {
        name: "HEAD".into(),
        expected: None,
        new: RefTarget::Symbolic(head_name.into()),
        reflog: Some(ReflogEntry {
            old_oid: new_oid,
            new_oid,
            committer,
            message: ctx.reflog("finish", Some(&format!("returning to {head_name}"))),
        }),
    });
    tx.commit()
}

fn print_rebase_diffstat(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    old_tree: &ObjectId,
    new_tree: &ObjectId,
    config: &GitConfig,
    lazy_fetch: bool,
    with_summary: bool,
) -> Result<()> {
    let entries = sley_diff_merge::diff_name_status_trees_with_options(
        db,
        format,
        old_tree,
        new_tree,
        sley_diff_merge::DiffNameStatusOptions {
            detect_renames: true,
            ..Default::default()
        },
    )?;
    if entries.is_empty() {
        return Ok(());
    }
    let mut stdout = io::stdout();
    let stat_entries = collect_diff_stat_entries(entries.as_slice(), db, None, false, crate::diff_lazy_fetch(lazy_fetch))?;
    // The diffstat rows + the "N file changed …" trailer (already emitted by
    // `write_diff_stat`). It is NOT followed by a separate shortstat — emitting
    // one double-printed the "N file changed" line (t3404 "verbose flag is
    // heeded"). Tree-to-tree diff: the "new" side is the target tree's blobs,
    // never the worktree (so `use_worktree_new = false`).
    write_diff_stat_materialized(
        &mut stdout,
        &stat_entries,
        DiffStatOptions {
            compact_summary: false,
            stat_count: None,
            color: false,
            quote_path_fully: true,
        },
        Some(config),
    )?;
    // The "Changes from … to …" start diffstat sets
    // `DIFF_FORMAT_SUMMARY | DIFF_FORMAT_DIFFSTAT` (builtin/rebase.c), so it
    // appends the per-entry create/delete-mode/rename summary lines. The finish
    // diffstat (orig-head..HEAD, sequencer.c) uses `DIFF_FORMAT_DIFFSTAT` only
    // — no summary lines.
    if with_summary {
        for entry in &entries {
            write_diff_summary_entry(&mut stdout, entry)?;
        }
    }
    Ok(())
}

fn create_squash_onto(ctx: &Ctx) -> Result<ObjectId> {
    let ident = commit_identity_from_env("COMMITTER", &ctx.config)?;
    let mut writer = ctx.db();
    sley_sequencer::create_commit(
        &mut writer,
        sley_sequencer::CommitCreate {
            tree: ObjectId::empty_tree(ctx.format),
            parents: Vec::new(),
            author: ident.clone(),
            committer: ident,
            message: Vec::new(),
            encoding: None,
            signature: None,
        },
    )
}


// ---------------------------------------------------------------------------
// make_script: generate pick lines for upstream..orig_head
// ---------------------------------------------------------------------------

/// Shortest unambiguous hex under the repository's `core.abbrev` policy
/// (generation-side; the drive loop uses the engine's host seam).
fn find_unique_abbrev_hex(ctx: &Ctx, db: &FileObjectDatabase, oid: &ObjectId) -> String {
    let hex = oid.to_hex();
    let configured = repository_abbrev(&ctx.git_dir, ctx.format)
        .ok()
        .flatten()
        .unwrap_or(hex.len());
    seq::unique_abbrev(db, oid, configured.min(hex.len()))
}

fn make_script_commits(
    ctx: &Ctx,
    db: &FileObjectDatabase,
    upstream: Option<&ObjectId>,
    cherry_base: Option<&ObjectId>,
    orig_head: &ObjectId,
    keep_empty: bool,
    reapply_cherry_picks: bool,
) -> Result<Vec<sley_rev::CommitRecord>> {
    // Mark everything reachable from upstream.
    let mut excluded = std::collections::HashSet::new();
    if let Some(upstream) = upstream {
        let mut queue = vec![*upstream];
        while let Some(oid) = queue.pop() {
            if !excluded.insert(oid) {
                continue;
            }
            let record = read_rev_list_commit_record(db, ctx.format, oid)?;
            queue.extend(record.parents.iter().copied());
        }
    } else if let Some(cherry_base) = cherry_base {
        // `--root --onto <newbase>` with no upstream: a related onto does not
        // actually go to the root — only back to the merge base of the onto and
        // orig_head (t3421 "does not go to root"). Disjoint histories have no
        // merge base, so the whole branch (to the root) is rebased.
        let bases = merge_bases(&ctx.common_git_dir, db, ctx.format, cherry_base, orig_head)?;
        let mut queue: Vec<ObjectId> = bases;
        while let Some(oid) = queue.pop() {
            if !excluded.insert(oid) {
                continue;
            }
            let record = read_rev_list_commit_record(db, ctx.format, oid)?;
            queue.extend(record.parents.iter().copied());
        }
    }

    // `--cherry-mark` duplicate detection (git make_script: revs.cherry_mark =
    // !reapply_cherry_picks). Build the set of patch-ids carried by the
    // *left-only* side — upstream commits that are not also reachable from the
    // merge base with orig_head — and drop right-side commits whose patch-id
    // matches, so an already-applied commit isn't replayed onto a base that
    // already has it. Skipped when --reapply-cherry-picks is set.
    let upstream_patch_ids: std::collections::HashSet<Vec<u8>> = if reapply_cherry_picks {
        std::collections::HashSet::new()
    } else if let Some(cherry_base) = cherry_base {
        // Bound the comparison to the symmetric difference: only consider
        // cherry-base commits reachable from it but not from the merge base
        // (matching git's `<base>...orig_head` left side). For `--root --onto
        // <newbase>` the base is the onto; unrelated histories have no merge
        // base, so every onto commit's patch-id is considered.
        let bases = merge_bases(&ctx.common_git_dir, db, ctx.format, cherry_base, orig_head)?;
        let mut base_reachable = std::collections::HashSet::new();
        let mut bq: Vec<ObjectId> = bases;
        while let Some(oid) = bq.pop() {
            if !base_reachable.insert(oid) {
                continue;
            }
            let record = read_rev_list_commit_record(db, ctx.format, oid)?;
            bq.extend(record.parents.iter().copied());
        }
        let mut ids = std::collections::HashSet::new();
        let mut uq = vec![*cherry_base];
        let mut seen = std::collections::HashSet::new();
        while let Some(oid) = uq.pop() {
            if base_reachable.contains(&oid) || !seen.insert(oid) {
                continue;
            }
            let record = read_rev_list_commit_record(db, ctx.format, oid)?;
            uq.extend(record.parents.iter().copied());
            if record.parents.len() > 1 {
                continue; // merges carry no single-parent patch-id
            }
            if let Some(id) = commit_patch_id(db, ctx.format, &record, ctx.lazy_fetch)? {
                ids.insert(id);
            }
        }
        ids
    } else {
        std::collections::HashSet::new()
    };
    // Collect the right side.
    let mut records: BTreeMap<ObjectId, sley_rev::CommitRecord> = BTreeMap::new();
    let mut order = Vec::new();
    let mut queue = vec![*orig_head];
    while let Some(oid) = queue.pop() {
        if excluded.contains(&oid) || records.contains_key(&oid) {
            continue;
        }
        let record = read_rev_list_commit_record(db, ctx.format, oid)?;
        queue.extend(record.parents.iter().copied());
        order.push(oid);
        records.insert(oid, record);
    }
    // git make_script ordering: REV_SORT_IN_GRAPH_ORDER + reverse. Build the
    // newest-first topological order with a LIFO frontier, releasing each
    // commit's in-set parents in parent order so the *second* parent is popped
    // first (matching git's prio_queue), then reverse for the oldest-first pick
    // order. A diamond's shared ancestor is held until all its children emit.
    let mut remaining_children: BTreeMap<ObjectId, usize> = BTreeMap::new();
    for record in records.values() {
        for parent in &record.parents {
            if records.contains_key(parent) {
                *remaining_children.entry(*parent).or_insert(0) += 1;
            }
        }
    }
    // Seed the frontier with the no-in-set-children commits (the tips) in
    // discovery order, so orig_head drives the walk.
    let mut stack: Vec<ObjectId> = order
        .iter()
        .filter(|oid| remaining_children.get(*oid).copied().unwrap_or(0) == 0)
        .copied()
        .collect();
    let mut newest_first = Vec::new();
    let mut emitted = std::collections::HashSet::new();
    while let Some(oid) = stack.pop() {
        if !emitted.insert(oid) {
            continue;
        }
        newest_first.push(oid);
        if let Some(record) = records.get(&oid) {
            for parent in &record.parents {
                if records.contains_key(parent) {
                    let count = remaining_children
                        .get_mut(parent)
                        .expect("in-set parent counted");
                    *count -= 1;
                    if *count == 0 {
                        stack.push(*parent);
                    }
                }
            }
        }
    }
    let sorted: Vec<ObjectId> = newest_first.into_iter().rev().collect();
    let mut out = Vec::new();
    for oid in sorted {
        let record = records.remove(&oid).expect("record collected");
        if record.parents.len() > 1 {
            continue; // skip merge commits
        }
        // Skip commits that are empty relative to their parent unless
        // --keep-empty.
        let parent_tree = match record.parents.first() {
            Some(parent) => commit_tree_oid(db, ctx.format, parent)?,
            None => ObjectId::empty_tree(ctx.format),
        };
        if record.commit.tree == parent_tree && !keep_empty {
            continue;
        }
        // Drop commits whose patch already lives upstream (git PATCHSAME). An
        // *empty* commit is never marked PATCHSAME, so only non-empty commits
        // are eligible — matching `!is_empty && (flags & PATCHSAME)`.
        if !upstream_patch_ids.is_empty()
            && record.commit.tree != parent_tree
            && let Some(id) = commit_patch_id(db, ctx.format, &record, ctx.lazy_fetch)?
            && upstream_patch_ids.contains(&id)
        {
            continue;
        }
        out.push(record);
    }
    Ok(out)
}

fn make_script_with_merges(
    ctx: &Ctx,
    db: &FileObjectDatabase,
    upstream: Option<&ObjectId>,
    orig_head: &ObjectId,
    keep_empty: bool,
    reapply_cherry_picks: bool,
    mode: RebaseMergesMode,
    root_with_onto: bool,
) -> Result<Vec<RebaseTodoItem>> {
    let mut excluded = std::collections::HashSet::new();
    if let Some(upstream) = upstream {
        let mut queue = vec![*upstream];
        while let Some(oid) = queue.pop() {
            if !excluded.insert(oid) {
                continue;
            }
            let record = read_rev_list_commit_record(db, ctx.format, oid)?;
            queue.extend(record.parents.iter().copied());
        }
    }

    let upstream_patch_ids: std::collections::HashSet<Vec<u8>> = if reapply_cherry_picks {
        std::collections::HashSet::new()
    } else if let Some(upstream) = upstream {
        let bases = merge_bases(&ctx.common_git_dir, db, ctx.format, upstream, orig_head)?;
        let mut base_reachable = std::collections::HashSet::new();
        let mut bq: Vec<ObjectId> = bases;
        while let Some(oid) = bq.pop() {
            if !base_reachable.insert(oid) {
                continue;
            }
            let record = read_rev_list_commit_record(db, ctx.format, oid)?;
            bq.extend(record.parents.iter().copied());
        }
        let mut ids = std::collections::HashSet::new();
        let mut uq = vec![*upstream];
        let mut seen = std::collections::HashSet::new();
        while let Some(oid) = uq.pop() {
            if base_reachable.contains(&oid) || !seen.insert(oid) {
                continue;
            }
            let record = read_rev_list_commit_record(db, ctx.format, oid)?;
            uq.extend(record.parents.iter().copied());
            if record.parents.len() > 1 {
                continue;
            }
            if let Some(id) = commit_patch_id(db, ctx.format, &record, ctx.lazy_fetch)? {
                ids.insert(id);
            }
        }
        ids
    } else {
        std::collections::HashSet::new()
    };

    let mut records: BTreeMap<ObjectId, sley_rev::CommitRecord> = BTreeMap::new();
    let mut queue = vec![*orig_head];
    while let Some(oid) = queue.pop() {
        if excluded.contains(&oid) || records.contains_key(&oid) {
            continue;
        }
        let record = read_rev_list_commit_record(db, ctx.format, oid)?;
        queue.extend(record.parents.iter().copied());
        records.insert(oid, record);
    }

    let mut indegree: BTreeMap<ObjectId, usize> = BTreeMap::new();
    let mut children: BTreeMap<ObjectId, Vec<ObjectId>> = BTreeMap::new();
    for (oid, record) in &records {
        indegree.entry(*oid).or_insert(0);
        for parent in &record.parents {
            if records.contains_key(parent) {
                *indegree.entry(*oid).or_insert(0) += 1;
                children.entry(*parent).or_default().push(*oid);
            }
        }
    }
    let mut ready: Vec<ObjectId> = indegree
        .iter()
        .filter(|&(_, &deg)| deg == 0)
        .map(|(oid, _)| *oid)
        .collect();
    let mut sorted = Vec::new();
    while let Some(oid) = ready.pop() {
        sorted.push(oid);
        if let Some(kids) = children.get(&oid) {
            for kid in kids.clone() {
                let deg = indegree.get_mut(&kid).expect("child has indegree");
                *deg -= 1;
                if *deg == 0 {
                    ready.push(kid);
                }
            }
        }
    }

    let branch_labels = branch_labels_by_oid(ctx)?;
    let mut state = LabelState::new(ctx, db);
    // Upstream passes `base_rev...orig_head` (symmetric range) to make_script, so
    // the BOTTOM commit registered as "onto" is the merge-base, not `upstream`
    // itself. Registering the merge-base(s) makes the first-parent walk reset
    // "onto" when it reaches the real base (so the rebased branch lands on the
    // new onto) while genuine cousin bases still get their own OID label.
    if let Some(upstream) = upstream {
        let bases = merge_bases(&ctx.common_git_dir, db, ctx.format, upstream, orig_head)?;
        // `merge_bases` may include non-maximal common ancestors here; register
        // only the maximal ones (a true merge-base is not an ancestor of another
        // base), so an ancestor of the real base is not mistakenly labelled onto.
        for (i, base) in bases.iter().enumerate() {
            let dominated = bases.iter().enumerate().any(|(j, other)| {
                i != j
                    && sley_rev::is_ancestor(&ctx.common_git_dir, ctx.format, db, base, other)
                        .unwrap_or(false)
            });
            if !dominated {
                state.register_onto(base);
            }
        }
    }
    let mut commit_todo: BTreeMap<ObjectId, RebaseTodoItem> = BTreeMap::new();
    let mut tips = Vec::new();

    for oid in &sorted {
        let record = records.get(oid).expect("sorted record exists");
        let parent_tree = match record.parents.first() {
            Some(parent) => commit_tree_oid(db, ctx.format, parent)?,
            None => ObjectId::empty_tree(ctx.format),
        };
        let is_empty = record.commit.tree == parent_tree;
        if is_empty && !keep_empty {
            continue;
        }
        if record.parents.len() <= 1
            && !upstream_patch_ids.is_empty()
            && !is_empty
            && let Some(id) = commit_patch_id(db, ctx.format, record, ctx.lazy_fetch)?
            && upstream_patch_ids.contains(&id)
        {
            continue;
        }

        if record.parents.len() > 1 {
            let message_label = merge_label_from_message(&record.commit.message);
            let mut arg = String::new();
            for parent in record.parents.iter().skip(1) {
                if !arg.is_empty() {
                    arg.push(' ');
                }
                let label = if records.contains_key(parent) {
                    if !tips.contains(parent) {
                        tips.push(*parent);
                    }
                    let base = branch_labels
                        .get(parent)
                        .cloned()
                        .unwrap_or_else(|| message_label.clone());
                    state.label_oid(parent, Some(&base))
                } else {
                    state.label_oid(parent, None)
                };
                arg.push_str(&label);
            }
            arg.push_str(" # ");
            arg.push_str(&commit_subject(&record.commit.message));
            commit_todo.insert(
                *oid,
                RebaseTodoItem {
                    command: TodoCommand::Merge,
                    flags: 0,
                    oid: Some(*oid),
                    arg,
                    raw: String::new(),
                },
            );
        } else {
            let mut arg = format!("# {}", commit_subject(&record.commit.message));
            if is_empty {
                arg.push_str(" # empty");
            }
            commit_todo.insert(
                *oid,
                RebaseTodoItem {
                    command: TodoCommand::Pick,
                    flags: 0,
                    oid: Some(*oid),
                    arg,
                    raw: String::new(),
                },
            );
        }
    }

    let mut child_seen = std::collections::HashSet::new();
    for oid in &sorted {
        let record = records.get(oid).expect("sorted record exists");
        for parent in &record.parents {
            if !records.contains_key(parent) {
                continue;
            }
            if !child_seen.insert(*parent) {
                state.label_oid(parent, Some("branch-point"));
            }
        }
    }
    if !tips.contains(orig_head) {
        tips.push(*orig_head);
    }

    let mut out = vec![RebaseTodoItem {
        command: TodoCommand::Label,
        flags: 0,
        oid: None,
        arg: "onto".to_string(),
        raw: String::new(),
    }];
    let mut shown = std::collections::HashSet::new();
    for tip in tips {
        if shown.contains(&tip) {
            continue;
        }
        let Some(mut current) = Some(tip) else {
            continue;
        };
        let branch_label = state.label_of(&tip).map(|s| s.to_string());
        out.push(RebaseTodoItem::comment(""));
        if let Some(label) = branch_label {
            out.push(RebaseTodoItem::comment(&format!("# Branch {label}")));
        }

        let mut list = Vec::new();
        let stop = loop {
            if !records.contains_key(&current) || shown.contains(&current) {
                break Some(current);
            }
            list.push(current);
            let record = records.get(&current).expect("record exists");
            let Some(parent) = record.parents.first().copied() else {
                break None;
            };
            current = parent;
        };
        list.reverse();

        let reset_arg = match stop {
            None => {
                if mode == RebaseMergesMode::RebaseCousins || root_with_onto {
                    "onto".to_string()
                } else {
                    "[new root]".to_string()
                }
            }
            Some(oid) => {
                // Faithful port of upstream phase-3 reset target: prefer an
                // existing label; otherwise (unless rebasing cousins) mint an
                // OID label so the side branch keeps its original base — this
                // is exactly what makes cousins *not* rebase by default.
                let to: Option<String> = if let Some(label) = state.label_of(&oid) {
                    Some(label.to_string())
                } else if mode != RebaseMergesMode::RebaseCousins {
                    Some(state.label_oid(&oid, None))
                } else {
                    None
                };
                match to {
                    None => "onto".to_string(),
                    Some(t) if t == "onto" => "onto".to_string(),
                    Some(t) => {
                        let subject = match records.get(&oid) {
                            Some(rec) => commit_subject(&rec.commit.message),
                            None => commit_subject(
                                &read_rev_list_commit_record(db, ctx.format, oid)?
                                    .commit
                                    .message,
                            ),
                        };
                        format!("{t} # {subject}")
                    }
                }
            }
        };
        out.push(RebaseTodoItem {
            command: TodoCommand::Reset,
            flags: 0,
            oid: None,
            arg: reset_arg,
            raw: String::new(),
        });

        for oid in list {
            if let Some(item) = commit_todo.get(&oid) {
                out.push(item.clone());
            }
            if let Some(label) = state.label_of(&oid) {
                out.push(RebaseTodoItem {
                    command: TodoCommand::Label,
                    flags: 0,
                    oid: None,
                    arg: label.to_string(),
                    raw: String::new(),
                });
            }
            shown.insert(oid);
        }
    }
    Ok(out)
}

fn branch_labels_by_oid(ctx: &Ctx) -> Result<BTreeMap<ObjectId, String>> {
    let refs = ctx.refs();
    let mut out = BTreeMap::new();
    for reference in refs.list_refs()? {
        if let Some(short) = reference.name.strip_prefix("refs/heads/")
            && let RefTarget::Direct(oid) = reference.target
        {
            out.entry(oid).or_insert_with(|| short.to_string());
        }
    }
    Ok(out)
}

/// `GIT_MAX_LABEL_LENGTH` from upstream sequencer.c: `NAME_MAX - LOCK_SUFFIX_LEN
/// - 16`. With `NAME_MAX == 255` and `strlen(".lock") == 5` this is 234.
const GIT_MAX_LABEL_LENGTH: usize = 255 - 5 - 16;

/// Faithful port of upstream sequencer.c `struct label_state` + `label_oid()`.
/// Tracks the commit→label mapping and the set of used labels (case-insensitive,
/// matching upstream's `strihash`), minting labels for branch tips, branch
/// points and cousin bases exactly as upstream does.
struct LabelState<'a> {
    ctx: &'a Ctx,
    db: &'a FileObjectDatabase,
    commit2label: BTreeMap<ObjectId, String>,
    used: std::collections::HashSet<String>,
    max_label_length: usize,
}

impl<'a> LabelState<'a> {
    fn new(ctx: &'a Ctx, db: &'a FileObjectDatabase) -> Self {
        let max_label_length = rebase_config_value(ctx, "rebase", "maxLabelLength")
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(GIT_MAX_LABEL_LENGTH);
        LabelState {
            ctx,
            db,
            commit2label: BTreeMap::new(),
            used: std::collections::HashSet::new(),
            max_label_length,
        }
    }

    /// Pre-register the `onto` commit so a reset that walks back to it resets
    /// onto, and so no other commit can be labelled "onto".
    fn register_onto(&mut self, oid: &ObjectId) {
        self.commit2label.insert(*oid, "onto".to_string());
        self.used.insert("onto".to_string());
    }

    fn label_of(&self, oid: &ObjectId) -> Option<&str> {
        self.commit2label.get(oid).map(|s| s.as_str())
    }

    fn taken(&self, label: &str) -> bool {
        self.used.contains(&label.to_ascii_lowercase())
    }

    /// Port of upstream `label_oid()`. `base == None` for "uninteresting"
    /// commits (use a unique abbreviation, extended on collision); `Some(base)`
    /// sanitizes/truncates the base and disambiguates full-OID/`#`/colliding
    /// labels with a `-N` suffix.
    fn label_oid(&mut self, oid: &ObjectId, base: Option<&str>) -> String {
        if let Some(existing) = self.commit2label.get(oid) {
            return existing.clone();
        }
        let label = match base {
            None => {
                let mut p = find_unique_abbrev_hex(self.ctx, self.db, oid);
                if self.taken(&p) {
                    let hex = oid.to_hex();
                    let mut chosen = hex.clone();
                    for i in (p.len() + 1)..hex.len() {
                        if !self.taken(&hex[..i]) {
                            chosen = hex[..i].to_string();
                            break;
                        }
                    }
                    p = chosen;
                }
                p
            }
            Some(base) => {
                let mut buf = sanitize_label(base, self.max_label_length);
                if buf.is_empty() {
                    buf = format!("rev-{}", find_unique_abbrev_hex(self.ctx, self.db, oid));
                }
                let hexsz = oid.to_hex().len();
                let is_full_hex = buf.len() == hexsz && buf.bytes().all(|b| b.is_ascii_hexdigit());
                if is_full_hex || buf == "#" || self.taken(&buf) {
                    let stem = buf.clone();
                    let mut i = 2;
                    loop {
                        let cand = format!("{stem}-{i}");
                        if !self.taken(&cand) {
                            buf = cand;
                            break;
                        }
                        i += 1;
                    }
                }
                buf
            }
        };
        self.used.insert(label.to_ascii_lowercase());
        self.commit2label.insert(*oid, label.clone());
        label
    }
}

/// Port of upstream label sanitization: keep ASCII alphanumerics and valid
/// multi-byte UTF-8 sequences verbatim, replace runs of other bytes with a
/// single dash (never leading), and truncate to `max_len` bytes without
/// splitting a UTF-8 character. Trailing dashes are intentionally kept.
fn sanitize_label(base: &str, max_len: usize) -> String {
    let bytes = base.as_bytes();
    let mut out: Vec<u8> = Vec::new();
    let mut i = 0;
    let mut label_is_utf8 = true;
    while i < bytes.len() && out.len() + 1 < max_len {
        let b = bytes[i];
        if b.is_ascii_alphanumeric() || (!label_is_utf8 && b & 0x80 != 0) {
            out.push(b);
            i += 1;
        } else if b & 0x80 != 0 {
            match utf8_char_len(&bytes[i..]) {
                Some(n) => {
                    if out.len() + n > max_len {
                        break;
                    }
                    out.extend_from_slice(&bytes[i..i + n]);
                    i += n;
                }
                None => {
                    label_is_utf8 = false;
                    out.push(b);
                    i += 1;
                }
            }
        } else {
            if !out.is_empty() && *out.last().unwrap() != b'-' {
                out.push(b'-');
            }
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Byte length of the valid UTF-8 character starting at `bytes[0]`, or `None`
/// if it is not a well-formed multi-byte sequence.
fn utf8_char_len(bytes: &[u8]) -> Option<usize> {
    let b0 = bytes[0];
    let n = if b0 & 0xE0 == 0xC0 {
        2
    } else if b0 & 0xF0 == 0xE0 {
        3
    } else if b0 & 0xF8 == 0xF0 {
        4
    } else {
        return None;
    };
    if bytes.len() < n {
        return None;
    }
    for &cb in &bytes[1..n] {
        if cb & 0xC0 != 0x80 {
            return None;
        }
    }
    Some(n)
}

fn merge_label_from_message(message: &[u8]) -> String {
    let subject = commit_subject(message);
    if let Some(rest) = subject.strip_prefix("Merge ")
        && let Some(start) = rest.find('\'')
    {
        let after = &rest[start + 1..];
        if let Some(end) = after.find('\'') {
            return after[..end].to_string();
        }
    }
    if let Some(rest) = subject.strip_prefix("Merge pull request ")
        && let Some((_, name)) = rest.split_once(" from ")
    {
        return name.to_string();
    }
    subject
}

/// Patch-id of a single (non-merge) commit's diff against its first parent, for
/// `--cherry-mark` duplicate detection. `None` when the diff is empty.
fn commit_patch_id(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    record: &sley_rev::CommitRecord,
    lazy_fetch: bool,
) -> Result<Option<Vec<u8>>> {
    if record.parents.len() > 1 {
        return Ok(None);
    }
    let parent_tree = match record.parents.first() {
        Some(parent) => commit_tree_oid(db, format, parent)?,
        None => ObjectId::empty_tree(format),
    };
    let diff = render_tree_to_tree_patch(db, format, &parent_tree, &record.commit.tree, lazy_fetch)
        .unwrap_or_default();
    Ok(commands::patch_id::patch_id_for_diff(&diff, format))
}

/// `format_subject(sb, msg, " ")`: the subject paragraph (lines up to the first
/// blank line) folded into one line joined with spaces. Autosquash matches
/// `fixup!`/`squash!` against this folded subject, so a multi-line original
/// subject (`To\nfixup`) is matched by `fixup! To fixup`.
fn format_subject(message: &[u8]) -> String {
    let text = String::from_utf8_lossy(message);
    let mut out = String::new();
    let mut first = true;
    for line in text.lines() {
        if line.trim().is_empty() {
            if first {
                // leading blank lines are skipped
                continue;
            }
            break;
        }
        if !first {
            out.push(' ');
        }
        out.push_str(line);
        first = false;
    }
    out
}

fn format_commit_subject(commit: &Commit) -> String {
    let message = commit_message_for_commit_encoding(commit, "UTF-8");
    format_subject(&message)
}

/// `skip_fixupish`: strip one `fixup! `/`amend! `/`squash! ` prefix, returning
/// the remainder.
fn skip_fixupish(subject: &str) -> Option<&str> {
    subject
        .strip_prefix("fixup! ")
        .or_else(|| subject.strip_prefix("amend! "))
        .or_else(|| subject.strip_prefix("squash! "))
}

/// `todo_list_rearrange_squash`: move `fixup!`/`squash!`/`amend!` commits
/// directly after their targets and rewrite their command. Faithful port of
/// sequencer.c: targets are matched first by exact title, then by commit
/// name (sha/ref) when the remainder has no space, and finally as a prefix of
/// an earlier subject. `amend!` becomes `fixup -C` (replace message).
fn rearrange_squash(
    ctx: &Ctx,
    db: &FileObjectDatabase,
    items: Vec<RebaseTodoItem>,
) -> Result<Vec<RebaseTodoItem>> {
    let n = items.len();
    // Per-item subject (from the commit), or None for drop/comment/no-commit.
    let mut subjects: Vec<Option<String>> = vec![None; n];
    // Title -> first item index with that exact subject.
    let mut subject2item: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    // Resolved commit oid -> item index (for the commit-name match tier).
    let mut commit2item: std::collections::HashMap<ObjectId, usize> =
        std::collections::HashMap::new();
    let mut next = vec![-1i64; n];
    let mut tail = vec![-1i64; n];
    let mut rewritten: Vec<Option<(TodoCommand, u8)>> = vec![None; n];
    let mut rearranged = false;

    for i in 0..n {
        let item = &items[i];
        if item.oid.is_none() || item.command == TodoCommand::Drop {
            continue;
        }
        // The subject is read off the commit, not the (potentially custom)
        // instruction-format arg.
        let record = read_rev_list_commit_record(db, ctx.format, item.oid.expect("checked"))?;
        let subject = format_commit_subject(&record.commit);
        subjects[i] = Some(subject.clone());

        let mut i2: i64 = -1;
        if let Some(mut p) = skip_fixupish(&subject) {
            // Skip any nested fixup!/squash!/amend! prefixes (with whitespace).
            loop {
                p = p.trim_start();
                match skip_fixupish(p) {
                    Some(rest) => p = rest,
                    None => break,
                }
            }
            if let Some(&found) = subject2item.get(p) {
                // found by title
                i2 = found as i64;
            } else if !p.contains(' ')
                && let Ok(oid) = resolve_revision(&ctx.git_dir, ctx.format, p, ctx.replace_objects)
                && let Ok(peeled) = sley_rev::peel_to_commit(db, ctx.format, &oid)
                && let Some(&found) = commit2item.get(&peeled)
            {
                // found by commit name (sha/ref)
                i2 = found as i64;
            } else {
                // copy can be a prefix of the commit subject
                for (j, subj) in subjects.iter().enumerate().take(i) {
                    if let Some(subj) = subj
                        && subj.starts_with(p)
                    {
                        i2 = j as i64;
                        break;
                    }
                }
            }
        }

        if i2 >= 0 {
            rearranged = true;
            let rewrite = if subject.starts_with("fixup!") {
                (TodoCommand::Fixup, 0u8)
            } else if subject.starts_with("amend!") {
                (TodoCommand::Fixup, seq::FLAG_REPLACE_FIXUP_MSG)
            } else {
                (TodoCommand::Squash, 0u8)
            };
            rewritten[i] = Some(rewrite);
            let i2u = i2 as usize;
            if tail[i2u] < 0 {
                next[i] = next[i2u];
                next[i2u] = i as i64;
            } else {
                let t = tail[i2u] as usize;
                next[i] = next[t];
                next[t] = i as i64;
            }
            tail[i2u] = i as i64;
        } else {
            subject2item.entry(subject).or_insert(i);
        }
        commit2item.insert(item.oid.expect("checked"), i);
    }

    if !rearranged {
        return Ok(items);
    }

    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        // Items already rearranged into a chain are emitted from their target.
        if rewritten[i].is_some() {
            continue;
        }
        let mut cur = i as i64;
        while cur >= 0 {
            let idx = cur as usize;
            let mut item = items[idx].clone();
            if let Some((command, flags)) = rewritten[idx] {
                item.command = command;
                item.flags = flags;
            }
            out.push(item);
            cur = next[idx];
        }
    }
    Ok(out)
}

fn add_exec_commands(items: Vec<RebaseTodoItem>, commands: &[String]) -> Vec<RebaseTodoItem> {
    let exec_items = || {
        commands.iter().map(|command| RebaseTodoItem {
            command: TodoCommand::Exec,
            flags: 0,
            oid: None,
            arg: command.clone(),
            raw: format!("exec {command}"),
        })
    };
    let mut out = Vec::new();
    let mut insert = false;
    for item in items {
        if insert && !item.command.is_fixup() {
            out.extend(exec_items());
            insert = false;
        }
        let is_pick = matches!(item.command, TodoCommand::Pick | TodoCommand::Merge);
        out.push(item);
        if is_pick {
            insert = true;
        }
    }
    if insert {
        out.extend(exec_items());
    }
    out
}

fn add_update_ref_commands(ctx: &Ctx, items: &[RebaseTodoItem]) -> Result<Vec<RebaseTodoItem>> {
    let protected = sley_worktree::worktree_refs_in_use(&ctx.git_dir)?;
    let wanted_oids = items
        .iter()
        .filter_map(|item| item.oid)
        .collect::<std::collections::HashSet<_>>();
    if wanted_oids.is_empty() {
        return Ok(items.to_vec());
    }

    let store = &ctx.common_refs;
    let mut refs_by_oid = BTreeMap::<ObjectId, Vec<String>>::new();
    for reference in store.list_refs_with_prefix("refs/heads/")? {
        if protected.contains(&reference.name) {
            continue;
        }
        let Some(oid) = sley_refs::resolve_ref_peeled(store, &reference.name)? else {
            continue;
        };
        if wanted_oids.contains(&oid) {
            refs_by_oid
                .entry(oid)
                .or_default()
                .push(reference.name.clone());
        }
    }
    if refs_by_oid.is_empty() {
        return Ok(items.to_vec());
    }

    let mut out = Vec::new();
    for item in items {
        out.push(item.clone());
        let Some(oid) = item.oid else {
            continue;
        };
        let Some(mut refs) = refs_by_oid.remove(&oid) else {
            continue;
        };
        // git builds the decoration list by prepending each ref as it loads
        // them sorted, so refs at one commit emit in reverse-sorted order.
        refs.sort();
        refs.reverse();
        for refname in refs {
            out.push(RebaseTodoItem {
                command: TodoCommand::UpdateRef,
                flags: 0,
                oid: None,
                arg: refname.clone(),
                raw: format!("update-ref {refname}"),
            });
        }
    }
    Ok(out)
}

fn write_rebase_update_refs_state(ctx: &Ctx, items: &[RebaseTodoItem]) -> Result<()> {
    let mut refs = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for item in items {
        if item.command != TodoCommand::UpdateRef {
            continue;
        }
        let refname = rdrive::todo_arg_before_comment(&item.arg).trim().to_string();
        if seen.insert(refname.clone()) {
            refs.push(refname);
        }
    }
    if refs.is_empty() {
        let _ = fs::remove_file(ctx.state_path("update-refs"));
        return Ok(());
    }
    let store = &ctx.common_refs;
    let zero = ObjectId::null(ctx.format);
    let mut text = String::new();
    for refname in refs {
        let old = sley_refs::resolve_ref_peeled(store, &refname)?.unwrap_or(zero);
        text.push_str(&refname);
        text.push('\n');
        text.push_str(&old.to_hex());
        text.push('\n');
        text.push_str(&zero.to_hex());
        text.push('\n');
    }
    fs::write(ctx.state_path("update-refs"), text)?;
    Ok(())
}







// ---------------------------------------------------------------------------
// complete_action: editor round + checkout onto + drive
// ---------------------------------------------------------------------------






fn launch_sequence_editor(ctx: &Ctx, path: &Path) -> Result<()> {
    let editor = env::var("GIT_SEQUENCE_EDITOR")
        .ok()
        .or_else(|| rebase_config_value(ctx, "sequence", "editor"))
        .or_else(|| env::var("GIT_EDITOR").ok())
        .or_else(|| rebase_config_value(ctx, "core", "editor"))
        .or_else(|| env::var("VISUAL").ok())
        .or_else(|| env::var("EDITOR").ok())
        .unwrap_or_else(|| "vi".to_string());
    if editor == ":" {
        return Ok(());
    }
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$@\""))
        .arg(editor.clone())
        .arg(path)
        .current_dir(&ctx.worktree_root)
        .status()?;
    if !status.success() {
        eprintln!("error: there was a problem with the editor '{editor}'");
        return Err(GitError::Exit(1));
    }
    Ok(())
}





fn run_rebase_post_checkout_hook(
    ctx: &Ctx,
    old_head: &ObjectId,
    new_head: &ObjectId,
) -> Result<()> {
    commands::hooks::run_hook_at(
        &ctx.git_dir,
        "post-checkout",
        commands::hooks::HookRun {
            args: vec![old_head.to_hex(), new_head.to_hex(), "1".to_string()],
            ..commands::hooks::HookRun::default()
        },
    )?;
    Ok(())
}

fn detach_head_with_reflog(
    ctx: &Ctx,
    old_oid: ObjectId,
    new_oid: ObjectId,
    reflog_message: Vec<u8>,
    committer: Vec<u8>,
) -> Result<()> {
    let refs = ctx.refs();
    let mut tx = refs.transaction();
    tx.update(RefUpdate {
        name: "HEAD".into(),
        expected: None,
        new: RefTarget::Direct(new_oid),
        reflog: Some(ReflogEntry {
            old_oid,
            new_oid,
            committer,
            message: reflog_message,
        }),
    });
    tx.commit()
}

// ---------------------------------------------------------------------------
// The drive loop
// ---------------------------------------------------------------------------
























// ---------------------------------------------------------------------------
// Picking one commit
// ---------------------------------------------------------------------------















// ---------------------------------------------------------------------------
// fixup / squash message machinery
// ---------------------------------------------------------------------------











// ---------------------------------------------------------------------------
// Native `git commit` for the machine
// ---------------------------------------------------------------------------










// ---------------------------------------------------------------------------
// Finishing
// ---------------------------------------------------------------------------








/// Match a `notes.rewriteRef` pattern against a concrete ref name. Supports a
/// trailing `*` wildcard (e.g. `refs/notes/*`) and exact names, mirroring the
/// common spellings git's `for_each_glob_ref` accepts here.
fn notes_rewrite_ref_matches(pattern: &str, refname: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => refname.starts_with(prefix),
        None => pattern == refname,
    }
}

/// Copy notes from rewritten commits to their replacements, honouring
/// `notes.rewrite.rebase` (default on), `notes.rewriteRef` (+ the
/// `GIT_NOTES_REWRITE_REF` env list) and `notes.rewriteMode` (default
/// `concatenate`).
fn copy_notes_for_rewrite(ctx: &Ctx, rewritten: &[(ObjectId, ObjectId)]) -> Result<()> {
    if rewritten.is_empty() {
        return Ok(());
    }
    let config = &ctx.config;
    // notes.rewrite.rebase defaults to true; an explicit false disables copying.
    if config.get_bool("notes", Some("rewrite"), "rebase") == Some(false) {
        return Ok(());
    }
    let mut patterns: Vec<String> = config
        .get_all("notes", None, "rewriteRef")
        .into_iter()
        .flatten()
        .map(str::to_string)
        .collect();
    if let Ok(env_refs) = env::var("GIT_NOTES_REWRITE_REF") {
        patterns.extend(
            env_refs
                .split(':')
                .filter(|s| !s.is_empty())
                .map(str::to_string),
        );
    }
    if patterns.is_empty() {
        return Ok(());
    }
    let mode = env::var("GIT_NOTES_REWRITE_MODE")
        .ok()
        .or_else(|| config.get("notes", None, "rewriteMode").map(str::to_string))
        .unwrap_or_else(|| "concatenate".to_string());
    if mode == "ignore" {
        return Ok(());
    }
    let store = ctx.refs();
    let identity = sley_notes::NotesCommitIdentity {
        author: commit_identity_from_env("AUTHOR", &ctx.config)?,
        committer: commit_identity_from_env("COMMITTER", &ctx.config)?,
    };
    for reference in store.list_refs()? {
        if !reference.name.starts_with("refs/notes/")
            || !patterns
                .iter()
                .any(|pattern| notes_rewrite_ref_matches(pattern, &reference.name))
        {
            continue;
        }
        let notes_ref = sley_notes::NotesRef::expand(&reference.name);
        for (old, new) in rewritten {
            let Some(source_blob) =
                sley_notes::read_note_for(&ctx.git_dir, ctx.format, store, &notes_ref, old)?
            else {
                continue;
            };
            let dest_blob =
                sley_notes::read_note_for(&ctx.git_dir, ctx.format, store, &notes_ref, new)?;
            // git's note_tree_insert skips when source and destination notes are
            // the same blob (avoids doubling when a commit is re-rebased to the
            // same id that already carries the copied note).
            if dest_blob == Some(source_blob) {
                continue;
            }
            let source =
                sley_notes::read_note_bytes(&ctx.git_dir, ctx.format, store, &notes_ref, old)?
                    .unwrap_or_default();
            // `overwrite` replaces; concatenate/cat_sort_uniq append to any note
            // already on the replacement commit, separated by a blank line
            // (combine_notes_concatenate).
            let combined = if mode == "overwrite" || dest_blob.is_none() {
                source
            } else {
                let mut cur =
                    sley_notes::read_note_bytes(&ctx.git_dir, ctx.format, store, &notes_ref, new)?
                        .unwrap_or_default();
                if cur.last() == Some(&b'\n') {
                    cur.pop();
                }
                cur.extend_from_slice(b"\n\n");
                cur.extend_from_slice(&source);
                cur
            };
            let expected = sley_notes::notes_ref_expected(store, &notes_ref)?;
            sley_notes::upsert_note_bytes_for(
                &ctx.git_dir,
                ctx.format,
                store,
                &notes_ref,
                new,
                &combined,
                "Notes added by 'git rebase'",
                &identity,
                expected,
            )?;
        }
    }
    Ok(())
}



// ---------------------------------------------------------------------------
// --continue / --skip / --abort / --quit / --edit-todo
// ---------------------------------------------------------------------------










// ---------------------------------------------------------------------------
// Autostash
// ---------------------------------------------------------------------------
