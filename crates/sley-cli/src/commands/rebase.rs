//! `git rebase` — the merge backend driven by the sequencer todo machine.
//!
//! The on-disk contract (`.git/rebase-merge/`) and the todo instruction sheet
//! live in `sley_sequencer::rebase`; this module is the porcelain: option
//! parsing, todo generation (`sequencer_make_script`), the
//! `complete_action`/`pick_commits` drive loop, `--continue` / `--abort` /
//! `--skip` / `--quit` / `--edit-todo`, and autostash handling.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use crate::commands::merge_rebase::{
    MergePathResult, RenameMergeConfig, commit_tree_oid, directory_renames_config,
    effective_config_with_overrides, head_commit_oid, merge_base_fork_point, merge_bases,
    merge_favor_from_strategy_opts, merge_index_entry, merge_read_blob, merge_remove_worktree_file,
    merge_rename_limit_config, merge_write_worktree_file, print_branch_commit_summary,
    print_commit_shortstat_between_trees, three_way_merge_trees,
    three_way_merge_trees_inner_with_info_opts_and_path_favor, three_way_merge_trees_with_favor,
};
use crate::commands::replay::{comment_char, launch_editor, strip_comment_lines};
use crate::*;
use sley_sequencer::rebase as seq;
use sley_sequencer::rebase::{RebaseTodoItem, TodoCommand};

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
}

impl Ctx {
    fn from_session(cli_session: &crate::session::CliSession) -> Result<Ctx> {
        let repository = cli_session.open_repository()?;
        let git_dir = repository.git_dir().to_path_buf();
        let common_git_dir = repository.common_dir().to_path_buf();
        let worktree_root = worktree_root_for_git_dir(cli_session, &git_dir)?;
        let format = repository.object_format();
        let config = read_repo_config(&git_dir)?;
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

fn reset_index_and_worktree_to_commit_for_rebase(ctx: &Ctx, commit: &ObjectId) -> Result<()> {
    if ctx.recurse_submodules {
        commands::read_tree::reset_index_and_worktree_to_commit(
            &ctx.worktree_root,
            &ctx.git_dir,
            ctx.format,
            commit,
            true,
        )
    } else {
        sley_worktree::reset_index_and_worktree_to_commit_with_process_filter_metadata(
            &ctx.worktree_root,
            &ctx.git_dir,
            ctx.format,
            commit,
            rebase_process_filter_metadata(ctx, commit),
        )?;
        Ok(())
    }
}

fn rebase_process_filter_metadata(
    ctx: &Ctx,
    commit: &ObjectId,
) -> Option<sley_worktree::ProcessFilterMetadata> {
    let mut metadata = Vec::new();
    let head_name = seq::read_state_line(&ctx.git_dir, "head-name")
        .map(|name| name.trim().to_string())
        .or_else(|| ctx.refs().current_branch_ref().ok().flatten());
    if let Some(head_name) = head_name
        && head_name.starts_with("refs/")
    {
        metadata.push(("ref".to_string(), head_name));
    }
    metadata.push(("treeish".to_string(), commit.to_hex()));
    Some(metadata)
}

type MachineOpts = seq::RebaseState;

// ---------------------------------------------------------------------------
// Todo list plumbing
// ---------------------------------------------------------------------------

type TodoList = seq::RebaseTodoList;

fn make_resolver<'a>(
    ctx: &'a Ctx,
    db: &'a FileObjectDatabase,
) -> impl FnMut(&str) -> seq::TodoOidLookup + 'a {
    move |token: &str| {
        let Ok(oid) = resolve_revision(&ctx.git_dir, ctx.format, token) else {
            return seq::TodoOidLookup::Missing;
        };
        let Ok(peeled) = sley_rev::peel_to_commit(db, ctx.format, &oid) else {
            return seq::TodoOidLookup::Missing;
        };
        let Ok(record) = read_rev_list_commit_record(db, ctx.format, peeled) else {
            return seq::TodoOidLookup::Missing;
        };
        seq::TodoOidLookup::Commit {
            oid: record.oid,
            parents: record.parents.len(),
        }
    }
}

fn find_unique_abbrev_hex(ctx: &Ctx, db: &FileObjectDatabase, oid: &ObjectId) -> String {
    let hex = oid.to_hex();
    let configured = repository_abbrev(&ctx.git_dir, ctx.format)
        .ok()
        .flatten()
        .unwrap_or(hex.len());
    seq::unique_abbrev(db, oid, configured.min(hex.len()))
}

/// `merge.conflictStyle` for a rebase pick's 3-way merge (honouring `-c`
/// overrides). diff3 and zdiff3 both add the `|||||||` base section; sley does
/// not yet distinguish the zealous variant.
fn rebase_merge_conflict_style(config: &GitConfig) -> sley_diff_merge::ConflictStyle {
    effective_config_with_overrides(config)
        .get("merge", None, "conflictstyle")
        .map(str::to_string)
        .map(|value| match value.as_str() {
            "diff3" | "zdiff3" => sley_diff_merge::ConflictStyle::Diff3,
            _ => sley_diff_merge::ConflictStyle::Merge,
        })
        .unwrap_or(sley_diff_merge::ConflictStyle::Merge)
}

fn todo_render_options(ctx: &Ctx, short: bool, abbreviate: bool) -> seq::TodoRenderOptions {
    let minimum_abbrev = short.then(|| {
        if abbreviate {
            7
        } else {
            repository_abbrev(&ctx.git_dir, ctx.format)
                .ok()
                .flatten()
                .unwrap_or(ctx.format.hex_len())
        }
    });
    seq::TodoRenderOptions {
        minimum_abbrev,
        abbreviate_commands: abbreviate,
    }
}

#[allow(clippy::too_many_arguments)]
fn write_todo_file(
    ctx: &Ctx,
    path: &Path,
    items: &[RebaseTodoItem],
    short: bool,
    help: bool,
    shortrevisions: Option<&str>,
    shortonto: Option<&str>,
    db: &FileObjectDatabase,
) -> Result<()> {
    let abbreviate =
        help && rebase_config_bool(ctx, "rebase", "abbreviateCommands").unwrap_or(false);
    let mut buf = seq::render_todo_list(db, items, todo_render_options(ctx, short, abbreviate));
    if help {
        let comment = comment_char(&ctx.git_dir) as char;
        let check_error = missing_commit_check_level(ctx) == MissingCommitCheck::Error;
        seq::append_todo_help(
            &mut buf,
            seq::count_commands(items),
            shortrevisions,
            shortonto,
            comment,
            check_error,
        );
    }
    fs::write(path, buf)?;
    Ok(())
}

/// `save_todo`: persist the not-yet-executed tail, append the current item to
/// `done`.
fn save_todo(ctx: &Ctx, todo: &TodoList, db: &FileObjectDatabase, reschedule: bool) -> Result<()> {
    seq::save_rebase_todo_list(&ctx.git_dir, db, todo, reschedule)
}

fn read_populate_todo(ctx: &Ctx, db: &FileObjectDatabase) -> Result<TodoList> {
    let mut resolver = make_resolver(ctx, db);
    match seq::load_rebase_todo_list(
        &ctx.git_dir,
        comment_char(&ctx.git_dir) as char,
        &mut resolver,
    )? {
        seq::LoadTodoListOutcome::Ready(todo) => Ok(todo),
        seq::LoadTodoListOutcome::Invalid { messages } => {
            for message in messages {
                eprintln!("{message}");
            }
            eprintln!("error: please fix this using 'git rebase --edit-todo'.");
            Err(GitError::Exit(1))
        }
    }
}

#[derive(PartialEq, Eq)]
enum MissingCommitCheck {
    Ignore,
    Warn,
    Error,
}

fn missing_commit_check_level(ctx: &Ctx) -> MissingCommitCheck {
    match rebase_config_value(ctx, "rebase", "missingCommitsCheck").as_deref() {
        Some(value) if value.eq_ignore_ascii_case("warn") => MissingCommitCheck::Warn,
        Some(value) if value.eq_ignore_ascii_case("error") => MissingCommitCheck::Error,
        _ => MissingCommitCheck::Ignore,
    }
}

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

fn warn_comment_char_auto(ctx: &Ctx) {
    if !rebase_config_value(ctx, "core", "commentChar")
        .is_some_and(|value| value.eq_ignore_ascii_case("auto"))
    {
        return;
    }
    eprintln!(
        "warning: Support for 'core.commentChar=auto' is deprecated and will be removed in Git 3.0"
    );
    eprintln!("hint: ");
    eprintln!("hint: To use the default comment string (#) please run");
    eprintln!("hint: ");
    eprintln!("hint:     git config unset core.commentChar");
    eprintln!("hint: ");
    eprintln!("hint: To set a custom comment string please run");
    eprintln!("hint: ");
    eprintln!("hint:     git config set core.commentChar <comment string>");
    eprintln!("hint: ");
    eprintln!("hint: where '<comment string>' is the string you wish to use.");
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
        apply_in_progress: commands::am::rebase_apply_in_progress(&ctx.git_dir),
        merge_in_progress: seq::in_progress(&ctx.git_dir),
    });
    if history_plan == seq::HistoryEditPlan::MissingState {
        eprintln!("fatal: no rebase in progress");
        return Err(GitError::Exit(128));
    }
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
                let result = commands::am::rebase_apply_continue(
                    &ctx.git_dir,
                    &ctx.common_git_dir,
                    &ctx.worktree_root,
                    ctx.format,
                    &ctx.config,
                    ctx.lazy_fetch,
                );
                // Ok iff the whole series completed; restore the autostash then
                // (a fresh conflict returns Err and keeps it for the next step).
                if result.is_ok() {
                    finish_apply_autostash(&ctx);
                }
                return result;
            }
            RebaseAction::Skip => {
                let result = commands::am::rebase_apply_skip(
                    &ctx.git_dir,
                    &ctx.common_git_dir,
                    &ctx.worktree_root,
                    ctx.format,
                    &ctx.config,
                    ctx.lazy_fetch,
                );
                if result.is_ok() {
                    finish_apply_autostash(&ctx);
                }
                return result;
            }
            RebaseAction::Abort => {
                let autostash = read_apply_autostash(&ctx);
                let result = commands::am::rebase_apply_abort(
                    &ctx.git_dir,
                    &ctx.worktree_root,
                    ctx.format,
                    &ctx.config,
                    ctx.lazy_fetch,
                );
                // Abort always ends the rebase; restore the autostash on top of
                // the restored orig_head (git applies it after reset).
                if result.is_ok() {
                    if let Some(text) = autostash {
                        apply_save_autostash_text(&ctx, &text, true);
                    }
                    seq::remove_merge_state(&ctx.git_dir);
                }
                return result;
            }
            RebaseAction::Quit => {
                if let Some(text) = read_apply_autostash(&ctx) {
                    apply_save_autostash_text(&ctx, &text, false);
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
        RebaseAction::Continue => return rebase_continue(&ctx),
        RebaseAction::Skip => return rebase_skip(&ctx),
        RebaseAction::Abort => return rebase_abort(&ctx),
        RebaseAction::Quit => return rebase_quit(&ctx),
        RebaseAction::EditTodo => return rebase_edit_todo(&ctx),
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
    let other_strategy_opts: Vec<&String> = args
        .strategy_opts
        .iter()
        .filter(|opt| !(args.ignore_whitespace && opt.as_str() == "ignore-space-change"))
        .collect();
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
        || !other_strategy_opts.is_empty();

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
            None => match default_upstream_name(ctx, &refs) {
                Some(name) => name,
                None => {
                    print_missing_upstream_advice(ctx, &refs);
                    return Err(GitError::Exit(1));
                }
            },
        }
    };
    let mut upstream = if args.root {
        None
    } else {
        let resolved = resolve_revision(&ctx.git_dir, ctx.format, &upstream_name)
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
            } else if let Ok(oid) = resolve_revision(&ctx.git_dir, ctx.format, branch)
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
        create_autostash(ctx, use_apply_backend)?;
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
            apply_autostash(ctx);
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
        )
        .and_then(|oid| sley_rev::peel_to_commit(&db, ctx.format, &oid));
        let right_oid = resolve_revision(
            &ctx.git_dir,
            ctx.format,
            if right.is_empty() { "HEAD" } else { right },
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
        match resolve_revision(&ctx.git_dir, ctx.format, &onto_name)
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
        cleanup_autostash_and_state(ctx);
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
                        apply_autostash(ctx);
                        seq::remove_merge_state(&ctx.git_dir);
                        return Err(err);
                    }
                } else {
                    // The <branch> argument names a non-branch (e.g. a tag): git
                    // still switches to it before reporting up-to-date, so detach
                    // HEAD onto its commit (RESET_HEAD_DETACH path).
                    reset_index_and_worktree_to_commit_for_rebase(ctx, &orig_head)?;
                    let refs = ctx.refs();
                    let old = head_commit_oid(&refs)?.unwrap_or_else(|| ObjectId::null(ctx.format));
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
            finish_rebase_cleanup(ctx);
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
        reset_index_and_worktree_to_commit_for_rebase(&ctx, &onto)?;
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
        finish_rebase_cleanup(ctx);
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
        head_name: head_name.clone(),
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
        let cherry_base = if args.root && args.onto_name.is_some() {
            Some(&onto)
        } else if fork_point {
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
        apply_autostash(ctx);
        seq::remove_merge_state(&ctx.git_dir);
        eprintln!("error: nothing to do");
        return Err(GitError::Exit(1));
    }

    complete_action(
        ctx,
        &db,
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
        checkout_onto_for_apply(ctx, db, onto, onto_name, orig_head)?;
        if let Some(head_name) = head_name
            && head_name.starts_with("refs/heads/")
        {
            let committer = committer_identity_for_reflog(&ctx.config)?;
            move_to_original_branch(ctx, head_name, *orig_head, *onto, committer)?;
        }
        // A noop rebase still finishes, so restore any autostash now.
        finish_apply_autostash(ctx);
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
        commits.push(commands::am::RebaseApplyCommit {
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
    if let Err(err) = checkout_onto_for_apply(ctx, db, onto, onto_name, orig_head) {
        apply_autostash(ctx);
        seq::remove_merge_state(&ctx.git_dir);
        let _ = fs::remove_dir_all(ctx.git_dir.join("rebase-apply"));
        return Err(err);
    }

    let result = commands::am::start_rebase_apply(
        &ctx.git_dir,
        &ctx.common_git_dir,
        &ctx.worktree_root,
        ctx.format,
        commands::am::RebaseApplyParams {
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
        &ctx.config,
        ctx.lazy_fetch,
    );
    // The series finished cleanly (Ok) iff the whole rebase completed; restore
    // the autostash then. A conflict returns Err and leaves the stash in place
    // for the eventual `--continue`/`--abort` to handle.
    if result.is_ok() {
        finish_apply_autostash(ctx);
    }
    result
}

/// Restore an autostash for a completed apply-backend rebase and clean up the
/// stray `rebase-merge/` directory `create_autostash` writes the autostash into
/// (the apply backend otherwise only removes `rebase-apply/`, leaving an empty
/// `rebase-merge/` that the next rebase mistakes for an interrupted one).
fn finish_apply_autostash(ctx: &Ctx) {
    apply_autostash(ctx);
    seq::remove_merge_state(&ctx.git_dir);
}

/// Detach HEAD onto `base` for the apply backend, refusing if the checkout would
/// clobber untracked files (mirrors the merge backend's `checkout_onto_base`).
fn checkout_onto_for_apply(
    ctx: &Ctx,
    db: &FileObjectDatabase,
    base: &ObjectId,
    onto_name: &str,
    orig_head: &ObjectId,
) -> Result<()> {
    let refs = ctx.refs();
    let old = head_commit_oid(&refs)?.unwrap_or(ObjectId::null(ctx.format));
    let base_tree = commit_tree_oid(db, ctx.format, base)?;
    let overwritten = checkout_would_overwrite_untracked(ctx, db, &base_tree)?;
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
    reset_index_and_worktree_to_commit_for_rebase(ctx, base)?;
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
    let overwritten = checkout_would_overwrite_untracked(ctx, db, &target_tree)?;
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
    let old = head_commit_oid(&refs)?.unwrap_or(ObjectId::null(ctx.format));
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
    let stat_entries = collect_diff_stat_entries(entries.as_slice(), db, None, false, lazy_fetch)?;
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

fn rebase_commit_signature(
    ctx: &Ctx,
    opts: &MachineOpts,
    tree: ObjectId,
    parents: &[ObjectId],
    author: &[u8],
    committer: &[u8],
    message: &[u8],
    encoding: Option<Vec<u8>>,
) -> Result<Option<Vec<u8>>> {
    let sign = if opts.no_gpg_sign {
        false
    } else {
        opts.gpg_sign.is_some()
            || ctx
                .config
                .get_bool("commit", None, "gpgsign")
                .unwrap_or(false)
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
    commands::signing::sign_payload(Some(&ctx.config), &unsigned.write(), key.as_deref()).map(Some)
}

// ---------------------------------------------------------------------------
// make_script: generate pick lines for upstream..orig_head
// ---------------------------------------------------------------------------

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
        let mut bq: Vec<ObjectId> = bases.clone();
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
        let mut bq: Vec<ObjectId> = bases.clone();
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
                && let Ok(oid) = resolve_revision(&ctx.git_dir, ctx.format, p)
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
        let Some(oid) = sley_refs::resolve_ref_peeled(&store, &reference.name)? else {
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
        let refname = todo_arg_before_comment(&item.arg).trim().to_string();
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
        let old = sley_refs::resolve_ref_peeled(&store, &refname)?.unwrap_or(zero);
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

/// One `(refname, before, after)` record in the `update-refs` state file. The
/// file stores three lines per ref; `after` is the all-zero OID until the ref's
/// `update-ref` todo command runs (recording the then-current HEAD).
struct UpdateRefRecord {
    refname: String,
    before: ObjectId,
    after: ObjectId,
}

fn read_update_refs_state(ctx: &Ctx) -> Result<Vec<UpdateRefRecord>> {
    let path = ctx.state_path("update-refs");
    let Ok(text) = fs::read_to_string(&path) else {
        return Ok(Vec::new());
    };
    let mut lines = text.lines();
    let mut out = Vec::new();
    while let Some(refname) = lines.next() {
        let (Some(before), Some(after)) = (lines.next(), lines.next()) else {
            break;
        };
        let before = ObjectId::from_hex(ctx.format, before.trim())
            .map_err(|_| GitError::InvalidObject("invalid update-refs before-oid".into()))?;
        let after = ObjectId::from_hex(ctx.format, after.trim())
            .map_err(|_| GitError::InvalidObject("invalid update-refs after-oid".into()))?;
        out.push(UpdateRefRecord {
            refname: refname.to_string(),
            before,
            after,
        });
    }
    Ok(out)
}

fn write_update_refs_records(ctx: &Ctx, records: &[UpdateRefRecord]) -> Result<()> {
    let path = ctx.state_path("update-refs");
    if records.is_empty() {
        let _ = fs::remove_file(&path);
        return Ok(());
    }
    let mut text = String::new();
    for rec in records {
        text.push_str(&rec.refname);
        text.push('\n');
        text.push_str(&rec.before.to_hex());
        text.push('\n');
        text.push_str(&rec.after.to_hex());
        text.push('\n');
    }
    fs::write(&path, text)?;
    Ok(())
}

/// Port of upstream `do_update_ref`: record the current HEAD as the `after`
/// value for `refname` in the update-refs state (applied later at finish).
fn do_update_ref(ctx: &Ctx, refname: &str) -> Result<()> {
    let mut records = read_update_refs_state(ctx)?;
    if records.is_empty() {
        return Ok(());
    }
    let refs = ctx.refs();
    let head = head_commit_oid(&refs)?.unwrap_or(ObjectId::null(ctx.format));
    for rec in &mut records {
        if rec.refname == refname {
            rec.after = head;
            break;
        }
    }
    write_update_refs_records(ctx, &records)
}

/// Port of upstream `do_update_refs`: at finish, apply every recorded ref
/// update (compare-and-swap `before` -> `after`), reporting the refs updated
/// and any that failed (e.g. moved out from under the rebase). Refs are
/// reported sorted by name. Returns an error if any update failed.
fn do_update_refs(ctx: &Ctx, quiet: bool) -> Result<()> {
    let mut records = read_update_refs_state(ctx)?;
    if records.is_empty() {
        return Ok(());
    }
    records.sort_by(|a, b| a.refname.cmp(&b.refname));
    let refs = &ctx.common_refs;
    let committer = committer_identity_for_reflog(&ctx.config)?;
    let zero = ObjectId::null(ctx.format);
    let mut updated = Vec::new();
    let mut failed = Vec::new();
    for rec in &records {
        // Skip refs whose update-ref command never ran (after still zero).
        if rec.after == zero {
            continue;
        }
        let current = sley_refs::resolve_ref_peeled(&refs, &rec.refname)?.unwrap_or(zero);
        if current != rec.before {
            eprintln!("error: update_ref failed for ref '{}': ", rec.refname);
            failed.push(rec.refname.clone());
            continue;
        }
        let precondition = if rec.before == zero {
            sley_refs::RefPrecondition::ExistingMustMatch(RefTarget::Direct(zero))
        } else {
            sley_refs::RefPrecondition::MustExistAndMatch(RefTarget::Direct(rec.before))
        };
        let mut tx = refs.transaction();
        tx.update_to(
            rec.refname.clone(),
            RefTarget::Direct(rec.after),
            precondition,
            Some(ReflogEntry {
                old_oid: rec.before,
                new_oid: rec.after,
                committer: committer.clone(),
                message: b"rewritten during rebase".to_vec(),
            }),
        );
        match tx.commit() {
            Ok(()) => updated.push(rec.refname.clone()),
            Err(_) => {
                eprintln!("error: update_ref failed for ref '{}': ", rec.refname);
                failed.push(rec.refname.clone());
            }
        }
    }
    if !quiet && (!updated.is_empty() || !failed.is_empty()) {
        eprint!("Updated the following refs with --update-refs:\n");
        for refname in &updated {
            eprintln!("\t{refname}");
        }
        if !failed.is_empty() {
            eprint!("Failed to update the following refs with --update-refs:\n");
            for refname in &failed {
                eprintln!("\t{refname}");
            }
        }
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(GitError::Exit(1))
    }
}

/// Port of upstream `todo_list_filter_update_refs`: after the todo is (re)read
/// on `--continue`/`--edit-todo`, drop state entries whose ref no longer has an
/// `update-ref` line (and was not yet updated), and add un-updated entries for
/// any new `update-ref` lines.
fn filter_update_refs(ctx: &Ctx, items: &[RebaseTodoItem]) -> Result<()> {
    let mut records = read_update_refs_state(ctx)?;
    let zero = ObjectId::null(ctx.format);
    let todo_refs: std::collections::HashSet<&str> = items
        .iter()
        .filter(|item| item.command == TodoCommand::UpdateRef)
        .map(|item| todo_arg_before_comment(&item.arg).trim())
        .collect();
    let mut updated = false;
    let before_len = records.len();
    records.retain(|rec| rec.after != zero || todo_refs.contains(rec.refname.as_str()));
    if records.len() != before_len {
        updated = true;
    }
    let store = &ctx.common_refs;
    for refname in &todo_refs {
        if !records.iter().any(|rec| rec.refname == *refname) {
            let before = sley_refs::resolve_ref_peeled(&store, refname)?.unwrap_or(zero);
            records.push(UpdateRefRecord {
                refname: (*refname).to_string(),
                before,
                after: zero,
            });
            updated = true;
        }
    }
    if updated {
        write_update_refs_records(ctx, &records)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// complete_action: editor round + checkout onto + drive
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn complete_action(
    ctx: &Ctx,
    db: &FileObjectDatabase,
    opts: MachineOpts,
    items: Vec<RebaseTodoItem>,
    upstream: Option<&ObjectId>,
    onto_name: &str,
    interactive: bool,
) -> Result<()> {
    let todo_path = ctx.state_path("git-rebase-todo");
    let backup_path = ctx.state_path("git-rebase-todo.backup");

    let shortonto = find_unique_abbrev_hex(ctx, db, &opts.onto);
    let shorthead = find_unique_abbrev_hex(ctx, db, &opts.orig_head);
    let shortrevisions = match upstream {
        Some(upstream) => {
            let shortrev = find_unique_abbrev_hex(ctx, db, upstream);
            format!("{shortrev}..{shorthead}")
        }
        None => shorthead,
    };

    warn_comment_char_auto(ctx);

    write_todo_file(
        ctx,
        &todo_path,
        &items,
        true,
        true,
        Some(&shortrevisions),
        Some(&shortonto),
        db,
    )?;
    write_todo_file(
        ctx,
        &backup_path,
        &items,
        false,
        true,
        Some(&shortrevisions),
        Some(&shortonto),
        db,
    )?;

    let mut new_items = items;
    if interactive {
        if let Err(err) = launch_sequence_editor(ctx, &todo_path) {
            apply_autostash(ctx);
            seq::remove_merge_state(&ctx.git_dir);
            return Err(err);
        }
        let edited = fs::read_to_string(&todo_path)?;
        let stripped = stripspace_drop_comments(&edited, comment_char(&ctx.git_dir));
        if stripped.trim().is_empty() {
            apply_autostash(ctx);
            seq::remove_merge_state(&ctx.git_dir);
            eprintln!("error: nothing to do");
            return Err(GitError::Exit(1));
        }
        let mut resolver = make_resolver(ctx, db);
        let (parsed, messages) = seq::parse_todo_buffer(
            &stripped,
            false,
            comment_char(&ctx.git_dir) as char,
            &mut resolver,
        );
        if !messages.is_empty() {
            for message in messages {
                eprintln!("{message}");
            }
            print_edit_todo_recovery_advice();
            checkout_onto(ctx, &opts, onto_name)?;
            return Err(GitError::Exit(1));
        }
        // Missing-commit check against the original list.
        if check_todo_dropped_commits(ctx, db, &new_items, &parsed)? {
            checkout_onto(ctx, &opts, onto_name)?;
            return Err(GitError::Exit(1));
        }
        new_items = parsed;
    }

    // Reconcile the update-refs state with the (possibly user-edited) todo:
    // drop refs whose update-ref line was removed, add any newly-inserted ones.
    filter_update_refs(ctx, &new_items)?;

    let mut todo = TodoList {
        items: new_items,
        current: 0,
        done_nr: 0,
        total_nr: 0,
    };

    // skip_unnecessary_picks: leading picks already on the base fast-forward
    // into `done`.
    let mut base = opts.onto;
    if opts.allow_ff {
        let mut skipped = 0usize;
        for item in &todo.items {
            if item.command == TodoCommand::Comment {
                break;
            }
            if item.command != TodoCommand::Pick {
                break;
            }
            let Some(oid) = &item.oid else { break };
            let record = read_rev_list_commit_record(db, ctx.format, *oid)?;
            if record.parents.len() != 1 || record.parents[0] != base {
                break;
            }
            base = *oid;
            skipped += 1;
        }
        if skipped > 0 {
            let done_text = seq::render_todo_list(
                db,
                &todo.items[..skipped],
                todo_render_options(ctx, false, false),
            );
            fs::write(ctx.state_path("done"), done_text)?;
            if todo
                .items
                .get(skipped)
                .is_some_and(|item| item.command.is_fixup())
            {
                for item in &todo.items[..skipped] {
                    if let Some(oid) = item.oid {
                        record_rewritten(ctx, &oid, Some(TodoCommand::Fixup))?;
                    }
                }
            }
            todo.items.drain(..skipped);
            todo.done_nr = skipped;
        }
    }

    let already_on_rebased_head = head_commit_oid(&ctx.refs())? == Some(opts.orig_head);
    if todo.items.is_empty() && already_on_rebased_head && base == opts.orig_head {
        fs::write(ctx.state_path("git-rebase-todo"), b"")?;
        fs::write(ctx.state_path("end"), format!("{}\n", todo.done_nr))?;
        return finish_rebase(ctx, &opts);
    }

    write_todo_file(ctx, &todo_path, &todo.items, false, false, None, None, db)?;
    todo.total_nr = todo.done_nr + seq::count_commands(&todo.items);
    fs::write(ctx.state_path("end"), format!("{}\n", todo.total_nr))?;

    checkout_onto_base(ctx, &opts, onto_name, &base)?;

    pick_commits(ctx, db, &opts, &mut todo)
}

fn stripspace_drop_comments(text: &str, comment: u8) -> String {
    let mut out = String::new();
    let mut blank_pending = false;
    for line in text.lines() {
        let trimmed = line.trim_end();
        if trimmed.as_bytes().first() == Some(&comment) {
            continue;
        }
        if trimmed.is_empty() {
            blank_pending = !out.is_empty();
            continue;
        }
        if blank_pending {
            out.push('\n');
            blank_pending = false;
        }
        out.push_str(trimmed);
        out.push('\n');
    }
    out
}

fn check_todo_dropped_commits(
    ctx: &Ctx,
    db: &FileObjectDatabase,
    old_items: &[RebaseTodoItem],
    new_items: &[RebaseTodoItem],
) -> Result<bool> {
    let level = missing_commit_check_level(ctx);
    if level == MissingCommitCheck::Ignore {
        return Ok(false);
    }
    let seen: std::collections::HashSet<ObjectId> =
        new_items.iter().filter_map(|item| item.oid).collect();
    let mut missing = Vec::new();
    for item in old_items.iter().rev() {
        if item.command == TodoCommand::Drop {
            continue;
        }
        if let Some(oid) = &item.oid
            && !seen.contains(oid)
        {
            missing.push(format!(
                " - {} {}",
                find_unique_abbrev_hex(ctx, db, oid),
                item.arg
            ));
        }
    }
    if missing.is_empty() {
        return Ok(false);
    }
    eprintln!("Warning: some commits may have been dropped accidentally.");
    eprintln!("Dropped commits (newer to older):");
    for line in &missing {
        eprintln!("{line}");
    }
    eprintln!("To avoid this message, use \"drop\" to explicitly remove a commit.");
    eprintln!();
    eprintln!("Use 'git config rebase.missingCommitsCheck' to change the level of warnings.");
    eprintln!("The possible behaviours are: ignore, warn, error.");
    eprintln!();
    if level == MissingCommitCheck::Error {
        eprintln!(
            "You can fix this with 'git rebase --edit-todo' and then run 'git rebase --continue'."
        );
        eprintln!("Or you can abort the rebase with 'git rebase --abort'.");
        fs::write(ctx.state_path("dropped"), b"")?;
        return Ok(true);
    }
    Ok(false)
}

fn print_edit_todo_recovery_advice() {
    eprintln!(
        "You can fix this with 'git rebase --edit-todo' and then run 'git rebase --continue'."
    );
    eprintln!("Or you can abort the rebase with 'git rebase --abort'.");
}

fn check_todo_dropped_commits_against_backup(
    ctx: &Ctx,
    db: &FileObjectDatabase,
    new_items: &[RebaseTodoItem],
) -> Result<bool> {
    let Ok(backup) = fs::read_to_string(ctx.state_path("git-rebase-todo.backup")) else {
        return Ok(false);
    };
    let mut resolver = make_resolver(ctx, db);
    let (backup_items, _) = seq::parse_todo_buffer(
        &backup,
        ctx.state_path("done").exists(),
        comment_char(&ctx.git_dir) as char,
        &mut resolver,
    );
    check_todo_dropped_commits(ctx, db, &backup_items, new_items)
}

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

fn checkout_onto(ctx: &Ctx, opts: &MachineOpts, onto_name: &str) -> Result<()> {
    checkout_onto_base(ctx, opts, onto_name, &opts.onto)
}

/// git's `reset_head`/`unpack_trees` aborts the detach-to-onto when checking out
/// the target tree would clobber an untracked working-tree file (a path present
/// in the onto tree whose worktree file is not tracked in the index and whose
/// content differs). Mirror that precondition: the blind
/// `reset_index_and_worktree_to_commit` would otherwise overwrite the file and
/// leave the rebase half-started (t3404 "abort with error when new base cannot be
/// checked out"). Returns the offending paths (empty ⇒ safe to proceed).
fn checkout_would_overwrite_untracked(
    ctx: &Ctx,
    db: &FileObjectDatabase,
    target_tree: &ObjectId,
) -> Result<Vec<Vec<u8>>> {
    let target = sley_diff_merge::flatten_tree(db, ctx.format, target_tree)?;
    let tracked: std::collections::BTreeSet<Vec<u8>> =
        match sley_worktree::read_repository_index(&ctx.git_dir, ctx.format)? {
            Some(index) => index
                .entries
                .iter()
                .filter(|entry| entry.stage() == sley_index::Stage::Normal)
                .map(|entry| entry.path.clone().into_bytes())
                .collect(),
            None => std::collections::BTreeSet::new(),
        };
    let mut overwritten = Vec::new();
    for (path, (mode, oid)) in &target {
        if tracked.contains(path) {
            continue;
        }
        // Gitlinks are not materialized as ordinary files; skip them.
        if *mode == 0o160000 {
            continue;
        }
        let Ok(rel) = std::str::from_utf8(path) else {
            continue;
        };
        let worktree_path = ctx.worktree_root.join(rel);
        let Ok(bytes) = fs::read(&worktree_path) else {
            continue;
        };
        let on_disk = sley_core::object_id_for_bytes(ctx.format, "blob", &bytes)?;
        if on_disk != *oid {
            overwritten.push(path.clone());
        }
    }
    overwritten.sort();
    Ok(overwritten)
}

fn print_merge_would_overwrite_untracked(paths: &[Vec<u8>]) {
    eprintln!("error: The following untracked working tree files would be overwritten by merge:");
    for path in paths {
        eprintln!("\t{}", String::from_utf8_lossy(path));
    }
    eprintln!("Please move or remove them before you merge.");
    eprintln!("Aborting");
}

fn checkout_onto_base(
    ctx: &Ctx,
    opts: &MachineOpts,
    onto_name: &str,
    base: &ObjectId,
) -> Result<()> {
    let refs = ctx.refs();
    let old = head_commit_oid(&refs)?.unwrap_or(ObjectId::null(ctx.format));
    let db = ctx.db();
    let base_tree = commit_tree_oid(&db, ctx.format, base)?;
    let overwritten = checkout_would_overwrite_untracked(ctx, &db, &base_tree)?;
    if !overwritten.is_empty() {
        eprintln!(
            "error: The following untracked working tree files would be overwritten by checkout:"
        );
        for path in &overwritten {
            eprintln!("\t{}", String::from_utf8_lossy(path));
        }
        eprintln!("Please move or remove them before you switch branches.");
        eprintln!("Aborting");
        apply_autostash(ctx);
        seq::remove_merge_state(&ctx.git_dir);
        eprintln!("error: could not detach HEAD");
        return Err(GitError::Exit(1));
    }
    if let Err(err) = reset_index_and_worktree_to_commit_for_rebase(ctx, base) {
        apply_autostash(ctx);
        seq::remove_merge_state(&ctx.git_dir);
        eprintln!("error: could not detach HEAD");
        let _ = err;
        return Err(GitError::Exit(1));
    }
    let committer = committer_identity_for_reflog(&ctx.config)?;
    detach_head_with_reflog(
        ctx,
        old,
        *base,
        ctx.reflog("start", Some(&format!("checkout {onto_name}"))),
        committer,
    )?;
    fs::write(
        ctx.git_dir.join("ORIG_HEAD"),
        format!("{}\n", opts.orig_head),
    )?;
    run_rebase_post_checkout_hook(ctx, &old, base)?;
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

fn pick_commits(
    ctx: &Ctx,
    db: &FileObjectDatabase,
    opts: &MachineOpts,
    todo: &mut TodoList,
) -> Result<()> {
    let _ = fs::remove_file(ctx.state_path("message"));
    let _ = fs::remove_file(ctx.state_path("stopped-sha"));
    let _ = fs::remove_file(ctx.state_path("amend"));
    let _ = fs::remove_file(ctx.state_path("patch"));

    while todo.current < todo.items.len() {
        let item = todo.items[todo.current].clone();
        save_todo(ctx, todo, db, false)?;
        if item.command != TodoCommand::Comment {
            todo.done_nr += 1;
            fs::write(ctx.state_path("msgnum"), format!("{}\n", todo.done_nr))?;
            if !opts.quiet {
                let terminator = if opts.verbose { "\n" } else { "\r" };
                eprint!("Rebasing ({}/{}){terminator}", todo.done_nr, todo.total_nr);
            }
        }
        let _ = fs::remove_file(ctx.state_path("author-script"));
        let _ = fs::remove_file(ctx.git_dir.join("MERGE_HEAD"));
        let _ = fs::remove_file(ctx.git_dir.join("AUTO_MERGE"));
        let _ = fs::remove_file(ctx.git_dir.join("REBASE_HEAD"));

        match item.command {
            TodoCommand::Break => {
                stopped_at_head(ctx, db);
                return Ok(());
            }
            TodoCommand::Pick
            | TodoCommand::Reword
            | TodoCommand::Edit
            | TodoCommand::Fixup
            | TodoCommand::Squash => {
                let stop = pick_one_commit(ctx, db, opts, todo, &item)?;
                match stop {
                    PickOutcome::Continue => {}
                    PickOutcome::EditStop => return Ok(()),
                    PickOutcome::Fail(code) => return Err(GitError::Exit(code)),
                }
            }
            TodoCommand::Exec => {
                let status = do_exec(ctx, &item.arg, opts.quiet)?;
                if status != 0 {
                    if opts.reschedule_failed_exec {
                        // Re-insert the exec at the current position.
                        reschedule_current(ctx, db, todo, &item)?;
                    }
                    return Err(GitError::Exit(if status == 127 { 1 } else { status }));
                }
                reread_todo_if_changed(ctx, db, todo)?;
            }
            TodoCommand::Label => {
                do_label(ctx, &item.arg)?;
            }
            TodoCommand::Reset => {
                if let Err(err) = do_reset(ctx, db, opts, &item.arg) {
                    reschedule_current(ctx, db, todo, &item)?;
                    return Err(err);
                }
            }
            TodoCommand::Merge => {
                let stop = do_merge(ctx, db, opts, todo, &item)?;
                match stop {
                    PickOutcome::Continue => {}
                    PickOutcome::EditStop => return Ok(()),
                    PickOutcome::Fail(code) => return Err(GitError::Exit(code)),
                }
            }
            TodoCommand::UpdateRef => {
                let refname = todo_arg_before_comment(&item.arg).trim().to_string();
                do_update_ref(ctx, &refname)?;
            }
            TodoCommand::Noop | TodoCommand::Drop | TodoCommand::Comment | TodoCommand::Revert => {}
        }

        if todo.current == usize::MAX {
            todo.current = 0;
        } else {
            todo.current += 1;
        }
    }

    finish_rebase(ctx, opts)
}

fn reschedule_current(
    ctx: &Ctx,
    db: &FileObjectDatabase,
    todo: &mut TodoList,
    item: &RebaseTodoItem,
) -> Result<()> {
    eprintln!("hint: Could not execute the todo command");
    eprintln!("hint: ");
    eprintln!(
        "hint:     {}",
        seq::render_todo_item(db, item, todo_render_options(ctx, false, false))
    );
    eprintln!("hint: ");
    eprintln!("hint: It has been rescheduled; To edit the command before continuing, please");
    eprintln!("hint: edit the todo list first:");
    eprintln!("hint: ");
    eprintln!("hint:     git rebase --edit-todo");
    eprintln!("hint:     git rebase --continue");
    // Rewrite the todo file with the current item back at the head.
    save_todo(ctx, todo, db, true)?;
    // Trim the duplicated done line: the item was appended to done by the
    // earlier save_todo, matching git (done keeps the failed attempt).
    Ok(())
}

fn reread_todo_if_changed(ctx: &Ctx, db: &FileObjectDatabase, todo: &mut TodoList) -> Result<()> {
    let on_disk = fs::read_to_string(ctx.state_path("git-rebase-todo")).unwrap_or_default();
    let expected = seq::render_todo_list(
        db,
        &todo.items[todo.current + 1..],
        todo_render_options(ctx, false, false),
    );
    if on_disk != expected {
        let mut reloaded = read_populate_todo(ctx, db)?;
        reloaded.done_nr = todo.done_nr;
        reloaded.total_nr = reloaded.done_nr + seq::count_commands(&reloaded.items);
        // current will be incremented by the caller loop; compensate.
        *todo = reloaded;
        todo.current = usize::MAX; // sentinel: wraps to 0 on increment
    }
    Ok(())
}

fn stopped_at_head(ctx: &Ctx, db: &FileObjectDatabase) {
    let refs = ctx.refs();
    match head_commit_oid(&refs) {
        Ok(Some(oid)) => match read_rev_list_commit_record(db, ctx.format, oid) {
            Ok(record) => {
                eprintln!(
                    "Stopped at {}...  {}",
                    find_unique_abbrev_hex(ctx, db, &oid),
                    commit_subject(&record.commit.message)
                );
            }
            Err(_) => eprintln!("Stopped at HEAD"),
        },
        _ => eprintln!("Stopped at HEAD"),
    }
}

fn do_exec(ctx: &Ctx, command: &str, quiet: bool) -> Result<i32> {
    if !quiet {
        eprintln!("Executing: {command}");
    }
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(&ctx.worktree_root)
        .env_remove("GIT_CHERRY_PICK_HELP")
        .status()?;
    let code = status.code().unwrap_or(1);
    let dirty = crate::collect_short_status(&ctx.worktree_root, &ctx.git_dir, ctx.format)
        .map(|status| {
            status
                .iter()
                .any(|entry| (entry.index != b' ' || entry.worktree != b' ') && entry.index != b'?')
        })
        .unwrap_or(false);
    if code != 0 {
        eprintln!(
            "warning: execution failed: {command}\n{}You can fix the problem, and then run\n\n  git rebase --continue\n",
            if dirty {
                "and made changes to the index and/or the working tree.\n"
            } else {
                ""
            }
        );
    } else if dirty {
        eprintln!(
            "warning: execution succeeded: {command}\nbut left changes to the index and/or the working tree.\nCommit or stash your changes, and then run\n\n  git rebase --continue\n"
        );
        return Ok(1);
    }
    Ok(code)
}

fn do_label(ctx: &Ctx, name: &str) -> Result<()> {
    let refs = ctx.refs();
    let head =
        head_commit_oid(&refs)?.ok_or_else(|| GitError::Command("could not read HEAD".into()))?;
    let refname = format!("refs/rewritten/{name}");
    let committer = committer_identity_for_reflog(&ctx.config)?;
    let mut tx = refs.transaction();
    tx.update(RefUpdate {
        name: refname.clone(),
        expected: None,
        new: RefTarget::Direct(head),
        reflog: Some(ReflogEntry {
            old_oid: ObjectId::null(ctx.format),
            new_oid: head,
            committer,
            message: format!("rebase (label) {name}").into_bytes(),
        }),
    });
    tx.commit()
}

fn do_reset(ctx: &Ctx, db: &FileObjectDatabase, opts: &MachineOpts, name: &str) -> Result<()> {
    let name = todo_arg_before_comment(name);
    let target = {
        if name == "[new root]" {
            match opts.squash_onto {
                Some(oid) => oid,
                None => {
                    let oid = create_squash_onto(ctx)?;
                    fs::write(ctx.state_path("squash-onto"), format!("{oid}\n"))?;
                    oid
                }
            }
        } else {
            let refname = format!("refs/rewritten/{name}");
            let refs = ctx.refs();
            match refs.read_ref(&refname)? {
                Some(RefTarget::Direct(oid)) => oid,
                _ if name.starts_with("refs/")
                    || looks_like_object_name(name)
                    || name.contains('^') =>
                {
                    resolve_reset_target(ctx, db, name)?
                }
                _ => {
                    eprintln!("error: could not resolve '{name}'");
                    return Err(GitError::Exit(1));
                }
            }
        }
    };
    let target_tree = commit_tree_oid(db, ctx.format, &target)?;
    let overwritten = checkout_would_overwrite_untracked(ctx, db, &target_tree)?;
    if !overwritten.is_empty() {
        eprintln!(
            "error: The following untracked working tree files would be overwritten by reset:"
        );
        for path in &overwritten {
            eprintln!("\t{}", String::from_utf8_lossy(path));
        }
        eprintln!("Please move or remove them before you reset.");
        return Err(GitError::Exit(1));
    }
    reset_index_and_worktree_to_commit_for_rebase(ctx, &target)?;
    let refs = ctx.refs();
    let old = head_commit_oid(&refs)?.unwrap_or(ObjectId::null(ctx.format));
    let committer = committer_identity_for_reflog(&ctx.config)?;
    detach_head_with_reflog(ctx, old, target, ctx.reflog("reset", Some(name)), committer)
}

/// The active `[new root]` marker commit: `opts.squash_onto` if set, else the
/// squash-onto state file. `reset [new root]` mints this synthetic empty root
/// on the fly (writing only the state file) even for non-`--root` rebases, so
/// both the merge-into-root fast-forward and the pick-as-root-commit paths must
/// consult the file, not just `opts`.
fn effective_squash_onto(ctx: &Ctx, opts: &MachineOpts) -> Option<ObjectId> {
    opts.squash_onto.or_else(|| {
        seq::read_state_line(&ctx.git_dir, "squash-onto")
            .and_then(|raw| ObjectId::from_hex(ctx.format, raw.trim()).ok())
    })
}

fn do_merge(
    ctx: &Ctx,
    db: &FileObjectDatabase,
    opts: &MachineOpts,
    todo: &mut TodoList,
    item: &RebaseTodoItem,
) -> Result<PickOutcome> {
    let (labels, oneline) = parse_merge_todo_arg(&item.arg);
    if labels.is_empty() {
        eprintln!("error: nothing to merge: '{}'", item.arg);
        return Ok(PickOutcome::Fail(1));
    }
    let mut merge_heads = Vec::new();
    for label in &labels {
        match resolve_merge_label(ctx, db, label)? {
            Some(oid) => merge_heads.push((label.clone(), oid)),
            None => {
                eprintln!("error: unable to parse '{label}'");
                return Ok(PickOutcome::Fail(1));
            }
        }
    }

    let refs = ctx.refs();
    let head = head_commit_oid(&refs)?
        .ok_or_else(|| GitError::Command("cannot merge without HEAD".into()))?;
    let original = match item.oid {
        Some(oid) => Some(read_rev_list_commit_record(db, ctx.format, oid)?),
        None => None,
    };

    // A "merge" into a "[new root]" is a fast-forward to the merge head. The
    // synthetic root may have been minted on the fly by an earlier `reset [new
    // root]`, which records it only in the squash-onto state file, so consult
    // that too rather than just `opts.squash_onto`.
    if effective_squash_onto(ctx, opts) == Some(head) {
        if merge_heads.len() > 1 {
            eprintln!("error: octopus merge cannot be executed on top of a [new root]");
            return Ok(PickOutcome::Fail(1));
        }
        let target = merge_heads[0].1;
        reset_index_and_worktree_to_commit_for_rebase(ctx, &target)?;
        let committer = committer_identity_for_reflog(&ctx.config)?;
        detach_head_with_reflog(ctx, head, target, ctx.reflog("merge", None), committer)?;
        return Ok(PickOutcome::Continue);
    }

    if opts.allow_ff
        && let Some(record) = &original
        && record.parents.first() == Some(&head)
        && record.parents[1..]
            .iter()
            .copied()
            .eq(merge_heads.iter().map(|(_, oid)| *oid))
    {
        reset_index_and_worktree_to_commit_for_rebase(ctx, &record.oid)?;
        let committer = committer_identity_for_reflog(&ctx.config)?;
        detach_head_with_reflog(
            ctx,
            head,
            record.oid,
            format!("{}: fast-forward", ctx.reflog_action).into_bytes(),
            committer,
        )?;
        record_rewritten(ctx, &record.oid, next_command_after_current(todo))?;
        if item.flags & seq::FLAG_EDIT_MERGE_MSG != 0 {
            let result = machine_commit(
                ctx,
                db,
                opts,
                MachineCommit {
                    amend: true,
                    edit: true,
                    cleanup_message: true,
                    allow_empty: true,
                    create_root: false,
                    message_file: None,
                    reflog_sub: "merge",
                    original: Some(record),
                },
            )?;
            if let CommitOutcome::Failed(code) = result {
                return Ok(PickOutcome::Fail(code));
            }
            reread_todo_if_changed(ctx, db, todo)?;
        }
        return Ok(PickOutcome::Continue);
    }

    if merge_heads.len() > 1 {
        return do_octopus_merge_commit(
            ctx,
            db,
            opts,
            todo,
            item,
            &merge_heads,
            original.as_ref(),
            oneline,
        );
    }

    let (label, merge_head) = &merge_heads[0];
    if sley_rev::is_ancestor(&ctx.common_git_dir, ctx.format, db, merge_head, &head)? {
        return Ok(PickOutcome::Continue);
    }

    if let Some(strategy) = &opts.strategy
        && custom_rebase_strategy_needs_external_driver(strategy)
    {
        return do_custom_strategy_merge(
            ctx,
            db,
            opts,
            todo,
            item,
            original.as_ref(),
            &labels,
            oneline.as_deref(),
            head,
            *merge_head,
            strategy,
        );
    }

    let merge_tree = commit_tree_oid(db, ctx.format, merge_head)?;
    let overwritten = checkout_would_overwrite_untracked(ctx, db, &merge_tree)?;
    if !overwritten.is_empty() {
        print_merge_would_overwrite_untracked(&overwritten);
        if let Some(record) = &original {
            fs::write(ctx.git_dir.join("REBASE_HEAD"), format!("{}\n", record.oid))?;
        }
        reschedule_current(ctx, db, todo, item)?;
        return Ok(PickOutcome::Fail(1));
    }

    let bases = merge_bases(&ctx.common_git_dir, db, ctx.format, &head, merge_head)?;
    let base_tree = match bases.first() {
        Some(base) => commit_tree_oid(db, ctx.format, base)?,
        None => ObjectId::empty_tree(ctx.format),
    };
    let head_tree = commit_tree_oid(db, ctx.format, &head)?;
    let base_map = sley_diff_merge::flatten_tree(db, ctx.format, &base_tree)?;
    let ours_map = sley_diff_merge::flatten_tree(db, ctx.format, &head_tree)?;
    let theirs_map = sley_diff_merge::flatten_tree(db, ctx.format, &merge_tree)?;
    let write_db = ctx.db();
    let (results, conflicts) = three_way_merge_trees_with_favor(
        &write_db,
        &ctx.config,
        ctx.lazy_fetch,
        ctx.format,
        &base_map,
        &ours_map,
        &theirs_map,
        "HEAD",
        label,
        merge_favor_from_strategy_opts(&opts.strategy_opts),
    )?;

    let message = merge_todo_message(ctx, item, original.as_ref(), &labels, oneline.as_deref())?;
    fs::write(ctx.git_dir.join("MERGE_MSG"), &message)?;
    fs::write(ctx.state_path("message"), &message)?;
    fs::write(ctx.git_dir.join("MERGE_HEAD"), format!("{merge_head}\n"))?;

    apply_merge_results(ctx, db, &results, &ours_map, !conflicts.is_empty())?;
    if !conflicts.is_empty() {
        let merged_tree = sley_worktree::write_tree_from_index(&ctx.git_dir, ctx.format)?;
        fs::write(ctx.git_dir.join("AUTO_MERGE"), format!("{merged_tree}\n"))?;
        for path in &conflicts {
            let display = String::from_utf8_lossy(path);
            if let Some(advice) = rebase_submodule_conflict_advice(&results, path) {
                eprintln!("Failed to merge submodule {display}");
                eprintln!("CONFLICT (submodule): Merge conflict in {display}");
                eprintln!(
                    "Recursive merging with submodules currently only supports trivial cases."
                );
                eprintln!("Please manually handle the merging of each conflicted submodule.");
                eprintln!("This can be accomplished with the following steps:");
                eprintln!(
                    " - go to submodule ({display}), and either merge commit {}",
                    advice.theirs
                );
                eprintln!("   or update to an existing commit which has merged those changes");
                eprintln!(" - come back to superproject and run:");
                eprintln!("      git add {display}");
                eprintln!("   to record the above merge or update");
                eprintln!(" - resolve any other conflicts in the superproject");
                eprintln!(" - commit the resulting index in the superproject");
            } else {
                println!("Auto-merging {display}");
                println!("CONFLICT (content): Merge conflict in {display}");
            }
        }
        let _ = commands::rerere::repo_rerere(
            &ctx.git_dir,
            &ctx.worktree_root,
            ctx.format,
            opts.rerere_autoupdate,
        );
        print_conflict_hints();
        if let Some(record) = &original {
            return stop_with_patch(ctx, db, opts, record, item, 1, false);
        }
        return Ok(PickOutcome::Fail(1));
    }

    let tree = sley_worktree::write_tree_from_index(&ctx.git_dir, ctx.format)?;
    create_merge_commit_from_index(
        ctx,
        opts,
        original.as_ref(),
        tree,
        vec![head, *merge_head],
        &message,
    )?;
    if let Some(record) = &original {
        record_rewritten(ctx, &record.oid, next_command_after_current(todo))?;
    }
    if item.flags & seq::FLAG_EDIT_MERGE_MSG != 0 {
        let result = machine_commit(
            ctx,
            db,
            opts,
            MachineCommit {
                amend: true,
                edit: true,
                cleanup_message: true,
                allow_empty: true,
                create_root: false,
                message_file: None,
                reflog_sub: "merge",
                original: original.as_ref(),
            },
        )?;
        if let CommitOutcome::Failed(code) = result {
            return Ok(PickOutcome::Fail(code));
        }
        reread_todo_if_changed(ctx, db, todo)?;
    }
    Ok(PickOutcome::Continue)
}

fn custom_rebase_strategy_needs_external_driver(strategy: &str) -> bool {
    !matches!(strategy, "ort" | "recursive" | "resolve")
}

fn is_unimplemented_git_core_merge_strategy(strategy: &str) -> bool {
    matches!(strategy, "octopus" | "one-file" | "ours" | "subtree")
}

fn custom_strategy_args(
    opts: &MachineOpts,
    base: ObjectId,
    head: ObjectId,
    merge_head: ObjectId,
) -> Vec<String> {
    let mut command_args: Vec<String> = opts
        .strategy_opts
        .iter()
        .map(|opt| format!("--{opt}"))
        .collect();
    command_args.push(base.to_hex());
    command_args.push("--".to_string());
    command_args.push(head.to_hex());
    command_args.push(merge_head.to_hex());
    command_args
}

fn run_custom_rebase_strategy(
    ctx: &Ctx,
    opts: &MachineOpts,
    strategy: &str,
    base: ObjectId,
    head: ObjectId,
    merge_head: ObjectId,
) -> Result<i32> {
    if is_unimplemented_git_core_merge_strategy(strategy) {
        return Err(GitError::Unsupported(format!(
            "merge strategy '{strategy}' is a Git core helper without a native Sley implementation"
        )));
    }
    let status = std::process::Command::new(format!("git-merge-{strategy}"))
        .args(custom_strategy_args(opts, base, head, merge_head))
        .current_dir(&ctx.worktree_root)
        .status()
        .map_err(|err| GitError::Command(format!("failed to run merge strategy: {err}")))?;
    Ok(status.code().unwrap_or(128))
}

#[allow(clippy::too_many_arguments)]
fn pick_one_commit_with_custom_strategy(
    ctx: &Ctx,
    db: &FileObjectDatabase,
    opts: &MachineOpts,
    todo: &mut TodoList,
    item: &RebaseTodoItem,
    record: &sley_rev::CommitRecord,
    head: ObjectId,
    parent: Option<ObjectId>,
    is_fixup: bool,
    final_fixup: bool,
    strategy: &str,
) -> Result<PickOutcome> {
    let base =
        parent.ok_or_else(|| GitError::Command("custom rebase strategy needs a parent".into()))?;
    let target_encoding = commit_encoding_config(&ctx.git_dir);
    let mut message =
        commit_message_for_commit_encoding(&record.commit, &target_encoding).into_owned();
    if opts.signoff && !is_fixup {
        message = commands::replay::append_signoff_before_comments(
            message,
            &commit_signoff_from_env(&ctx.config)?,
        );
    }
    if is_fixup {
        update_squash_messages(ctx, db, item, record)?;
    }
    write_message_files(ctx, &message, is_fixup, final_fixup)?;
    if !is_fixup {
        fs::write(ctx.state_path("message"), &message)?;
    }
    fs::write(ctx.git_dir.join("MERGE_HEAD"), format!("{}\n", record.oid))?;

    let status = run_custom_rebase_strategy(ctx, opts, strategy, base, head, record.oid)?;
    if status != 0 {
        let _ = commands::rerere::repo_rerere(
            &ctx.git_dir,
            &ctx.worktree_root,
            ctx.format,
            opts.rerere_autoupdate,
        );
        print_conflict_hints();
        return stop_with_patch(ctx, db, opts, record, item, status, false);
    }

    let tree = sley_worktree::write_tree_from_index(&ctx.git_dir, ctx.format)?;
    let head_tree = commit_tree_oid(db, ctx.format, &head)?;
    let parent_tree = commit_tree_oid(db, ctx.format, &base)?;
    let index_unchanged = tree == head_tree;
    let originally_empty = record.commit.tree == parent_tree;
    let mut allow_empty = false;
    if index_unchanged {
        if originally_empty {
            allow_empty = true;
        } else if opts.keep_redundant_commits {
            allow_empty = true;
        } else if opts.drop_redundant_commits {
            eprintln!(
                "dropping {} {} -- patch contents already upstream",
                record.oid,
                commit_subject(&record.commit.message)
            );
            let _ = fs::remove_file(ctx.git_dir.join("MERGE_MSG"));
            let _ = fs::remove_file(ctx.git_dir.join("MERGE_HEAD"));
            return Ok(PickOutcome::Continue);
        } else {
            fs::write(
                ctx.git_dir.join("CHERRY_PICK_HEAD"),
                format!("{}\n", record.oid),
            )?;
            eprintln!(
                "The previous cherry-pick is now empty, possibly due to conflict resolution."
            );
            eprintln!("If you wish to commit it anyway, use:");
            eprintln!();
            eprintln!("    git commit --allow-empty");
            eprintln!();
            eprintln!("Otherwise, please use 'git rebase --skip'");
            return stop_with_patch(ctx, db, opts, record, item, 1, false);
        }
    }

    let (commit_message_file, amend, edit) = if is_fixup {
        if !final_fixup {
            (Some(ctx.state_path("message-squash")), true, false)
        } else if ctx.state_path("message-fixup").exists() {
            (Some(ctx.state_path("message-fixup")), true, false)
        } else {
            (Some(ctx.state_path("message-squash")), true, true)
        }
    } else {
        (Some(ctx.git_dir.join("MERGE_MSG")), false, false)
    };
    let result = machine_commit(
        ctx,
        db,
        opts,
        MachineCommit {
            amend,
            edit: edit || item.command == TodoCommand::Reword,
            cleanup_message: !(is_fixup && !final_fixup),
            allow_empty,
            create_root: false,
            message_file: commit_message_file,
            reflog_sub: command_reflog_name(item.command),
            original: Some(record),
        },
    )?;
    match result {
        CommitOutcome::Committed => {
            record_rewritten(ctx, &record.oid, next_command_after_current(todo))?;
        }
        CommitOutcome::Failed(code) => {
            if is_fixup {
                intend_to_amend(ctx)?;
                let squash = fs::read(ctx.state_path("message-squash")).unwrap_or_default();
                fs::write(ctx.state_path("message"), &squash)?;
                fs::write(ctx.git_dir.join("MERGE_MSG"), &squash)?;
            }
            return stop_with_patch(ctx, db, opts, record, item, code, false);
        }
    }

    if final_fixup {
        let _ = fs::remove_file(ctx.state_path("message-fixup"));
        let _ = fs::remove_file(ctx.state_path("message-squash"));
        let _ = fs::remove_file(ctx.state_path("current-fixups"));
    }
    if item.command == TodoCommand::Edit {
        eprintln!(
            "Stopped at {}...  {}",
            find_unique_abbrev_hex(ctx, db, &record.oid),
            item.arg
        );
        return stop_with_patch(ctx, db, opts, record, item, 0, true);
    }
    if item.command == TodoCommand::Reword || edit {
        reread_todo_if_changed(ctx, db, todo)?;
    }
    Ok(PickOutcome::Continue)
}

#[allow(clippy::too_many_arguments)]
fn do_custom_strategy_merge(
    ctx: &Ctx,
    db: &FileObjectDatabase,
    opts: &MachineOpts,
    todo: &mut TodoList,
    item: &RebaseTodoItem,
    original: Option<&sley_rev::CommitRecord>,
    labels: &[String],
    oneline: Option<&str>,
    head: ObjectId,
    merge_head: ObjectId,
    strategy: &str,
) -> Result<PickOutcome> {
    let message = merge_todo_message(ctx, item, original, labels, oneline)?;
    fs::write(ctx.git_dir.join("MERGE_MSG"), &message)?;
    fs::write(ctx.state_path("message"), &message)?;
    fs::write(ctx.git_dir.join("MERGE_HEAD"), format!("{merge_head}\n"))?;

    let bases = merge_bases(&ctx.common_git_dir, db, ctx.format, &head, &merge_head)?;
    let base = bases
        .first()
        .ok_or_else(|| GitError::Command("custom rebase merge strategy needs a base".into()))?;
    let status = run_custom_rebase_strategy(ctx, opts, strategy, *base, head, merge_head)?;
    if status != 0 {
        return Ok(PickOutcome::Fail(status));
    }

    let tree = sley_worktree::write_tree_from_index(&ctx.git_dir, ctx.format)?;
    create_merge_commit_from_index(ctx, opts, original, tree, vec![head, merge_head], &message)?;
    if let Some(record) = original {
        record_rewritten(ctx, &record.oid, next_command_after_current(todo))?;
    }
    if item.flags & seq::FLAG_EDIT_MERGE_MSG != 0 {
        let result = machine_commit(
            ctx,
            db,
            opts,
            MachineCommit {
                amend: true,
                edit: true,
                cleanup_message: true,
                allow_empty: true,
                create_root: false,
                message_file: None,
                reflog_sub: "merge",
                original,
            },
        )?;
        if let CommitOutcome::Failed(code) = result {
            return Ok(PickOutcome::Fail(code));
        }
        reread_todo_if_changed(ctx, db, todo)?;
    }
    Ok(PickOutcome::Continue)
}

fn todo_arg_before_comment(arg: &str) -> &str {
    arg.split_once(" # ")
        .map(|(left, _)| left.trim())
        .unwrap_or_else(|| arg.trim())
}

fn looks_like_object_name(name: &str) -> bool {
    name.len() >= 7 && name.bytes().all(|b| b.is_ascii_hexdigit())
}

fn resolve_reset_target(ctx: &Ctx, db: &FileObjectDatabase, name: &str) -> Result<ObjectId> {
    let oid = match resolve_revision(&ctx.git_dir, ctx.format, name) {
        Ok(oid) => oid,
        Err(err) => return Err(err),
    };
    match sley_rev::peel_to_commit(db, ctx.format, &oid) {
        Ok(commit) => Ok(commit),
        Err(_) => {
            if let Ok(object) = db.read_object(&oid) {
                eprintln!("error: object {oid} is a {}", object.object_type.as_str());
                return Err(GitError::Exit(1));
            }
            Err(GitError::InvalidObject(format!(
                "{name} does not point to a commit"
            )))
        }
    }
}

fn parse_merge_todo_arg(arg: &str) -> (Vec<String>, Option<String>) {
    let (left, oneline) = match arg.split_once(" # ") {
        Some((left, right)) => (left, Some(right.trim().to_string())),
        None => (arg, None),
    };
    (
        left.split_whitespace().map(str::to_string).collect(),
        oneline,
    )
}

fn resolve_merge_label(
    ctx: &Ctx,
    db: &FileObjectDatabase,
    label: &str,
) -> Result<Option<ObjectId>> {
    let refs = ctx.refs();
    let rewritten = format!("refs/rewritten/{label}");
    if let Some(RefTarget::Direct(oid)) = refs.read_ref(&rewritten)? {
        return Ok(Some(oid));
    }
    match resolve_revision(&ctx.git_dir, ctx.format, label)
        .and_then(|oid| sley_rev::peel_to_commit(db, ctx.format, &oid))
    {
        Ok(oid) => Ok(Some(oid)),
        Err(_) => Ok(None),
    }
}

fn merge_todo_message(
    ctx: &Ctx,
    item: &RebaseTodoItem,
    original: Option<&sley_rev::CommitRecord>,
    labels: &[String],
    oneline: Option<&str>,
) -> Result<Vec<u8>> {
    if let Some(record) = original {
        let target_encoding = commit_encoding_config(&ctx.git_dir);
        let author = commit_author_for_commit_encoding(&record.commit, &target_encoding);
        if let Some(script) = seq::format_author_script(&author) {
            fs::write(ctx.state_path("author-script"), script)?;
        }
        return Ok(
            commit_message_for_commit_encoding(&record.commit, &target_encoding).into_owned(),
        );
    }
    if let Some(oneline) = oneline {
        let mut message = oneline.as_bytes().to_vec();
        message.push(b'\n');
        return Ok(message);
    }
    let message = if labels.len() > 1 {
        format!("Merge branches '{}'\n", labels.join(" "))
    } else {
        format!("Merge branch '{}'\n", labels[0])
    };
    let _ = item;
    Ok(message.into_bytes())
}

fn create_merge_commit_from_index(
    ctx: &Ctx,
    opts: &MachineOpts,
    original: Option<&sley_rev::CommitRecord>,
    tree: ObjectId,
    parents: Vec<ObjectId>,
    message: &[u8],
) -> Result<()> {
    let refs = ctx.refs();
    let head =
        head_commit_oid(&refs)?.ok_or_else(|| GitError::Command("cannot read HEAD".into()))?;
    let target_encoding = commit_encoding_config(&ctx.git_dir);
    let author = match read_author_script_identity(ctx)? {
        Some(identity) => identity,
        None => match original {
            Some(record) => {
                commit_author_for_commit_encoding(&record.commit, &target_encoding).into_owned()
            }
            None => commit_identity_from_env("AUTHOR", &ctx.config)?,
        },
    };
    let (author, committer) = rebase_commit_identities(opts, author, &ctx.config)?;
    let encoding = commit_encoding_header_from_config(&ctx.git_dir);
    let mut writer = ctx.db();
    let new_oid = sley_sequencer::create_commit(
        &mut writer,
        sley_sequencer::CommitCreate {
            tree,
            parents,
            author,
            committer: committer.clone(),
            message: strip_comment_lines(message, comment_char(&ctx.git_dir)),
            encoding,
            signature: None,
        },
    )?;
    let subject = commit_subject(message);
    detach_head_with_reflog(
        ctx,
        head,
        new_oid,
        ctx.reflog("merge", Some(&subject)),
        committer,
    )?;
    let _ = fs::remove_file(ctx.git_dir.join("MERGE_HEAD"));
    let _ = fs::remove_file(ctx.git_dir.join("MERGE_MODE"));
    let _ = fs::remove_file(ctx.git_dir.join("MERGE_MSG"));
    commands::hooks::run_hook_at(
        &ctx.git_dir,
        "post-commit",
        commands::hooks::HookRun::default(),
    )?;
    Ok(())
}

fn do_octopus_merge_commit(
    ctx: &Ctx,
    db: &FileObjectDatabase,
    opts: &MachineOpts,
    todo: &mut TodoList,
    item: &RebaseTodoItem,
    merge_heads: &[(String, ObjectId)],
    original: Option<&sley_rev::CommitRecord>,
    oneline: Option<String>,
) -> Result<PickOutcome> {
    let refs = ctx.refs();
    let head = head_commit_oid(&refs)?
        .ok_or_else(|| GitError::Command("cannot merge without HEAD".into()))?;
    let mut merged_tree = commit_tree_oid(db, ctx.format, &head)?;
    let mut parents = vec![head];
    for (label, oid) in merge_heads {
        if sley_rev::is_ancestor(&ctx.common_git_dir, ctx.format, db, oid, &head)? {
            continue;
        }
        let base = merge_bases(&ctx.common_git_dir, db, ctx.format, &head, oid)?
            .first()
            .copied()
            .map(|base| commit_tree_oid(db, ctx.format, &base))
            .transpose()?
            .unwrap_or_else(|| ObjectId::empty_tree(ctx.format));
        let base_map = sley_diff_merge::flatten_tree(db, ctx.format, &base)?;
        let ours_map = sley_diff_merge::flatten_tree(db, ctx.format, &merged_tree)?;
        let theirs_tree = commit_tree_oid(db, ctx.format, oid)?;
        let theirs_map = sley_diff_merge::flatten_tree(db, ctx.format, &theirs_tree)?;
        let write_db = ctx.db();
        let (results, conflicts) = three_way_merge_trees(
            &write_db,
            &ctx.config,
            ctx.lazy_fetch,
            ctx.format,
            &base_map,
            &ours_map,
            &theirs_map,
            "HEAD",
            label,
        )?;
        apply_merge_results(ctx, db, &results, &ours_map, !conflicts.is_empty())?;
        if !conflicts.is_empty() {
            return Ok(PickOutcome::Fail(1));
        }
        merged_tree = sley_worktree::write_tree_from_index(&ctx.git_dir, ctx.format)?;
        parents.push(*oid);
    }
    if parents.len() == 1 {
        return Ok(PickOutcome::Continue);
    }
    let labels: Vec<String> = merge_heads.iter().map(|(label, _)| label.clone()).collect();
    let message = merge_todo_message(ctx, item, original, &labels, oneline.as_deref())?;
    create_merge_commit_from_index(ctx, opts, original, merged_tree, parents, &message)?;
    if let Some(record) = original {
        record_rewritten(ctx, &record.oid, next_command_after_current(todo))?;
    }
    Ok(PickOutcome::Continue)
}

// ---------------------------------------------------------------------------
// Picking one commit
// ---------------------------------------------------------------------------

enum PickOutcome {
    Continue,
    EditStop,
    Fail(i32),
}

#[allow(clippy::too_many_arguments)]
fn pick_one_commit(
    ctx: &Ctx,
    db: &FileObjectDatabase,
    opts: &MachineOpts,
    todo: &mut TodoList,
    item: &RebaseTodoItem,
) -> Result<PickOutcome> {
    let oid = item.oid.expect("pick-like commands carry a commit");
    let record = read_rev_list_commit_record(db, ctx.format, oid)?;
    let refs = ctx.refs();
    let head =
        head_commit_oid(&refs)?.ok_or_else(|| GitError::Command("cannot read HEAD".into()))?;

    let is_fixup = item.command.is_fixup();
    let final_fixup = is_fixup && !next_is_fixup(todo);
    let create_root = effective_squash_onto(ctx, opts) == Some(head);
    if create_root && is_fixup {
        eprintln!("error: cannot fixup root commit");
        return Ok(PickOutcome::Fail(1));
    }

    let target_encoding = commit_encoding_config(&ctx.git_dir);

    // Write the author script for --continue / commit amending.
    let author = commit_author_for_commit_encoding(&record.commit, &target_encoding);
    if let Some(script) = seq::format_author_script(&author) {
        fs::write(ctx.state_path("author-script"), script)?;
    }

    let parent = record.parents.first().copied();

    // Fast-forward when the pick's parent is exactly HEAD, or when recreating
    // the root (`--root` with no `--onto`): git treats HEAD == squash_onto as
    // "unborn" (sequencer.c), so a parentless commit fast-forwards onto it and
    // is reused as-is, leaving a no-op `--root` rebase at the original commits.
    let ff_to_head = parent == Some(head);
    let ff_root = create_root && parent.is_none();
    if opts.allow_ff && !is_fixup && (ff_to_head || ff_root) {
        let target_tree = commit_tree_oid(db, ctx.format, &oid)?;
        let overwritten = checkout_would_overwrite_untracked(ctx, db, &target_tree)?;
        if !overwritten.is_empty() {
            print_merge_would_overwrite_untracked(&overwritten);
            fs::write(ctx.git_dir.join("REBASE_HEAD"), format!("{oid}\n"))?;
            reschedule_current(ctx, db, todo, item)?;
            return Ok(PickOutcome::Fail(1));
        }
        reset_index_and_worktree_to_commit_for_rebase(ctx, &oid)?;
        let committer = committer_identity_for_reflog(&ctx.config)?;
        detach_head_with_reflog(
            ctx,
            head,
            oid,
            format!("{}: fast-forward", ctx.reflog_action).into_bytes(),
            committer,
        )?;
        match item.command {
            TodoCommand::Reword => {
                // Amend the fast-forwarded commit with an edited message.
                let res = machine_commit(
                    ctx,
                    db,
                    opts,
                    MachineCommit {
                        amend: true,
                        edit: true,
                        cleanup_message: true,
                        allow_empty: true,
                        create_root: false,
                        message_file: None,
                        reflog_sub: "reword",
                        original: Some(&record),
                    },
                )?;
                if let CommitOutcome::Failed(code) = res {
                    return stop_with_patch(ctx, db, opts, &record, item, code, false);
                }
                record_rewritten(ctx, &record.oid, next_command_after_current(todo))?;
                reread_todo_if_changed(ctx, db, todo)?;
                return Ok(PickOutcome::Continue);
            }
            TodoCommand::Edit => {
                eprintln!(
                    "Stopped at {}...  {}",
                    find_unique_abbrev_hex(ctx, db, &oid),
                    item.arg
                );
                return stop_with_patch(ctx, db, opts, &record, item, 0, true);
            }
            _ => {
                record_rewritten(ctx, &record.oid, next_command_after_current(todo))?;
                return Ok(PickOutcome::Continue);
            }
        }
    }

    // Merge the commit's change onto HEAD.
    let parent_tree = match &parent {
        Some(parent) => commit_tree_oid(db, ctx.format, parent)?,
        None => ObjectId::empty_tree(ctx.format),
    };
    let head_tree = commit_tree_oid(db, ctx.format, &head)?;
    let theirs_tree = record.commit.tree;
    let overwritten = checkout_would_overwrite_untracked(ctx, db, &theirs_tree)?;
    if !overwritten.is_empty() {
        print_merge_would_overwrite_untracked(&overwritten);
        fs::write(ctx.git_dir.join("REBASE_HEAD"), format!("{oid}\n"))?;
        reschedule_current(ctx, db, todo, item)?;
        return Ok(PickOutcome::Fail(1));
    }
    if let Some(strategy) = opts.strategy.as_deref().filter(|strategy| {
        custom_rebase_strategy_needs_external_driver(strategy) && parent.is_some()
    }) {
        return pick_one_commit_with_custom_strategy(
            ctx,
            db,
            opts,
            todo,
            item,
            &record,
            head,
            parent,
            is_fixup,
            final_fixup,
            strategy,
        );
    }
    let base_map = sley_diff_merge::flatten_tree(db, ctx.format, &parent_tree)?;
    let ours_map = sley_diff_merge::flatten_tree(db, ctx.format, &head_tree)?;
    let theirs_map = sley_diff_merge::flatten_tree(db, ctx.format, &theirs_tree)?;
    let write_db = ctx.db();
    // The conflict-marker label for the picked side is git's `msg.label`:
    // "<short-oid> (<subject>)" (sequencer.c get_message), not the bare subject.
    let theirs_label = format!(
        "{} ({})",
        find_unique_abbrev_hex(ctx, db, &record.oid),
        commit_subject(&record.commit.message)
    );
    // The base is the parent of the commit being picked, so its diff3 ancestor
    // label is `parent of <msg.label>` (sequencer.c set_replay_opts /
    // `parent_label`). Honour merge.conflictStyle for the picked merge.
    let ancestor_label = format!("parent of {theirs_label}");
    let (results, conflicts, _info) = three_way_merge_trees_inner_with_info_opts_and_path_favor(
        &write_db,
        ctx.format,
        &base_map,
        &ours_map,
        &theirs_map,
        "HEAD",
        &theirs_label,
        &ancestor_label,
        merge_favor_from_strategy_opts(&opts.strategy_opts),
        rebase_merge_conflict_style(&ctx.config),
        rebase_ws_ignore_from_strategy_opts(&opts.strategy_opts),
        RenameMergeConfig {
            detect_renames: true,
            rename_threshold: sley_diff_merge::DEFAULT_RENAME_THRESHOLD,
            rename_limit: merge_rename_limit_config(&ctx.config),
            directory_renames: directory_renames_config(&ctx.config),
            lazy_fetch: ctx.lazy_fetch,
        },
        None,
    )?;

    // Compose the message (fixup/squash machinery).
    let mut message =
        commit_message_for_commit_encoding(&record.commit, &target_encoding).into_owned();
    if opts.signoff && !is_fixup {
        message = commands::replay::append_signoff_before_comments(
            message,
            &commit_signoff_from_env(&ctx.config)?,
        );
    }
    if is_fixup {
        update_squash_messages(ctx, db, item, &record)?;
    }

    let auto_merged_paths: Vec<Vec<u8>> = results
        .iter()
        .filter_map(|(path, result)| {
            if let MergePathResult::Resolved(Some((mode, oid))) = result
                && ours_map.get(path) != Some(&(*mode, *oid))
                && theirs_map.get(path) != base_map.get(path)
                && ours_map.get(path) != base_map.get(path)
                && ours_map.contains_key(path)
            {
                return Some(path.clone());
            }
            None
        })
        .collect();

    apply_merge_results(ctx, db, &results, &ours_map, !conflicts.is_empty())?;

    if !conflicts.is_empty() {
        // Conflict stop.
        let merged_tree = sley_worktree::write_tree_from_index(&ctx.git_dir, ctx.format)?;
        fs::write(ctx.git_dir.join("AUTO_MERGE"), format!("{merged_tree}\n"))?;
        let conflict_set: BTreeSet<Vec<u8>> = conflicts.iter().cloned().collect();
        for path in &auto_merged_paths {
            if !conflict_set.contains(path) {
                println!("Auto-merging {}", String::from_utf8_lossy(path));
            }
        }
        for path in &conflicts {
            let display = String::from_utf8_lossy(path);
            if let Some(advice) = rebase_submodule_conflict_advice(&results, path) {
                eprintln!("Failed to merge submodule {display}");
                eprintln!("CONFLICT (submodule): Merge conflict in {display}");
                eprintln!(
                    "Recursive merging with submodules currently only supports trivial cases."
                );
                eprintln!("Please manually handle the merging of each conflicted submodule.");
                eprintln!("This can be accomplished with the following steps:");
                eprintln!(
                    " - go to submodule ({display}), and either merge commit {}",
                    advice.theirs
                );
                eprintln!("   or update to an existing commit which has merged those changes");
                eprintln!(" - come back to superproject and run:");
                eprintln!("      git add {display}");
                eprintln!("   to record the above merge or update");
                eprintln!(" - resolve any other conflicts in the superproject");
                eprintln!(" - commit the resulting index in the superproject");
            } else {
                println!("Auto-merging {display}");
                println!("CONFLICT (content): Merge conflict in {display}");
            }
        }

        // MERGE_MSG with the conflicts comment block.
        let mut merge_msg = message.clone();
        if !merge_msg.ends_with(b"\n") {
            merge_msg.push(b'\n');
        }
        let comment = comment_char(&ctx.git_dir);
        merge_msg.push(b'\n');
        merge_msg.push(comment);
        merge_msg.extend_from_slice(b" Conflicts:\n");
        for path in &conflicts {
            merge_msg.push(comment);
            merge_msg.push(b'\t');
            merge_msg.extend_from_slice(path);
            merge_msg.push(b'\n');
        }
        if is_fixup {
            // error_failed_squash: message file gets the squash message.
            let squash = fs::read(ctx.state_path("message-squash")).unwrap_or_default();
            fs::write(ctx.state_path("message"), &squash)?;
            fs::write(ctx.git_dir.join("MERGE_MSG"), &squash)?;
            intend_to_amend(ctx)?;
        } else {
            fs::write(ctx.git_dir.join("MERGE_MSG"), &merge_msg)?;
            fs::write(ctx.state_path("message"), &merge_msg)?;
        }

        // Record the conflict in the rerere database and, if a resolution is
        // known, replay it (staging it when rerere.autoUpdate / --rerere-
        // autoupdate is in effect).
        let _ = commands::rerere::repo_rerere(
            &ctx.git_dir,
            &ctx.worktree_root,
            ctx.format,
            opts.rerere_autoupdate,
        );

        eprintln!(
            "error: could not apply {}... {}",
            find_unique_abbrev_hex(ctx, db, &oid),
            commit_subject(&record.commit.message)
        );
        print_conflict_hints();
        return stop_with_patch(ctx, db, opts, &record, item, 1, false);
    }

    for path in &auto_merged_paths {
        println!("Auto-merging {}", String::from_utf8_lossy(path));
    }

    let merged_tree = sley_worktree::write_tree_from_index(&ctx.git_dir, ctx.format)?;

    // Empty handling.
    let index_unchanged = merged_tree == head_tree;
    let originally_empty = theirs_tree == parent_tree;
    let mut allow_empty = false;
    if index_unchanged {
        if originally_empty {
            allow_empty = true; // rebase always allows originally-empty commits
        } else if opts.keep_redundant_commits {
            allow_empty = true;
        } else if opts.drop_redundant_commits {
            eprintln!(
                "dropping {} {} -- patch contents already upstream",
                oid,
                commit_subject(&record.commit.message)
            );
            let _ = fs::remove_file(ctx.git_dir.join("MERGE_MSG"));
            return Ok(PickOutcome::Continue);
        } else {
            // EMPTY_STOP: try to commit without --allow-empty; it fails with
            // the "previous cherry-pick is now empty" advice.
            fs::write(ctx.git_dir.join("CHERRY_PICK_HEAD"), format!("{oid}\n"))?;
            write_message_files(ctx, &message, is_fixup, final_fixup)?;
            eprintln!(
                "The previous cherry-pick is now empty, possibly due to conflict resolution."
            );
            eprintln!("If you wish to commit it anyway, use:");
            eprintln!();
            eprintln!("    git commit --allow-empty");
            eprintln!();
            eprintln!("Otherwise, please use 'git rebase --skip'");
            return stop_with_patch(ctx, db, opts, &record, item, 1, false);
        }
    }

    // Commit.
    write_message_files(ctx, &message, is_fixup, final_fixup)?;
    let (commit_message_file, amend, edit) = if is_fixup {
        if !final_fixup {
            (Some(ctx.state_path("message-squash")), true, false)
        } else if ctx.state_path("message-fixup").exists() {
            (Some(ctx.state_path("message-fixup")), true, false)
        } else {
            (Some(ctx.state_path("message-squash")), true, true)
        }
    } else {
        (Some(ctx.git_dir.join("MERGE_MSG")), false, false)
    };
    if item.command == TodoCommand::Reword && !is_fixup {
        let result = machine_commit(
            ctx,
            db,
            opts,
            MachineCommit {
                amend,
                edit,
                cleanup_message: true,
                allow_empty,
                create_root,
                message_file: commit_message_file.clone(),
                reflog_sub: command_reflog_name(item.command),
                original: Some(&record),
            },
        )?;
        if let CommitOutcome::Failed(code) = result {
            return stop_with_patch(ctx, db, opts, &record, item, code, false);
        }
        let result = machine_commit(
            ctx,
            db,
            opts,
            MachineCommit {
                amend: true,
                edit: true,
                cleanup_message: true,
                allow_empty: true,
                create_root: false,
                message_file: None,
                reflog_sub: command_reflog_name(item.command),
                original: Some(&record),
            },
        )?;
        match result {
            CommitOutcome::Committed => {
                record_rewritten(ctx, &record.oid, next_command_after_current(todo))?;
                reread_todo_if_changed(ctx, db, todo)?;
                return Ok(PickOutcome::Continue);
            }
            CommitOutcome::Failed(code) => {
                return stop_with_patch(ctx, db, opts, &record, item, code, false);
            }
        }
    }
    let result = machine_commit(
        ctx,
        db,
        opts,
        MachineCommit {
            amend,
            edit: edit || item.command == TodoCommand::Reword,
            cleanup_message: !(is_fixup && !final_fixup),
            allow_empty,
            create_root,
            message_file: commit_message_file,
            reflog_sub: command_reflog_name(item.command),
            original: Some(&record),
        },
    )?;
    match result {
        CommitOutcome::Committed => {
            record_rewritten(ctx, &record.oid, next_command_after_current(todo))?;
        }
        CommitOutcome::Failed(code) => {
            if is_fixup {
                intend_to_amend(ctx)?;
                let squash = fs::read(ctx.state_path("message-squash")).unwrap_or_default();
                fs::write(ctx.state_path("message"), &squash)?;
                fs::write(ctx.git_dir.join("MERGE_MSG"), &squash)?;
            }
            return stop_with_patch(ctx, db, opts, &record, item, code, false);
        }
    }

    if final_fixup {
        let _ = fs::remove_file(ctx.state_path("message-fixup"));
        let _ = fs::remove_file(ctx.state_path("message-squash"));
        let _ = fs::remove_file(ctx.state_path("current-fixups"));
    }

    if item.command == TodoCommand::Edit {
        let new_head = head_commit_oid(&ctx.refs())?.expect("just committed");
        eprintln!(
            "Stopped at {}...  {}",
            find_unique_abbrev_hex(ctx, db, &oid),
            item.arg
        );
        let _ = new_head;
        return stop_with_patch(ctx, db, opts, &record, item, 0, true);
    }

    if item.command == TodoCommand::Reword || edit {
        reread_todo_if_changed(ctx, db, todo)?;
    }
    Ok(PickOutcome::Continue)
}

struct RebaseSubmoduleConflictAdvice {
    theirs: String,
}

fn rebase_submodule_conflict_advice(
    results: &BTreeMap<Vec<u8>, MergePathResult>,
    path: &[u8],
) -> Option<RebaseSubmoduleConflictAdvice> {
    let MergePathResult::Conflict {
        base, ours, theirs, ..
    } = results.get(path)?
    else {
        return None;
    };
    if ![base, ours, theirs]
        .into_iter()
        .flatten()
        .any(|(mode, _)| sley_index::is_gitlink(*mode))
    {
        return None;
    }
    let (_, theirs_oid) = theirs.as_ref()?;
    Some(RebaseSubmoduleConflictAdvice {
        theirs: short_oid(theirs_oid),
    })
}

fn short_oid(oid: &ObjectId) -> String {
    oid.to_hex()[..oid.abbrev_hex_len(7)].to_string()
}

fn command_reflog_name(command: TodoCommand) -> &'static str {
    match command {
        TodoCommand::Pick => "pick",
        TodoCommand::Reword => "reword",
        TodoCommand::Edit => "edit",
        TodoCommand::Fixup => "fixup",
        TodoCommand::Squash => "squash",
        _ => "pick",
    }
}

fn next_is_fixup(todo: &TodoList) -> bool {
    todo.items[todo.current + 1..]
        .iter()
        .find(|item| item.command != TodoCommand::Comment)
        .is_some_and(|item| item.command.is_fixup())
}

fn next_command_after_current(todo: &TodoList) -> Option<TodoCommand> {
    todo.items[todo.current + 1..]
        .iter()
        .find(|item| item.command != TodoCommand::Comment)
        .map(|item| item.command)
}

fn first_todo_command(todo: &TodoList) -> Option<TodoCommand> {
    todo.items
        .iter()
        .find(|item| item.command != TodoCommand::Comment)
        .map(|item| item.command)
}

fn write_message_files(
    ctx: &Ctx,
    message: &[u8],
    is_fixup: bool,
    _final_fixup: bool,
) -> Result<()> {
    if !is_fixup {
        fs::write(ctx.git_dir.join("MERGE_MSG"), message)?;
    }
    Ok(())
}

fn apply_merge_results(
    ctx: &Ctx,
    db: &FileObjectDatabase,
    results: &BTreeMap<Vec<u8>, MergePathResult>,
    ours_map: &BTreeMap<Vec<u8>, (u32, ObjectId)>,
    with_conflicts: bool,
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
                        merge_read_blob(db, oid, ctx.lazy_fetch)?
                    };
                    merge_write_worktree_file(&ctx.worktree_root, path, &content, *mode)?;
                }
            }
            MergePathResult::Resolved(None) => {
                if ours_map.contains_key(path) {
                    merge_remove_worktree_file(&ctx.worktree_root, path)?;
                }
            }
            MergePathResult::Conflict { worktree, .. } => {
                if with_conflicts {
                    match worktree {
                        Some((mode, content)) => {
                            merge_write_worktree_file(&ctx.worktree_root, path, content, *mode)?
                        }
                        None => merge_remove_worktree_file(&ctx.worktree_root, path)?,
                    }
                }
            }
        }
    }

    let mut entries = Vec::new();
    for (path, result) in results {
        match result {
            MergePathResult::Resolved(Some((mode, oid))) => {
                let mut entry = merge_index_entry(path, *mode, *oid, 0);
                if !sley_index::is_gitlink(*mode)
                    && let Ok(rel) = std::str::from_utf8(path)
                    && let Ok(metadata) = fs::symlink_metadata(ctx.worktree_root.join(rel))
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
            .then_with(|| index_entry_stage(left).cmp(&index_entry_stage(right)))
    });
    fs::write(
        sley_worktree::repository_index_path(&ctx.git_dir),
        Index {
            version: 2,
            entries,
            extensions: Vec::new(),
            checksum: None,
        }
        .write(ctx.format)?,
    )?;
    Ok(())
}

fn print_conflict_hints() {
    eprintln!("hint: Resolve all conflicts manually, mark them as resolved with");
    eprintln!("hint: \"git add/rm <conflicted_files>\", then run \"git rebase --continue\".");
    eprintln!("hint: You can instead skip this commit: run \"git rebase --skip\".");
    eprintln!(
        "hint: To abort and get back to the state before \"git rebase\", run \"git rebase --abort\"."
    );
    eprintln!("hint: Disable this message with \"git config set advice.mergeConflict false\"");
}

fn intend_to_amend(ctx: &Ctx) -> Result<()> {
    let refs = ctx.refs();
    let head =
        head_commit_oid(&refs)?.ok_or_else(|| GitError::Command("cannot read HEAD".into()))?;
    fs::write(ctx.state_path("amend"), format!("{head}\n"))?;
    Ok(())
}

/// `error_with_patch` / `make_patch`: record stop state and exit.
fn stop_with_patch(
    ctx: &Ctx,
    db: &FileObjectDatabase,
    opts: &MachineOpts,
    record: &sley_rev::CommitRecord,
    _item: &RebaseTodoItem,
    exit_code: i32,
    to_amend: bool,
) -> Result<PickOutcome> {
    fs::write(ctx.state_path("stopped-sha"), format!("{}\n", record.oid))?;
    fs::write(ctx.git_dir.join("REBASE_HEAD"), format!("{}\n", record.oid))?;

    // Write the patch file: diff of the commit against its first parent.
    let parent_tree = match record.parents.first() {
        Some(parent) => commit_tree_oid(db, ctx.format, parent)?,
        None => ObjectId::empty_tree(ctx.format),
    };
    let patch = render_tree_to_tree_patch(
        db,
        ctx.format,
        &parent_tree,
        &record.commit.tree,
        ctx.lazy_fetch,
    )
    .unwrap_or_default();
    fs::write(ctx.state_path("patch"), patch)?;

    if to_amend {
        // An `edit` command has already created the rewritten commit.  Resume
        // must amend that commit with its *current* message, not the original
        // pre-rebase message.  The distinction is observable with options that
        // transform the message while picking (notably `rebase --signoff`):
        // saving `record.commit.message` here made the subsequent `--continue`
        // silently discard the trailer.  Reading HEAD also preserves any
        // editor-driven message change made by the commit step itself.
        let head = head_commit_oid(&ctx.refs())?
            .ok_or_else(|| GitError::Command("cannot read HEAD after edit".into()))?;
        let head_record = read_rev_list_commit_record(db, ctx.format, head)?;
        let mut message = head_record.commit.message;
        if !message.ends_with(b"\n") {
            message.push(b'\n');
        }
        fs::write(ctx.state_path("message"), message)?;
    } else if !ctx.state_path("message").exists() {
        let mut message = record.commit.message.clone();
        if !message.ends_with(b"\n") {
            message.push(b'\n');
        }
        fs::write(ctx.state_path("message"), message)?;
    }

    if to_amend {
        intend_to_amend(ctx)?;
        let sign_opt = opts.gpg_sign.as_ref().map(|key| {
            if key.is_empty() {
                " -S".to_string()
            } else {
                format!(" '-S{key}'")
            }
        });
        eprintln!("You can amend the commit now, with");
        eprintln!();
        eprintln!("  git commit --amend{} ", sign_opt.as_deref().unwrap_or(""));
        eprintln!();
        eprintln!("Once you are satisfied with your changes, run");
        eprintln!();
        eprintln!("  git rebase --continue");
        return Ok(PickOutcome::EditStop);
    }
    if exit_code != 0 {
        // git error_with_patch prints the parsed commit subject (`%.*s`,
        // subject_len/subject), not the raw todo arg (which carries the `# `
        // prefix `pick <oid> # <subject>`).
        eprintln!(
            "Could not apply {}... {}",
            find_unique_abbrev_hex(ctx, db, &record.oid),
            commit_subject(&record.commit.message)
        );
        return Ok(PickOutcome::Fail(exit_code));
    }
    Ok(PickOutcome::EditStop)
}

// ---------------------------------------------------------------------------
// fixup / squash message machinery
// ---------------------------------------------------------------------------

fn current_fixup_count(ctx: &Ctx) -> usize {
    fs::read_to_string(ctx.state_path("current-fixups"))
        .map(|text| text.lines().filter(|line| !line.is_empty()).count())
        .unwrap_or(0)
}

fn commented_lines(text: &[u8], comment: u8) -> Vec<u8> {
    let mut out = Vec::new();
    for line in text.split_inclusive(|&b| b == b'\n') {
        let content = if line.ends_with(b"\n") {
            &line[..line.len() - 1]
        } else {
            line
        };
        if content.is_empty() {
            out.push(comment);
        } else {
            out.push(comment);
            out.push(b' ');
            out.extend_from_slice(content);
        }
        out.push(b'\n');
    }
    out
}

/// `commit_subject_length`: bytes of the subject paragraph (up to and including
/// the blank line that ends it), so `amend!`/`fixup!`/`squash!` subjects can be
/// commented out while the body stays verbatim.
fn commit_subject_length(body: &[u8]) -> usize {
    let mut p = 0usize;
    while p < body.len() {
        // skip_blank_lines: if the current line is blank, the subject ends here.
        let next = skip_blank_lines(&body[p..]) + p;
        if next != p {
            break;
        }
        // advance past this (non-blank) line.
        match body[p..].iter().position(|&b| b == b'\n') {
            Some(off) => p += off + 1,
            None => return body.len(),
        }
    }
    p
}

/// `skip_blank_lines`: return the offset past any leading all-whitespace lines.
fn skip_blank_lines(buf: &[u8]) -> usize {
    let mut p = 0usize;
    loop {
        let eol = buf[p..]
            .iter()
            .position(|&b| b == b'\n')
            .map(|off| p + off)
            .unwrap_or(buf.len());
        if buf[p..eol].iter().any(|&b| !b.is_ascii_whitespace()) {
            return p;
        }
        if eol >= buf.len() {
            return buf.len();
        }
        p = eol + 1;
    }
}

/// `seen_squash`: the current fixup chain contains a `squash` command.
fn seen_squash(ctx: &Ctx) -> bool {
    fs::read_to_string(ctx.state_path("current-fixups"))
        .map(|text| text.starts_with("squash") || text.contains("\nsquash"))
        .unwrap_or(false)
}

/// `is_fixup_flag`: a `fixup -C` / `fixup -c` (replaces the prior message).
fn is_fixup_flag(command: TodoCommand, flags: u8) -> bool {
    command == TodoCommand::Fixup
        && (flags & seq::FLAG_REPLACE_FIXUP_MSG != 0 || flags & seq::FLAG_EDIT_FIXUP_MSG != 0)
}

/// `update_squash_message_for_fixup`: when a `fixup -C/-c` follows earlier
/// messages, re-comment any still-uncommented prior commit message so only the
/// replacing message survives. Mirrors sequencer.c by rewriting the
/// "This is the …th commit message:" headers to their "will be skipped" form
/// and commenting the bodies they introduce.
fn update_squash_message_for_fixup(msg: &[u8], comment: u8) -> Vec<u8> {
    let comment_str = (comment as char).to_string();
    // The header markers (kept) and their skipped variants, both already
    // carrying the comment prefix.
    let kept_first = format!("{comment_str} This is the 1st commit message:");
    let skip_first = format!("{comment_str} The 1st commit message will be skipped:");
    let max_message = msg.iter().filter(|&&b| b == b'\n').count() + 2;
    let nth_markers = (2..=max_message)
        .map(|n| {
            (
                format!("{comment_str} This is the commit message #{n}:"),
                format!("{comment_str} The commit message #{n} will be skipped:"),
            )
        })
        .collect::<Vec<_>>();
    let mut out = Vec::with_capacity(msg.len());
    let mut commenting = false;
    let mut idx = 1usize;
    for line in msg.split_inclusive(|&b| b == b'\n') {
        let body = line.strip_suffix(b"\n").unwrap_or(line);
        let (kept_nth, skip_nth) = &nth_markers[idx - 1];
        if body == kept_first.as_bytes() {
            out.extend_from_slice(skip_first.as_bytes());
            out.push(b'\n');
            commenting = true;
            continue;
        }
        if body == kept_nth.as_bytes() {
            out.extend_from_slice(skip_nth.as_bytes());
            out.push(b'\n');
            idx += 1;
            commenting = true;
            continue;
        }
        if body == skip_first.as_bytes() || body == skip_nth.as_bytes() {
            if body == skip_nth.as_bytes() {
                idx += 1;
            }
            out.extend_from_slice(line);
            commenting = false;
            continue;
        }
        if commenting {
            // Comment out the message body, but leave blank lines untouched.
            if body.is_empty() {
                out.push(b'\n');
            } else if body.first() == Some(&comment) {
                out.extend_from_slice(line);
            } else {
                out.push(comment);
                out.push(b' ');
                out.extend_from_slice(line);
            }
        } else {
            out.extend_from_slice(line);
        }
    }
    out
}

fn remove_last_squash_message_section(
    msg: &[u8],
    remaining_messages: usize,
    comment: u8,
) -> Vec<u8> {
    let comment_str = (comment as char).to_string();
    let mut text = String::from_utf8_lossy(msg).into_owned();

    if let Some(first_newline) = text.find('\n') {
        let header = format!("{comment_str} This is a combination of ");
        if text.starts_with(&header) {
            let replacement =
                format!("{comment_str} This is a combination of {remaining_messages} commits.");
            text.replace_range(..first_newline, &replacement);
        }
    }

    let markers = [
        format!("\n{comment_str} This is the commit message #"),
        format!("\n{comment_str} The commit message #"),
    ];
    if let Some(index) = markers.iter().filter_map(|marker| text.rfind(marker)).max() {
        text.truncate(index + 1);
    }
    text.into_bytes()
}

/// Append the replacing/squashed message body (`append_squash_message`):
/// the "commit message #N" header plus the fixup commit's body. For
/// `fixup -C/-c` the body replaces the message, so it is added verbatim
/// (uncommented) and may be persisted to `message-fixup`.
fn append_squash_message(
    ctx: &Ctx,
    buf: &mut Vec<u8>,
    body: &[u8],
    command: TodoCommand,
    flags: u8,
    comment: u8,
    count: usize,
) -> Result<()> {
    let comment_str = (comment as char).to_string();
    // `amend!` subjects (and fixup!/squash! when squashing) get their subject
    // commented out.
    let commented_len = if body.starts_with(b"amend!")
        || ((command == TodoCommand::Squash || seen_squash(ctx))
            && (body.starts_with(b"squash!") || body.starts_with(b"fixup!")))
    {
        commit_subject_length(body)
    } else {
        0
    };
    buf.push(b'\n');
    buf.extend_from_slice(
        format!(
            "{comment_str} This is the commit message #{}:\n\n",
            count + 2
        )
        .as_bytes(),
    );
    buf.extend_from_slice(&commented_lines(&body[..commented_len], comment));
    let fixup_off = buf.len();
    buf.extend_from_slice(&body[commented_len..]);

    if is_fixup_flag(command, flags) && !seen_squash(ctx) {
        if (flags & seq::FLAG_REPLACE_FIXUP_MSG != 0)
            && (ctx.state_path("message-fixup").exists()
                || !ctx.state_path("message-squash").exists())
        {
            let fixup_msg = &buf[fixup_off + skip_blank_lines(&buf[fixup_off..])..];
            fs::write(ctx.state_path("message-fixup"), fixup_msg)?;
        } else {
            let _ = fs::remove_file(ctx.state_path("message-fixup"));
        }
    } else {
        let _ = fs::remove_file(ctx.state_path("message-fixup"));
    }
    Ok(())
}

/// `update_squash_messages`: build the combined `message-squash` (and, for plain
/// fixup chains, `message-fixup`). Handles `squash`, plain `fixup`, and
/// `fixup -C`/`-c` (`FLAG_REPLACE_FIXUP_MSG` / `FLAG_EDIT_FIXUP_MSG`).
fn update_squash_messages(
    ctx: &Ctx,
    db: &FileObjectDatabase,
    item: &RebaseTodoItem,
    record: &sley_rev::CommitRecord,
) -> Result<()> {
    let comment = comment_char(&ctx.git_dir);
    let comment_str = (comment as char).to_string();
    let target_encoding = commit_encoding_config(&ctx.git_dir);
    let count = current_fixup_count(ctx);
    let flagged = is_fixup_flag(item.command, item.flags);
    let mut buf: Vec<u8>;
    if count > 0 {
        let existing = fs::read(ctx.state_path("message-squash"))?;
        // Replace the first line (the combination header).
        let eol = if existing.first() == Some(&comment) {
            existing
                .iter()
                .position(|&b| b == b'\n')
                .unwrap_or(existing.len())
        } else {
            0
        };
        buf = format!(
            "{comment_str} This is a combination of {} commits.",
            count + 2
        )
        .into_bytes();
        buf.extend_from_slice(&existing[eol..]);
        if flagged && !seen_squash(ctx) {
            buf = update_squash_message_for_fixup(&buf, comment);
        }
    } else {
        let refs = ctx.refs();
        let head = head_commit_oid(&refs)?
            .ok_or_else(|| GitError::Command("need a HEAD to fixup".into()))?;
        let head_record = read_rev_list_commit_record(db, ctx.format, head)?;
        let head_body =
            commit_message_for_commit_encoding(&head_record.commit, &target_encoding).into_owned();
        // Plain fixup (no flag) seeds message-fixup with HEAD's body.
        if item.command == TodoCommand::Fixup && item.flags == 0 {
            fs::write(ctx.state_path("message-fixup"), &head_body)?;
        }
        buf = format!("{comment_str} This is a combination of 2 commits.\n").into_bytes();
        if flagged {
            buf.extend_from_slice(
                format!("{comment_str} The 1st commit message will be skipped:\n\n").as_bytes(),
            );
            buf.extend_from_slice(&commented_lines(&head_body, comment));
        } else {
            buf.extend_from_slice(
                format!("{comment_str} This is the 1st commit message:\n\n").as_bytes(),
            );
            buf.extend_from_slice(&head_body);
        }
    }

    let body = commit_message_for_commit_encoding(&record.commit, &target_encoding);
    if item.command == TodoCommand::Squash || flagged {
        append_squash_message(
            ctx,
            &mut buf,
            &body,
            item.command,
            item.flags,
            comment,
            count,
        )?;
    } else {
        // Plain fixup: the message is skipped.
        buf.push(b'\n');
        buf.extend_from_slice(
            format!(
                "{comment_str} The commit message #{} will be skipped:\n\n",
                count + 2
            )
            .as_bytes(),
        );
        buf.extend_from_slice(&commented_lines(&body, comment));
    }
    fs::write(ctx.state_path("message-squash"), &buf)?;

    // Append to current-fixups.
    let mut fixups = fs::read_to_string(ctx.state_path("current-fixups")).unwrap_or_default();
    if !fixups.is_empty() && !fixups.ends_with('\n') {
        fixups.push('\n');
    }
    fixups.push_str(item.command.as_str());
    fixups.push(' ');
    fixups.push_str(&record.oid.to_hex());
    fixups.push('\n');
    fs::write(
        ctx.state_path("current-fixups"),
        fixups.trim_end_matches('\n'),
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Native `git commit` for the machine
// ---------------------------------------------------------------------------

struct MachineCommit<'a> {
    amend: bool,
    edit: bool,
    cleanup_message: bool,
    allow_empty: bool,
    create_root: bool,
    /// Seed message file (MERGE_MSG / message-squash / message-fixup); `None`
    /// amends with HEAD's message.
    message_file: Option<PathBuf>,
    reflog_sub: &'a str,
    /// The original commit being replayed (authorship source).
    original: Option<&'a sley_rev::CommitRecord>,
}

enum CommitOutcome {
    Committed,
    Failed(i32),
}

fn machine_commit(
    ctx: &Ctx,
    db: &FileObjectDatabase,
    opts: &MachineOpts,
    commit: MachineCommit<'_>,
) -> Result<CommitOutcome> {
    let refs = ctx.refs();
    let head =
        head_commit_oid(&refs)?.ok_or_else(|| GitError::Command("cannot read HEAD".into()))?;
    let head_record = read_rev_list_commit_record(db, ctx.format, head)?;

    let mut message = match &commit.message_file {
        Some(path) => fs::read(path).unwrap_or_default(),
        None => head_record.commit.message.clone(),
    };

    let editmsg = ctx.git_dir.join("COMMIT_EDITMSG");
    if commit.edit {
        let comment_string = commands::status::commit_comment_string(&ctx.git_dir);
        let mut template = message.clone();
        if !template.ends_with(b"\n") {
            template.push(b'\n');
        }
        // git seeds COMMIT_EDITMSG with the message followed by a blank line and
        // the standard help comment block; the trailing blank line is what lets
        // an appended line (e.g. `fixup -c`'s amended subject) land in its own
        // paragraph after stripspace.
        template.push(b'\n');
        template.extend_from_slice(
            format!(
                "{comment_string} Please enter the commit message for your changes. Lines starting\n{comment_string} with '{comment_string}' will be ignored, and an empty message aborts the commit.\n"
            )
            .as_bytes(),
        );
        template.extend_from_slice(&commands::commit::render_commit_editor_status_for_rebase(
            &ctx.git_dir,
            &ctx.worktree_root,
            ctx.format,
            &comment_string,
            commit.amend,
        )?);
        fs::write(&editmsg, template)?;
    } else {
        fs::write(&editmsg, &message)?;
    }
    let prepare_source = if commit.amend && commit.message_file.is_none() {
        commands::commit::PrepareCommitMsgSource::Commit("HEAD")
    } else if ctx.git_dir.join("CHERRY_PICK_HEAD").is_file() {
        commands::commit::PrepareCommitMsgSource::Merge
    } else {
        commands::commit::PrepareCommitMsgSource::Message
    };
    commands::commit::run_prepare_commit_msg_hook(
        &ctx.git_dir,
        &editmsg,
        prepare_source,
        Vec::new(),
        !commit.edit,
    )?;
    if commit.edit {
        launch_editor(&ctx.git_dir, &editmsg)?;
        let path_arg = editmsg.to_string_lossy().into_owned();
        commands::hooks::run_hook_l_at(&ctx.git_dir, "commit-msg", &[path_arg.as_str()])?;
        message = fs::read(&editmsg)?;
    } else {
        message = fs::read(&editmsg)?;
    }
    if commit.edit {
        message = strip_comment_lines(&message, comment_char(&ctx.git_dir));
        if message.iter().all(|b| b.is_ascii_whitespace()) {
            eprintln!("Aborting commit due to empty commit message.");
            return Ok(CommitOutcome::Failed(1));
        }
    } else if commit.cleanup_message {
        // verbatim, but the seed files for non-edit commits never carry
        // comments except the conflicts block which only exists when editing.
        message = strip_comment_lines(&message, comment_char(&ctx.git_dir));
    }

    let tree = sley_worktree::write_tree_from_index(&ctx.git_dir, ctx.format)?;
    let old_tree_for_summary = if commit.reflog_sub == "continue" && !opts.quiet {
        Some(if commit.amend {
            head_record.commit.tree
        } else {
            commit_tree_oid(db, ctx.format, &head)?
        })
    } else {
        None
    };
    let target_encoding = commit_encoding_config(&ctx.git_dir);
    let (parents, author) = if commit.create_root {
        let author = match read_author_script_identity(ctx)? {
            Some(identity) => identity,
            None => match commit.original {
                Some(record) => {
                    commit_author_for_commit_encoding(&record.commit, &target_encoding).into_owned()
                }
                None => commit_identity_from_env("AUTHOR", &ctx.config)?,
            },
        };
        (Vec::new(), author)
    } else if commit.amend {
        let author = head_record.commit.author.clone();
        (head_record.commit.parents.clone(), author)
    } else {
        let author = match read_author_script_identity(ctx)? {
            Some(identity) => identity,
            None => match commit.original {
                Some(record) => {
                    commit_author_for_commit_encoding(&record.commit, &target_encoding).into_owned()
                }
                None => commit_identity_from_env("AUTHOR", &ctx.config)?,
            },
        };
        (vec![head], author)
    };

    if !commit.amend && !commit.create_root && !commit.allow_empty {
        let parent_tree = commit_tree_oid(db, ctx.format, &head)?;
        if tree == parent_tree {
            return Ok(CommitOutcome::Failed(1));
        }
    }

    // Apply `--reset-author-date`/`--ignore-date` and
    // `--committer-date-is-author-date`, mirroring sequencer.c's
    // try_to_commit. `ignore_date` rewrites the author date to "now"; the
    // committer date is then either "now" (`ignore_date`), the author's date
    // (`committer_date_is_author_date` without `ignore_date`), or the
    // environment's committer date.
    let (author, committer) = rebase_commit_identities(opts, author, &ctx.config)?;
    let encoding = commit_encoding_header_from_config(&ctx.git_dir);
    let signature = rebase_commit_signature(
        ctx,
        opts,
        tree,
        &parents,
        &author,
        &committer,
        &message,
        encoding.clone(),
    )?;
    let mut writer = ctx.db();
    let new_oid = sley_sequencer::create_commit(
        &mut writer,
        sley_sequencer::CommitCreate {
            tree,
            parents,
            author,
            committer: committer.clone(),
            message: message.clone(),
            encoding,
            signature,
        },
    )?;

    let subject = commit_subject(&message);
    let reflog_message = if commit.reflog_sub.is_empty() {
        format!("{}: {subject}", ctx.reflog_action).into_bytes()
    } else {
        ctx.reflog(commit.reflog_sub, Some(&subject))
    };
    detach_head_with_reflog(ctx, head, new_oid, reflog_message, committer)?;

    // Record any rerere resolution for the just-committed conflict (matches
    // git invoking rerere() on commit), so a later identical conflict replays.
    let _ = commands::rerere::record_resolved_after_commit(
        &ctx.git_dir,
        &ctx.worktree_root,
        ctx.format,
    );

    // Post-commit cleanup.
    let _ = fs::remove_file(ctx.git_dir.join("CHERRY_PICK_HEAD"));
    let _ = fs::remove_file(ctx.git_dir.join("MERGE_MSG"));
    let _ = fs::remove_file(ctx.git_dir.join("AUTO_MERGE"));
    commands::hooks::run_hook_at(
        &ctx.git_dir,
        "post-commit",
        commands::hooks::HookRun::default(),
    )?;

    if let Some(old_tree) = old_tree_for_summary {
        print_branch_commit_summary(db, &ctx.git_dir, ctx.format, &new_oid, &message)?;
        print_commit_shortstat_between_trees(db, ctx.format, &old_tree, &tree, ctx.lazy_fetch)?;
    }

    let _ = opts;
    Ok(CommitOutcome::Committed)
}

fn rebase_commit_identities(
    opts: &MachineOpts,
    author: Vec<u8>,
    config: &GitConfig,
) -> Result<(Vec<u8>, Vec<u8>)> {
    let now = opts.ignore_date.then(rebase_now_date);
    let author = match &now {
        Some(now) => reset_identity_date(&author, now),
        None => author,
    };
    let committer = match (opts.committer_date_is_author_date, now.as_deref()) {
        (true, Some(now)) | (false, Some(now)) => {
            reset_identity_date(&commit_identity_from_env("COMMITTER", config)?, now)
        }
        (true, None) => {
            let author_date = identity_date(&author).unwrap_or_else(rebase_now_date);
            commit_identity_from_env_with_date("COMMITTER", &author_date, config)?
        }
        (false, None) => commit_identity_from_env("COMMITTER", config)?,
    };
    Ok((author, committer))
}

fn rebase_ws_ignore_from_strategy_opts(opts: &[String]) -> sley_diff_merge::WsIgnore {
    let mut ws_ignore = sley_diff_merge::WsIgnore::EMPTY;
    for opt in opts {
        match opt.as_str() {
            "ignore-space-change" => ws_ignore.space_change = true,
            "ignore-all-space" => ws_ignore.all_space = true,
            "ignore-space-at-eol" => ws_ignore.space_at_eol = true,
            "ignore-cr-at-eol" => ws_ignore.cr_at_eol = true,
            _ => {}
        }
    }
    ws_ignore
}

/// The current wall-clock time as git's raw `@<seconds> +0000`. Mirrors git's
/// `reset_ident_date()` + `datestamp()` path used by `--ignore-date` /
/// `--committer-date-is-author-date`: the upstream tests run under `TZ=UTC`, so
/// the synthesized "now" carries a `+0000` offset.
fn rebase_now_date() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("@{secs} +0000")
}

/// Extract the `@<seconds> <tz>` date portion from a raw identity line
/// (`Name <email> <seconds> <tz>`), suitable for `commit_identity_from_env_with_date`.
fn identity_date(identity: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(identity).ok()?;
    let close = text.rfind('>')?;
    let rest = text[close + 1..].trim();
    let mut parts = rest.split_whitespace();
    let seconds = parts.next()?;
    let tz = parts.next()?;
    Some(format!("@{seconds} {tz}"))
}

/// Replace the date portion of a raw identity line (`Name <email> <seconds> <tz>`)
/// with `new_date` (any form `canonicalize_commit_date` accepts), keeping the
/// name+email. Used to reset author/committer dates to "now".
fn reset_identity_date(identity: &[u8], new_date: &str) -> Vec<u8> {
    let text = String::from_utf8_lossy(identity);
    let Some(close) = text.rfind('>') else {
        return identity.to_vec();
    };
    let canonical = canonicalize_commit_date(new_date);
    let mut out = text[..=close].as_bytes().to_vec();
    out.push(b' ');
    out.extend_from_slice(canonical.as_bytes());
    out
}

fn read_author_script_identity(ctx: &Ctx) -> Result<Option<Vec<u8>>> {
    let Ok(text) = fs::read(ctx.state_path("author-script")) else {
        return Ok(None);
    };
    let Some((name, email, date)) = seq::parse_author_script_bytes(&text) else {
        return Ok(None);
    };
    let identity = sley_sequencer::format_commit_identity_bytes(&name, &email, &date)?;
    Ok(Some(identity))
}

// ---------------------------------------------------------------------------
// Finishing
// ---------------------------------------------------------------------------

fn finish_rebase(ctx: &Ctx, opts: &MachineOpts) -> Result<()> {
    let refs = ctx.refs();
    let head =
        head_commit_oid(&refs)?.ok_or_else(|| GitError::Command("cannot read HEAD".into()))?;
    let head_name_display;
    if let Some(head_name) = &opts.head_name {
        let committer = committer_identity_for_reflog(&ctx.config)?;
        let mut tx = refs.transaction();
        let branch_already_at_head = matches!(refs.read_ref(head_name)?, Some(RefTarget::Direct(current)) if current == head);
        if !branch_already_at_head {
            tx.update(RefUpdate {
                name: head_name.clone(),
                expected: None,
                new: RefTarget::Direct(head),
                reflog: Some(ReflogEntry {
                    old_oid: opts.orig_head,
                    new_oid: head,
                    committer: committer.clone(),
                    message: ctx.reflog("finish", Some(&format!("{head_name} onto {}", opts.onto))),
                }),
            });
        }
        tx.update(RefUpdate {
            name: "HEAD".into(),
            expected: None,
            new: RefTarget::Symbolic(head_name.clone()),
            reflog: Some(ReflogEntry {
                old_oid: head,
                new_oid: head,
                committer: committer.clone(),
                message: ctx.reflog("finish", Some(&format!("returning to {head_name}"))),
            }),
        });
        tx.commit()?;
        head_name_display = head_name.clone();
    } else {
        head_name_display = "detached HEAD".to_string();
    }

    if opts.verbose {
        let db = ctx.db();
        let old_tree = commit_tree_oid(&db, ctx.format, &opts.orig_head)?;
        let new_tree = commit_tree_oid(&db, ctx.format, &head)?;
        // Finish (orig-head..HEAD) diffstat: DIFFSTAT only, no summary lines.
        print_rebase_diffstat(
            &db,
            ctx.format,
            &old_tree,
            &new_tree,
            &ctx.config,
            ctx.lazy_fetch,
            false,
        )?;
    }

    run_post_rewrite_hook(ctx)?;

    apply_autostash(ctx);
    cleanup_rewritten_refs(ctx);

    if !opts.quiet {
        eprintln!("Successfully rebased and updated {head_name_display}.");
    }

    let update_refs_result = do_update_refs(ctx, opts.quiet);

    seq::remove_merge_state(&ctx.git_dir);
    update_refs_result
}

fn rewritten_list_path(ctx: &Ctx) -> PathBuf {
    ctx.state_path("rewritten-list")
}

fn rewritten_pending_path(ctx: &Ctx) -> PathBuf {
    ctx.state_path("rewritten-pending")
}

fn flush_rewritten_pending(ctx: &Ctx) -> Result<()> {
    let pending_path = rewritten_pending_path(ctx);
    let pending = fs::read_to_string(&pending_path).unwrap_or_default();
    if pending.is_empty() {
        return Ok(());
    }
    let refs = ctx.refs();
    let head =
        head_commit_oid(&refs)?.ok_or_else(|| GitError::Command("cannot read HEAD".into()))?;
    let mut list = fs::read_to_string(rewritten_list_path(ctx)).unwrap_or_default();
    for line in pending.lines().filter(|line| !line.trim().is_empty()) {
        list.push_str(line.trim());
        list.push(' ');
        list.push_str(&head.to_hex());
        list.push('\n');
    }
    fs::write(rewritten_list_path(ctx), list)?;
    let _ = fs::remove_file(pending_path);
    Ok(())
}

fn record_rewritten(
    ctx: &Ctx,
    old_oid: &ObjectId,
    next_command: Option<TodoCommand>,
) -> Result<()> {
    let pending_path = rewritten_pending_path(ctx);
    let mut pending = fs::read_to_string(&pending_path).unwrap_or_default();
    pending.push_str(&old_oid.to_hex());
    pending.push('\n');
    fs::write(&pending_path, pending)?;
    if !next_command.is_some_and(TodoCommand::is_fixup) {
        flush_rewritten_pending(ctx)?;
    }
    Ok(())
}

fn run_post_rewrite_hook(ctx: &Ctx) -> Result<()> {
    flush_rewritten_pending(ctx)?;
    let path = rewritten_list_path(ctx);
    let input = fs::read(&path).unwrap_or_default();
    if input.is_empty() {
        return Ok(());
    }
    // Copy notes from each rewritten commit to its replacement, per
    // `notes.rewrite.rebase` / `notes.rewriteRef` (git does this internally,
    // independent of the post-rewrite hook). Best-effort: a failure here must
    // not fail the (already-finished) rebase.
    let pairs = parse_rewritten_list(ctx, &input);
    if let Err(err) = copy_notes_for_rewrite(ctx, &pairs) {
        eprintln!("warning: failed to copy notes: {err}");
    }
    let _ = commands::hooks::run_hook_at(
        &ctx.git_dir,
        "post-rewrite",
        commands::hooks::HookRun {
            args: vec!["rebase".to_string()],
            stdin: Some(input),
            ..commands::hooks::HookRun::default()
        },
    );
    Ok(())
}

/// Parse the `rewritten-list` (one `<old-sha> <new-sha>` pair per line) into
/// resolved object id pairs, skipping any malformed line.
fn parse_rewritten_list(ctx: &Ctx, input: &[u8]) -> Vec<(ObjectId, ObjectId)> {
    let text = String::from_utf8_lossy(input);
    let mut pairs = Vec::new();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let (Some(old), Some(new)) = (parts.next(), parts.next()) else {
            continue;
        };
        if let (Ok(old), Ok(new)) = (
            ObjectId::from_hex(ctx.format, old),
            ObjectId::from_hex(ctx.format, new),
        ) {
            pairs.push((old, new));
        }
    }
    pairs
}

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
                sley_notes::read_note_for(&ctx.git_dir, ctx.format, &store, &notes_ref, old)?
            else {
                continue;
            };
            let dest_blob =
                sley_notes::read_note_for(&ctx.git_dir, ctx.format, &store, &notes_ref, new)?;
            // git's note_tree_insert skips when source and destination notes are
            // the same blob (avoids doubling when a commit is re-rebased to the
            // same id that already carries the copied note).
            if dest_blob == Some(source_blob) {
                continue;
            }
            let source =
                sley_notes::read_note_bytes(&ctx.git_dir, ctx.format, &store, &notes_ref, old)?
                    .unwrap_or_default();
            // `overwrite` replaces; concatenate/cat_sort_uniq append to any note
            // already on the replacement commit, separated by a blank line
            // (combine_notes_concatenate).
            let combined = if mode == "overwrite" || dest_blob.is_none() {
                source
            } else {
                let mut cur =
                    sley_notes::read_note_bytes(&ctx.git_dir, ctx.format, &store, &notes_ref, new)?
                        .unwrap_or_default();
                if cur.last() == Some(&b'\n') {
                    cur.pop();
                }
                cur.extend_from_slice(b"\n\n");
                cur.extend_from_slice(&source);
                cur
            };
            let expected = sley_notes::notes_ref_expected(&store, &notes_ref)?;
            sley_notes::upsert_note_bytes_for(
                &ctx.git_dir,
                ctx.format,
                &store,
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

fn finish_rebase_cleanup(ctx: &Ctx) {
    let _ = fs::remove_file(ctx.git_dir.join("REBASE_HEAD"));
    let _ = fs::remove_file(ctx.git_dir.join("AUTO_MERGE"));
    apply_autostash(ctx);
    cleanup_rewritten_refs(ctx);
    seq::remove_merge_state(&ctx.git_dir);
}

fn cleanup_rewritten_refs(ctx: &Ctx) {
    let refs = ctx.refs();
    let Ok(all_refs) = refs.list_refs() else {
        return;
    };
    for reference in all_refs {
        if reference.name.starts_with("refs/rewritten/") {
            let _ = refs.delete_ref(&reference.name);
        }
    }
}

// ---------------------------------------------------------------------------
// --continue / --skip / --abort / --quit / --edit-todo
// ---------------------------------------------------------------------------

fn rebase_continue(ctx: &Ctx) -> Result<()> {
    let db = ctx.db();
    let opts = seq::read_rebase_state(&ctx.git_dir, ctx.format)?;

    // Unstaged changes gate.
    let status = crate::collect_short_status(&ctx.worktree_root, &ctx.git_dir, ctx.format)?;
    let unmerged = status.iter().any(|entry| {
        matches!(entry.index, b'U' | b'A' | b'D') && matches!(entry.worktree, b'U' | b'A' | b'D')
    });
    let has_unstaged = status.iter().any(|entry| {
        entry.worktree != b' ' && entry.worktree != b'?' && !is_submodule_only_status(entry)
    });
    if unmerged || has_unstaged {
        println!("You must edit all merge conflicts and then");
        println!("mark them as resolved using git add");
        return Err(GitError::Exit(1));
    }

    let mut todo = read_populate_todo(ctx, &db)?;
    filter_update_refs(ctx, &todo.items)?;
    if ctx.state_path("dropped").exists() {
        if check_todo_dropped_commits_against_backup(ctx, &db, &todo.items)? {
            return Err(GitError::Exit(1));
        }
        let _ = fs::remove_file(ctx.state_path("dropped"));
    }

    if commit_staged_changes(ctx, &db, &opts, &todo)? {
        return Err(GitError::Exit(1));
    }

    record_stopped_sha_rewritten(ctx, &todo)?;
    let _ = fs::remove_file(ctx.state_path("stopped-sha"));

    pick_commits(ctx, &db, &opts, &mut todo)
}

fn is_submodule_only_status(entry: &sley_worktree::ShortStatusEntry) -> bool {
    entry.submodule.is_some()
        && entry.index == b' '
        && entry.worktree == b'M'
        && entry.index_mode.is_some_and(sley_index::is_gitlink)
}

fn record_stopped_sha_rewritten(ctx: &Ctx, todo: &TodoList) -> Result<()> {
    if let Ok(raw) = fs::read_to_string(ctx.state_path("stopped-sha"))
        && let Ok(stopped) = ObjectId::from_hex(ctx.format, raw.trim())
    {
        record_rewritten(ctx, &stopped, first_todo_command(todo))?;
    }
    Ok(())
}

/// Returns `true` when the continue must abort (error already printed).
fn commit_staged_changes(
    ctx: &Ctx,
    db: &FileObjectDatabase,
    opts: &MachineOpts,
    todo: &TodoList,
) -> Result<bool> {
    let refs = ctx.refs();
    let head =
        head_commit_oid(&refs)?.ok_or_else(|| GitError::Command("cannot read HEAD".into()))?;
    let head_tree = commit_tree_oid(db, ctx.format, &head)?;
    let index_tree = sley_worktree::write_tree_from_index(&ctx.git_dir, ctx.format)?;
    let is_clean = index_tree == head_tree;

    let message_path = ctx.state_path("message");
    if !is_clean && !message_path.exists() {
        eprintln!(
            "error: you have staged changes in your working tree\nIf these changes are meant to be squashed into the previous commit, run:\n\n  git commit --amend \n\nIf they are meant to go into a new commit, run:\n\n  git commit \n\nIn both cases, once you're done, continue with:\n\n  git rebase --continue\n"
        );
        return Ok(true);
    }

    let amend_path = ctx.state_path("amend");
    let mut amend = false;
    let mut final_fixup = false;
    let mut edit = true;
    let mut cleanup_only = false;
    if amend_path.exists() {
        let raw = fs::read_to_string(&amend_path)?;
        let to_amend = ObjectId::from_hex(ctx.format, raw.trim())
            .map_err(|_| GitError::InvalidObject("invalid contents: amend".into()))?;
        if !is_clean && head != to_amend {
            eprintln!(
                "error: \nYou have uncommitted changes in your working tree. Please, commit them\nfirst and then run 'git rebase --continue' again."
            );
            return Ok(true);
        }
        let fixup_count = current_fixup_count(ctx);
        if is_clean && fixup_count > 0 {
            if head != to_amend || !ctx.state_path("stopped-sha").exists() {
                // A final fixup/squash was completed manually.
                if !next_is_fixup_first(todo) {
                    let _ = fs::remove_file(ctx.state_path("message-fixup"));
                    let _ = fs::remove_file(ctx.state_path("message-squash"));
                    let _ = fs::remove_file(ctx.state_path("current-fixups"));
                }
            } else {
                // Skipping a failed fixup/squash in a chain.
                let fixups = fs::read_to_string(ctx.state_path("current-fixups"))?;
                let mut lines: Vec<&str> = fixups.lines().collect();
                lines.pop();
                let had_squash = lines.iter().any(|line| line.starts_with("squash "));
                fs::write(ctx.state_path("current-fixups"), lines.join("\n"))?;
                if !lines.is_empty() && !next_is_fixup_first(todo) {
                    final_fixup = true;
                    if !had_squash {
                        edit = false;
                        cleanup_only = true;
                        let head_record = read_rev_list_commit_record(db, ctx.format, head)?;
                        fs::write(
                            ctx.state_path("message-squash"),
                            &head_record.commit.message,
                        )?;
                    } else if let Ok(message_squash) = fs::read(ctx.state_path("message-squash")) {
                        let remaining_messages = lines.len() + 1;
                        let pruned = remove_last_squash_message_section(
                            &message_squash,
                            remaining_messages,
                            comment_char(&ctx.git_dir),
                        );
                        fs::write(ctx.state_path("message-squash"), pruned)?;
                    }
                } else if next_is_fixup_first(todo) {
                    // Update the squash message to skip the latest commit
                    // message.
                    let head_record = read_rev_list_commit_record(db, ctx.format, head)?;
                    fs::write(
                        ctx.state_path("message-squash"),
                        &head_record.commit.message,
                    )?;
                }
            }
        }
        amend = true;
    }

    if is_clean {
        let _ = fs::remove_file(ctx.git_dir.join("CHERRY_PICK_HEAD"));
        let _ = fs::remove_file(ctx.git_dir.join("MERGE_MSG"));
        if !final_fixup {
            let _ = fs::remove_file(amend_path);
            return Ok(false);
        }
    }

    let message_file = if final_fixup {
        Some(ctx.state_path("message-squash"))
    } else if amend && is_clean {
        None
    } else {
        Some(message_path)
    };
    let result = machine_commit(
        ctx,
        db,
        opts,
        MachineCommit {
            amend,
            edit: edit && !cleanup_only,
            cleanup_message: true,
            allow_empty: true,
            create_root: false,
            message_file,
            reflog_sub: "continue",
            original: None,
        },
    )?;
    if matches!(result, CommitOutcome::Failed(_)) {
        eprintln!("error: could not commit staged changes.");
        return Ok(true);
    }

    let _ = fs::remove_file(ctx.state_path("amend"));
    let _ = fs::remove_file(ctx.git_dir.join("MERGE_HEAD"));
    let _ = fs::remove_file(ctx.git_dir.join("AUTO_MERGE"));
    if final_fixup {
        let _ = fs::remove_file(ctx.state_path("message-fixup"));
        let _ = fs::remove_file(ctx.state_path("message-squash"));
    }
    if current_fixup_count(ctx) > 0 {
        let _ = fs::remove_file(ctx.state_path("current-fixups"));
    }
    Ok(false)
}

fn next_is_fixup_first(todo: &TodoList) -> bool {
    todo.items
        .iter()
        .find(|item| item.command != TodoCommand::Comment)
        .is_some_and(|item| item.command.is_fixup())
}

fn rebase_skip(ctx: &Ctx) -> Result<()> {
    let db = ctx.db();
    let opts = seq::read_rebase_state(&ctx.git_dir, ctx.format)?;
    let refs = ctx.refs();
    let head =
        head_commit_oid(&refs)?.ok_or_else(|| GitError::Command("cannot read HEAD".into()))?;
    reset_index_and_worktree_to_commit_for_rebase(ctx, &head)?;
    let _ = fs::remove_file(ctx.git_dir.join("CHERRY_PICK_HEAD"));
    let _ = fs::remove_file(ctx.git_dir.join("MERGE_MSG"));
    let _ = fs::remove_file(ctx.git_dir.join("AUTO_MERGE"));

    let mut todo = read_populate_todo(ctx, &db)?;
    if commit_staged_changes(ctx, &db, &opts, &todo)? {
        return Err(GitError::Exit(1));
    }
    record_stopped_sha_rewritten(ctx, &todo)?;
    let _ = fs::remove_file(ctx.state_path("stopped-sha"));
    pick_commits(ctx, &db, &opts, &mut todo)
}

fn rebase_abort(ctx: &Ctx) -> Result<()> {
    let opts = seq::read_rebase_state(&ctx.git_dir, ctx.format)?;
    let db = ctx.db();
    let target = sley_rev::peel_to_commit(&db, ctx.format, &opts.orig_head)?;
    reset_index_and_worktree_to_commit_for_rebase(ctx, &target)?;
    let refs = ctx.refs();
    let committer = committer_identity_for_reflog(&ctx.config)?;
    let old_head = head_commit_oid(&refs)?.unwrap_or(ObjectId::null(ctx.format));
    let returning_to = match &opts.head_name {
        Some(head_name) => head_name.clone(),
        None => opts.orig_head.to_hex(),
    };
    let reflog_message = ctx.reflog("abort", Some(&format!("returning to {returning_to}")));
    let mut tx = refs.transaction();
    match &opts.head_name {
        Some(head_name) => {
            // The merge backend ran on a DETACHED HEAD (the pick loop detaches
            // onto `onto`), so the branch ref never moved off orig_head — abort
            // only re-attaches HEAD to it. Updating the branch ref here would
            // add a spurious branch-reflog entry; git leaves it untouched
            // (t3406 #15).
            if refs.read_ref(head_name)?.is_none() {
                tx.update(RefUpdate {
                    name: head_name.clone(),
                    expected: None,
                    new: RefTarget::Direct(target),
                    reflog: None,
                });
            }
            tx.update(RefUpdate {
                name: "HEAD".into(),
                expected: None,
                new: RefTarget::Symbolic(head_name.clone()),
                reflog: Some(ReflogEntry {
                    old_oid: old_head,
                    new_oid: target,
                    committer,
                    message: reflog_message,
                }),
            });
        }
        None => {
            tx.update(RefUpdate {
                name: "HEAD".into(),
                expected: None,
                new: RefTarget::Direct(target),
                reflog: Some(ReflogEntry {
                    old_oid: old_head,
                    new_oid: target,
                    committer,
                    message: reflog_message,
                }),
            });
        }
    }
    tx.commit()?;
    let _ = fs::remove_file(ctx.git_dir.join("CHERRY_PICK_HEAD"));
    let _ = fs::remove_file(ctx.git_dir.join("MERGE_MSG"));
    let _ = fs::remove_file(ctx.git_dir.join("AUTO_MERGE"));
    finish_rebase_cleanup(ctx);
    Ok(())
}

fn rebase_quit(ctx: &Ctx) -> Result<()> {
    save_autostash(ctx);
    cleanup_rewritten_refs(ctx);
    seq::remove_merge_state(&ctx.git_dir);
    let _ = fs::remove_file(ctx.git_dir.join("REBASE_HEAD"));
    Ok(())
}

fn rebase_edit_todo(ctx: &Ctx) -> Result<()> {
    let db = ctx.db();
    let todo_path = ctx.state_path("git-rebase-todo");
    let text = fs::read_to_string(&todo_path)?;
    let stripped = stripspace_drop_comments(&text, comment_char(&ctx.git_dir));
    let mut resolver = make_resolver(ctx, &db);
    let (items, old_messages) = seq::parse_todo_buffer(
        &stripped,
        ctx.state_path("done").exists(),
        comment_char(&ctx.git_dir) as char,
        &mut resolver,
    );
    drop(resolver);
    let incorrect = !old_messages.is_empty() || ctx.state_path("dropped").exists();
    write_todo_file(ctx, &todo_path, &items, true, true, None, None, &db)?;
    if !incorrect {
        write_todo_file(
            ctx,
            &ctx.state_path("git-rebase-todo.backup"),
            &items,
            false,
            true,
            None,
            None,
            &db,
        )?;
    }
    launch_sequence_editor(ctx, &todo_path)?;
    let edited = fs::read_to_string(&todo_path)?;
    let stripped = stripspace_drop_comments(&edited, comment_char(&ctx.git_dir));
    let mut resolver = make_resolver(ctx, &db);
    let (new_items, messages) = seq::parse_todo_buffer(
        &stripped,
        ctx.state_path("done").exists(),
        comment_char(&ctx.git_dir) as char,
        &mut resolver,
    );
    drop(resolver);
    if !messages.is_empty() {
        for message in messages {
            eprintln!("{message}");
        }
        print_edit_todo_recovery_advice();
        return Err(GitError::Exit(1));
    }
    if incorrect {
        for message in old_messages {
            eprintln!("{message}");
        }
        if check_todo_dropped_commits_against_backup(ctx, &db, &new_items)? {
            return Err(GitError::Exit(1));
        }
        let _ = fs::remove_file(ctx.state_path("dropped"));
    } else if check_todo_dropped_commits(ctx, &db, &items, &new_items)? {
        return Err(GitError::Exit(1));
    }
    // Reconcile the update-refs state with the edited todo (drop removed
    // update-ref lines, add new ones).
    filter_update_refs(ctx, &new_items)?;
    write_todo_file(ctx, &todo_path, &new_items, false, false, None, None, &db)?;
    let done_nr = fs::read_to_string(ctx.state_path("done"))
        .map(|text| {
            let mut resolver = make_resolver(ctx, &db);
            let (done_items, _) = seq::parse_todo_buffer(
                &text,
                true,
                comment_char(&ctx.git_dir) as char,
                &mut resolver,
            );
            seq::count_commands(&done_items)
        })
        .unwrap_or(0);
    let total_nr = done_nr + seq::count_commands(&new_items);
    fs::write(ctx.state_path("end"), format!("{total_nr}\n"))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Autostash
// ---------------------------------------------------------------------------

fn create_autostash(ctx: &Ctx, use_apply_backend: bool) -> Result<()> {
    let status = crate::collect_short_status(&ctx.worktree_root, &ctx.git_dir, ctx.format)?;
    let dirty = status.iter().any(|entry| {
        !rebase_status_is_submodule(entry)
            && entry.index != b'?'
            && (entry.index != b' ' || entry.worktree != b' ')
    });
    if !dirty {
        return Ok(());
    }
    let created = commands::stash::create_stash_for_autostash_at(&ctx.git_dir, &ctx.worktree_root)?;
    let Some(oid) = created else {
        eprintln!("fatal: Cannot autostash");
        return Err(GitError::Exit(128));
    };
    // git records the autostash inside the active backend's state dir
    // (`rebase-apply/` for the apply backend, `rebase-merge/` for the merge
    // sequencer). The t-suite asserts on `$dotest/autostash` per backend, and
    // routing it to the wrong dir both fails those asserts and leaves the other
    // dir behind for the next rebase to trip over.
    let dir = if use_apply_backend {
        ctx.git_dir.join("rebase-apply")
    } else {
        seq::merge_dir(&ctx.git_dir)
    };
    fs::create_dir_all(&dir)?;
    fs::write(dir.join("autostash"), oid.to_hex())?;
    let db = ctx.db();
    println!(
        "Created autostash: {}",
        find_unique_abbrev_hex(ctx, &db, &oid)
    );
    let refs = ctx.refs();
    let head =
        head_commit_oid(&refs)?.ok_or_else(|| GitError::Command("cannot read HEAD".into()))?;
    reset_index_and_worktree_to_commit_for_rebase(ctx, &head)?;
    Ok(())
}

fn apply_autostash(ctx: &Ctx) {
    apply_save_autostash(ctx, true);
}

fn save_autostash(ctx: &Ctx) {
    apply_save_autostash(ctx, false);
}

fn read_apply_autostash(ctx: &Ctx) -> Option<String> {
    let path = ctx.git_dir.join("rebase-apply").join("autostash");
    fs::read_to_string(path).ok()
}

fn apply_save_autostash(ctx: &Ctx, attempt_apply: bool) {
    // The autostash lives in whichever backend's state dir created it:
    // `rebase-merge/autostash` (merge sequencer) or `rebase-apply/autostash`
    // (apply backend). Check both so the restore path is backend-agnostic.
    let merge_path = ctx.state_path("autostash");
    let apply_path = ctx.git_dir.join("rebase-apply").join("autostash");
    let path = if merge_path.exists() {
        merge_path
    } else {
        apply_path
    };
    let Ok(text) = fs::read_to_string(&path) else {
        return;
    };
    let _ = fs::remove_file(&path);
    apply_save_autostash_text(ctx, &text, attempt_apply);
}

fn apply_save_autostash_text(ctx: &Ctx, text: &str, attempt_apply: bool) {
    let oid_text = text.trim().to_string();
    if oid_text.is_empty() {
        return;
    }
    let Ok(oid) = ObjectId::from_hex(ctx.format, &oid_text) else {
        return;
    };
    let applied = attempt_apply
        && commands::stash::apply_stash_commit_quietly_at(
            &ctx.git_dir,
            &ctx.worktree_root,
            &oid,
            ctx.lazy_fetch,
        )
        .unwrap_or(false);
    if applied {
        eprintln!("Applied autostash.");
        return;
    }
    // Store the stash for later.
    let stored = commands::stash::store_stash_commit_at(&ctx.git_dir, &oid, "autostash").is_ok();
    if !stored {
        eprintln!("error: cannot store {oid_text}");
    } else if attempt_apply {
        print_autostash_conflict_advice();
    } else {
        eprintln!("Autostash exists; creating a new stash entry.");
        eprintln!("Your changes are safe in the stash.");
        eprintln!("You can run \"git stash pop\" or \"git stash drop\" at any time.");
    }
}

fn print_autostash_conflict_advice() {
    eprintln!("Your local changes are stashed, however applying them");
    eprintln!("resulted in conflicts.  You can either resolve the conflicts");
    eprintln!("and then discard the stash with \"git stash drop\", or, if you");
    eprintln!("do not want to resolve them now, run \"git reset --hard\" and");
    eprintln!("apply the local changes later by running \"git stash pop\".");
}

fn cleanup_autostash_and_state(ctx: &Ctx) {
    apply_autostash(ctx);
    seq::remove_merge_state(&ctx.git_dir);
}

#[cfg(test)]
mod native_strategy_tests {
    use super::{
        custom_rebase_strategy_needs_external_driver, is_unimplemented_git_core_merge_strategy,
    };

    #[test]
    fn git_core_merge_helpers_never_fall_through_to_path() {
        for strategy in ["octopus", "one-file", "ours", "subtree"] {
            assert!(custom_rebase_strategy_needs_external_driver(strategy));
            assert!(is_unimplemented_git_core_merge_strategy(strategy));
        }
        for strategy in ["ort", "recursive", "resolve"] {
            assert!(!custom_rebase_strategy_needs_external_driver(strategy));
            assert!(!is_unimplemented_git_core_merge_strategy(strategy));
        }
        assert!(custom_rebase_strategy_needs_external_driver(
            "custom-driver"
        ));
        assert!(!is_unimplemented_git_core_merge_strategy("custom-driver"));
    }
}
