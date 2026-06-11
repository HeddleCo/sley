//! `git rebase` — the merge backend driven by the sequencer todo machine.
//!
//! The on-disk contract (`.git/rebase-merge/`) and the todo instruction sheet
//! live in `sley_sequencer::rebase`; this module is the porcelain: option
//! parsing, todo generation (`sequencer_make_script`), the
//! `complete_action`/`pick_commits` drive loop, `--continue` / `--abort` /
//! `--skip` / `--quit` / `--edit-todo`, and autostash handling.

use crate::commands::merge_rebase::{
    MergePathResult, commit_tree_oid, head_commit_oid, merge_bases, merge_index_entry,
    merge_read_blob, merge_remove_worktree_file, merge_write_worktree_file,
    print_branch_commit_summary, print_commit_shortstat_between_trees, three_way_merge_trees,
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
    keep_empty: bool,
    empty: EmptyMode,
    force: bool,
    exec: Vec<String>,
    signoff: bool,
    reschedule_failed_exec: Option<bool>,
    root: bool,
    fork_point: Option<bool>,
    reapply_cherry_picks: Option<bool>,
    update_refs: Option<bool>,
    strategy: Option<String>,
    strategy_opts: Vec<String>,
    positional: Vec<String>,
    total_args: usize,
}

fn rebase_usage_error() -> GitError {
    print_rebase_usage();
    GitError::Exit(129)
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
        keep_empty: false,
        empty: EmptyMode::Unspecified,
        force: false,
        exec: Vec::new(),
        signoff: false,
        reschedule_failed_exec: None,
        root: false,
        fork_point: None,
        reapply_cherry_picks: None,
        update_refs: None,
        strategy: None,
        strategy_opts: Vec::new(),
        positional: Vec::new(),
        total_args: args.len(),
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
            "--onto" | _ if arg == "--onto" || arg.starts_with("--onto=") => {
                out.onto_name = Some(take_value(&mut index)?);
            }
            "--keep-base" => out.keep_base = true,
            "-i" | "--interactive" => out.interactive = true,
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
            "--autosquash" => out.autosquash = Some(true),
            "--no-autosquash" => out.autosquash = Some(false),
            "-k" | "--keep-empty" => out.keep_empty = true,
            "--no-keep-empty" => out.keep_empty = false,
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
                let value = args.get(index).cloned().ok_or_else(rebase_usage_error)?;
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
            "--no-verify" | "--verify" => {}
            "--rerere-autoupdate" | "--no-rerere-autoupdate" => {}
            "--allow-empty-message" => {}
            "--committer-date-is-author-date" | "--reset-author-date" | "--ignore-date" => {
                out.force = true;
            }
            "--ignore-whitespace" => {
                out.strategy_opts.push("ignore-space-change".to_string());
            }
            _ if arg.starts_with("-C") => {
                let value = &arg[2..];
                if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                    eprintln!("fatal: switch `C' expects a numerical value");
                    return Err(GitError::Exit(128));
                }
            }
            _ if arg.starts_with("--whitespace=") => {
                let value = &arg["--whitespace=".len()..];
                if !matches!(value, "warn" | "nowarn" | "error" | "error-all" | "fix" | "strip") {
                    eprintln!("fatal: Invalid whitespace option: '{value}'");
                    return Err(GitError::Exit(128));
                }
            }
            "--rebase-merges" | "-r" => {}
            _ if arg.starts_with("--rebase-merges=") => {}
            "--" => {
                out.positional
                    .extend(args[index + 1..].iter().cloned());
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
        if command.is_empty() {
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
    git_dir: PathBuf,
    common_git_dir: PathBuf,
    worktree_root: PathBuf,
    format: ObjectFormat,
    /// `GIT_REFLOG_ACTION` or `"rebase"`.
    reflog_action: String,
}

impl Ctx {
    fn discover() -> Result<Ctx> {
        let cwd = env::current_dir()?;
        let git_dir = discover_git_dir(&cwd)?;
        let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
        let worktree_root = worktree_root_for_git_dir(&git_dir)?;
        let format = repository_object_format(&common_git_dir)?;
        let reflog_action = env::var("GIT_REFLOG_ACTION").unwrap_or_else(|_| "rebase".to_string());
        Ok(Ctx {
            git_dir,
            common_git_dir,
            worktree_root,
            format,
            reflog_action,
        })
    }

    fn db(&self) -> FileObjectDatabase {
        FileObjectDatabase::from_git_dir(&self.common_git_dir, self.format)
    }

    fn refs(&self) -> FileRefStore {
        FileRefStore::new(&self.git_dir, self.format)
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

/// Machine flags persisted in the state dir (`write_basic_state` +
/// `read_populate_opts`).
struct MachineOpts {
    quiet: bool,
    verbose: bool,
    signoff: bool,
    allow_ff: bool,
    drop_redundant_commits: bool,
    keep_redundant_commits: bool,
    reschedule_failed_exec: bool,
    head_name: Option<String>,
    onto: ObjectId,
    orig_head: ObjectId,
}

fn write_basic_state(ctx: &Ctx, opts: &MachineOpts) -> Result<()> {
    let dir = seq::merge_dir(&ctx.git_dir);
    fs::create_dir_all(&dir)?;
    fs::write(dir.join("interactive"), b"")?;
    let head_name = opts
        .head_name
        .clone()
        .unwrap_or_else(|| "detached HEAD".to_string());
    fs::write(dir.join("head-name"), format!("{head_name}\n"))?;
    fs::write(dir.join("onto"), format!("{}\n", opts.onto))?;
    fs::write(dir.join("orig-head"), format!("{}\n", opts.orig_head))?;
    if opts.quiet {
        fs::write(dir.join("quiet"), b"")?;
    }
    if opts.verbose {
        fs::write(dir.join("verbose"), b"")?;
    }
    if opts.signoff {
        fs::write(dir.join("signoff"), b"--signoff\n")?;
    }
    if opts.drop_redundant_commits {
        fs::write(dir.join("drop_redundant_commits"), b"")?;
    }
    if opts.keep_redundant_commits {
        fs::write(dir.join("keep_redundant_commits"), b"")?;
    }
    if opts.reschedule_failed_exec {
        fs::write(dir.join("reschedule-failed-exec"), b"")?;
    } else {
        fs::write(dir.join("no-reschedule-failed-exec"), b"")?;
    }
    Ok(())
}

fn read_basic_state(ctx: &Ctx) -> Result<MachineOpts> {
    let head_name = seq::read_state_line(&ctx.git_dir, "head-name")
        .ok_or_else(|| GitError::not_found("rebase-merge/head-name"))?;
    let onto_raw = seq::read_state_line(&ctx.git_dir, "onto")
        .ok_or_else(|| GitError::not_found("rebase-merge/onto"))?;
    let onto = ObjectId::from_hex(ctx.format, onto_raw.trim())
        .map_err(|_| GitError::InvalidObject("invalid onto value during rebase".into()))?;
    let orig_raw = seq::read_state_line(&ctx.git_dir, "orig-head")
        .ok_or_else(|| GitError::not_found("rebase-merge/orig-head"))?;
    let orig_head = ObjectId::from_hex(ctx.format, orig_raw.trim())
        .map_err(|_| GitError::InvalidObject("invalid orig-head value during rebase".into()))?;
    let exists = |name: &str| ctx.state_path(name).exists();
    let signoff = exists("signoff");
    Ok(MachineOpts {
        quiet: exists("quiet"),
        verbose: exists("verbose"),
        signoff,
        allow_ff: !signoff,
        drop_redundant_commits: exists("drop_redundant_commits"),
        keep_redundant_commits: exists("keep_redundant_commits"),
        reschedule_failed_exec: exists("reschedule-failed-exec"),
        head_name: if head_name.starts_with("refs/") {
            Some(head_name)
        } else {
            None
        },
        onto,
        orig_head,
    })
}

// ---------------------------------------------------------------------------
// Todo list plumbing
// ---------------------------------------------------------------------------

struct TodoList {
    items: Vec<RebaseTodoItem>,
    /// Index of the item currently being executed.
    current: usize,
    /// Count of executed commands (the `msgnum` counter).
    done_nr: usize,
    /// `done_nr` + remaining real commands (the `end` counter).
    total_nr: usize,
}

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

fn count_commands(items: &[RebaseTodoItem]) -> usize {
    items
        .iter()
        .filter(|item| item.command != TodoCommand::Comment)
        .count()
}

fn serialize_item(item: &RebaseTodoItem, short: bool, db: &FileObjectDatabase) -> String {
    let oid_text = item.oid.as_ref().map(|oid| {
        if short {
            find_unique_abbrev_hex(db, oid)
        } else {
            oid.to_hex()
        }
    });
    seq::todo_item_to_string(item, oid_text.as_deref())
}

fn find_unique_abbrev_hex(db: &FileObjectDatabase, oid: &ObjectId) -> String {
    let hex = oid.to_hex();
    let mut width = 7usize.min(hex.len());
    while width < hex.len() {
        match db.resolve_prefix(&hex[..width]) {
            Ok(sley_odb::ObjectPrefixResolution::Ambiguous(_)) => width += 1,
            _ => break,
        }
    }
    hex[..width].to_string()
}

fn todo_to_text(items: &[RebaseTodoItem], short: bool, db: &FileObjectDatabase) -> String {
    let mut out = String::new();
    for item in items {
        out.push_str(&serialize_item(item, short, db));
        out.push('\n');
    }
    out
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
    let mut buf = todo_to_text(items, short, db);
    if help {
        let comment = comment_char(&ctx.git_dir) as char;
        let check_error = missing_commit_check_level(ctx) == MissingCommitCheck::Error;
        seq::append_todo_help(
            &mut buf,
            count_commands(items),
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
    let next = if reschedule {
        todo.current
    } else {
        todo.current + 1
    };
    let tail = if next <= todo.items.len() {
        &todo.items[next..]
    } else {
        &[]
    };
    fs::write(
        ctx.state_path("git-rebase-todo"),
        todo_to_text(tail, false, db),
    )?;
    if !reschedule && next > 0 {
        let line = serialize_item(&todo.items[next - 1], false, db);
        let done_path = ctx.state_path("done");
        let mut existing = fs::read(&done_path).unwrap_or_default();
        existing.extend_from_slice(line.as_bytes());
        existing.push(b'\n');
        fs::write(done_path, existing)?;
    }
    Ok(())
}

fn read_populate_todo(ctx: &Ctx, db: &FileObjectDatabase) -> Result<TodoList> {
    let text = fs::read_to_string(ctx.state_path("git-rebase-todo"))?;
    let done_exists = ctx.state_path("done").exists();
    let mut resolver = make_resolver(ctx, db);
    let (items, messages) =
        seq::parse_todo_buffer(&text, done_exists, comment_char(&ctx.git_dir) as char, &mut resolver);
    if !messages.is_empty() {
        for message in &messages {
            eprintln!("{message}");
        }
        eprintln!("error: please fix this using 'git rebase --edit-todo'.");
        return Err(GitError::Exit(1));
    }
    let done_nr = fs::read_to_string(ctx.state_path("done"))
        .map(|text| {
            let mut resolver = make_resolver(ctx, db);
            let (done_items, _) = seq::parse_todo_buffer(
                &text,
                true,
                comment_char(&ctx.git_dir) as char,
                &mut resolver,
            );
            count_commands(&done_items)
        })
        .unwrap_or(0);
    let total_nr = done_nr + count_commands(&items);
    Ok(TodoList {
        items,
        current: 0,
        done_nr,
        total_nr,
    })
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
    let config = read_repo_config(&ctx.git_dir).ok()?;
    config.get(section, None, key).map(str::to_string)
}

fn rebase_config_bool(ctx: &Ctx, section: &str, key: &str) -> Option<bool> {
    let value = rebase_config_value(ctx, section, key)?;
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" | "" => Some(false),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub(crate) fn cmd_rebase(args: &[String]) -> Result<()> {
    let parsed = parse_rebase_args(args)?;
    let ctx = Ctx::discover()?;

    if parsed.action != RebaseAction::None && parsed.total_args != 1 {
        return Err(rebase_usage_error());
    }
    if parsed.positional.len() > 2 {
        return Err(rebase_usage_error());
    }

    let in_progress = seq::in_progress(&ctx.git_dir);
    if parsed.action != RebaseAction::None && !in_progress {
        eprintln!("fatal: no rebase in progress");
        return Err(GitError::Exit(128));
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
                io::stdout().write_all(&patch)?;
                return Ok(());
            }
            eprintln!("fatal: there is no current patch");
            return Err(GitError::Exit(128));
        }
        RebaseAction::None => {}
    }

    if in_progress {
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
    let implied_merge = interactive_explicit
        || args.merge_backend
        || !args.exec.is_empty()
        || args.autosquash == Some(true)
        || args.empty != EmptyMode::Unspecified
        || args.keep_empty
        || args.strategy.is_some()
        || !args.strategy_opts.is_empty();
    let _ = implied_merge;

    // Resolve upstream.
    let upstream_name = match args.positional.first() {
        Some(name) => name.clone(),
        None => {
            if args.root {
                String::new()
            } else {
                match default_upstream_name(ctx, &refs) {
                    Some(name) => name,
                    None => {
                        print_missing_upstream_advice(ctx, &refs);
                        return Err(GitError::Exit(1));
                    }
                }
            }
        }
    };
    let upstream = if args.root {
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

    // Branch / orig_head / head_name.
    let (branch_name, head_name, orig_head, switch_to) = match args.positional.get(1) {
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

    // Onto.
    let onto_name = match &args.onto_name {
        Some(name) => name.clone(),
        None if args.keep_base => format!("{upstream_name}...{branch_name}"),
        None => upstream_name.clone(),
    };
    let onto = if onto_name.contains("...") {
        let (left, right) = onto_name.split_once("...").expect("contains ...");
        let left_oid = resolve_revision(&ctx.git_dir, ctx.format, if left.is_empty() { "HEAD" } else { left })
            .and_then(|oid| sley_rev::peel_to_commit(&db, ctx.format, &oid));
        let right_oid = resolve_revision(&ctx.git_dir, ctx.format, if right.is_empty() { "HEAD" } else { right })
            .and_then(|oid| sley_rev::peel_to_commit(&db, ctx.format, &oid));
        match (left_oid, right_oid) {
            (Ok(left), Ok(right)) => {
                let bases = merge_bases(&ctx.common_git_dir, &db, ctx.format, &left, &right)?;
                match bases.first() {
                    Some(base) if bases.len() == 1 => *base,
                    _ => {
                        eprintln!("fatal: '{onto_name}': need exactly one merge base");
                        return Err(GitError::Exit(128));
                    }
                }
            }
            _ => {
                eprintln!("fatal: '{onto_name}': need exactly one merge base");
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

    let autostash = args
        .autostash
        .unwrap_or_else(|| rebase_config_bool(ctx, "rebase", "autostash").unwrap_or(false));
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
    let reschedule_failed_exec = args.reschedule_failed_exec.unwrap_or_else(|| {
        rebase_config_bool(ctx, "rebase", "rescheduleFailedExec").unwrap_or(false)
    });
    let force = args.force || args.signoff;

    // Autostash before the clean-tree check.
    if autostash {
        create_autostash(ctx)?;
    }

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
            if let Some(switch_to) = &switch_to
                && head_name.is_some()
            {
                checkout_up_to_date(ctx, &db, switch_to, &orig_head)?;
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
        print_rebase_diffstat(&db, ctx.format, &old_tree, &new_tree)?;
    }

    // The apply backend's explicit fast-forward case.
    if allow_preemptive_ff
        && !force
        && branch_base.as_ref() == Some(&orig_head)
    {
        // onto is a descendant of orig_head: fast-forward.
        sley_worktree::reset_index_and_worktree_to_commit(
            &ctx.worktree_root,
            &ctx.git_dir,
            ctx.format,
            &onto,
        )?;
        let committer = commit_identity_from_env("COMMITTER")?;
        detach_head_with_reflog(
            ctx,
            orig_head,
            onto,
            ctx.reflog("start", Some(&format!("checkout {onto_name}"))),
            committer.clone(),
        )?;
        if !args.quiet {
            println!("Fast-forwarded {branch_name} to {onto_name}.");
        }
        if let Some(head_name) = &head_name {
            move_to_original_branch(ctx, head_name, orig_head, onto, committer)?;
        }
        finish_rebase_cleanup(ctx);
        return Ok(());
    }

    let opts = MachineOpts {
        quiet: args.quiet,
        verbose: args.verbose,
        signoff: args.signoff,
        allow_ff: !force,
        drop_redundant_commits: empty == EmptyMode::Drop,
        keep_redundant_commits: empty == EmptyMode::Keep,
        reschedule_failed_exec,
        head_name: head_name.clone(),
        onto,
        orig_head,
    };

    // Generate the todo list.
    let records = make_script_commits(ctx, &db, upstream.as_ref(), &orig_head, args.keep_empty)?;
    let mut items: Vec<RebaseTodoItem> = records
        .iter()
        .map(|record| RebaseTodoItem {
            command: TodoCommand::Pick,
            flags: 0,
            oid: Some(record.oid),
            arg: format!("# {}", commit_subject(&record.commit.message)),
            raw: String::new(),
        })
        .collect();

    write_basic_state(ctx, &opts)?;
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

    if autosquash {
        items = rearrange_squash(ctx, &db, items)?;
    }

    if !args.exec.is_empty() {
        items = add_exec_commands(items, &args.exec);
    }

    if count_commands(&items) == 0 {
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

fn default_upstream_name(ctx: &Ctx, refs: &FileRefStore) -> Option<String> {
    let branch = match refs.read_ref("HEAD").ok()?? {
        RefTarget::Symbolic(name) => name.strip_prefix("refs/heads/")?.to_string(),
        RefTarget::Direct(_) => return None,
    };
    let config = read_repo_config(&ctx.git_dir).ok()?;
    let merge = config
        .get("branch", Some(branch.as_str()), "merge")
        .map(str::to_string)?;
    let remote = config
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
        Ok(Some(RefTarget::Symbolic(name))) => name
            .strip_prefix("refs/heads/")
            .map(str::to_string),
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
    let status = sley_worktree::short_status(&ctx.worktree_root, &ctx.git_dir, ctx.format)?;
    let has_unstaged = status
        .iter()
        .any(|entry| entry.worktree != b' ' && entry.worktree != b'?' && entry.index != b'?');
    let has_staged = status
        .iter()
        .any(|entry| entry.index != b' ' && entry.index != b'?');
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
            return Ok(record.parents.is_empty() && false);
        }
        current = record.parents[0];
    }
}

fn checkout_up_to_date(
    ctx: &Ctx,
    _db: &FileObjectDatabase,
    branch: &str,
    oid: &ObjectId,
) -> Result<()> {
    sley_worktree::reset_index_and_worktree_to_commit(
        &ctx.worktree_root,
        &ctx.git_dir,
        ctx.format,
        oid,
    )?;
    let refs = ctx.refs();
    let committer = commit_identity_from_env("COMMITTER")?;
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
    tx.commit()
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
    write_diff_stat(
        &mut stdout,
        &entries,
        db,
        None,
        false,
        DiffStatOptions {
            compact_summary: false,
            stat_count: None,
            color: false,
        },
    )?;
    write_diff_shortstat(&mut stdout, &entries, db, None, true)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// make_script: generate pick lines for upstream..orig_head
// ---------------------------------------------------------------------------

fn make_script_commits(
    ctx: &Ctx,
    db: &FileObjectDatabase,
    upstream: Option<&ObjectId>,
    orig_head: &ObjectId,
    keep_empty: bool,
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
    }
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
    // Topological order, parents first.
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
        out.push(record);
    }
    Ok(out)
}

fn rearrange_squash(
    ctx: &Ctx,
    db: &FileObjectDatabase,
    items: Vec<RebaseTodoItem>,
) -> Result<Vec<RebaseTodoItem>> {
    let _ = (ctx, db);
    // `--autosquash`: move fixup!/squash! commits behind their targets.
    let mut subjects: Vec<(usize, String)> = Vec::new();
    let mut moved: Vec<Option<Vec<usize>>> = vec![None; items.len()];
    let mut used = vec![false; items.len()];
    let mut rewritten: Vec<Option<TodoCommand>> = vec![None; items.len()];
    for (idx, item) in items.iter().enumerate() {
        if item.command != TodoCommand::Pick {
            continue;
        }
        let subject = item.arg.strip_prefix("# ").unwrap_or(&item.arg).to_string();
        let mut rest = subject.as_str();
        let mut command = None;
        loop {
            if let Some(stripped) = rest.strip_prefix("fixup! ") {
                rest = stripped;
                command.get_or_insert(TodoCommand::Fixup);
            } else if let Some(stripped) = rest.strip_prefix("squash! ") {
                rest = stripped;
                command.get_or_insert(TodoCommand::Squash);
            } else if let Some(stripped) = rest.strip_prefix("amend! ") {
                rest = stripped;
                command.get_or_insert(TodoCommand::Fixup);
            } else {
                break;
            }
        }
        if let Some(command) = command {
            // Find the target: latest earlier item whose subject matches
            // `rest` (exact or prefix).
            let target = subjects
                .iter()
                .rev()
                .find(|(_, s)| s == rest || s.starts_with(rest))
                .map(|(i, _)| *i);
            if let Some(target) = target {
                moved[target].get_or_insert_with(Vec::new).push(idx);
                used[idx] = true;
                rewritten[idx] = Some(command);
                continue;
            }
        }
        subjects.push((idx, subject));
    }
    if used.iter().all(|&u| !u) {
        return Ok(items);
    }
    let mut out = Vec::with_capacity(items.len());
    fn push_chain(
        idx: usize,
        items: &[RebaseTodoItem],
        moved: &[Option<Vec<usize>>],
        rewritten: &[Option<TodoCommand>],
        out: &mut Vec<RebaseTodoItem>,
    ) {
        let mut item = items[idx].clone();
        if let Some(command) = rewritten[idx] {
            item.command = command;
        }
        out.push(item);
        if let Some(fixups) = &moved[idx] {
            for &fixup in fixups {
                push_chain(fixup, items, moved, rewritten, out);
            }
        }
    }
    for (idx, item) in items.iter().enumerate() {
        if used[idx] {
            continue;
        }
        let _ = item;
        push_chain(idx, &items, &moved, &rewritten, &mut out);
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

    let shortonto = find_unique_abbrev_hex(db, &opts.onto);
    let shorthead = find_unique_abbrev_hex(db, &opts.orig_head);
    let shortrevisions = match upstream {
        Some(upstream) => {
            let shortrev = find_unique_abbrev_hex(db, upstream);
            format!("{shortrev}..{shorthead}")
        }
        None => shorthead,
    };

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
            eprintln!(
                "You can fix this with 'git rebase --edit-todo' and then run 'git rebase --continue'."
            );
            eprintln!("Or you can abort the rebase with 'git rebase --abort'.");
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
            let done_text = todo_to_text(&todo.items[..skipped], false, db);
            fs::write(ctx.state_path("done"), done_text)?;
            todo.items.drain(..skipped);
            todo.done_nr = skipped;
        }
    }

    write_todo_file(ctx, &todo_path, &todo.items, false, false, None, None, db)?;
    todo.total_nr = todo.done_nr + count_commands(&todo.items);
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
                find_unique_abbrev_hex(db, oid),
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

fn checkout_onto_base(
    ctx: &Ctx,
    opts: &MachineOpts,
    onto_name: &str,
    base: &ObjectId,
) -> Result<()> {
    let refs = ctx.refs();
    let old = head_commit_oid(&refs)?.unwrap_or(ObjectId::null(ctx.format));
    if let Err(err) = sley_worktree::reset_index_and_worktree_to_commit(
        &ctx.worktree_root,
        &ctx.git_dir,
        ctx.format,
        base,
    ) {
        apply_autostash(ctx);
        seq::remove_merge_state(&ctx.git_dir);
        eprintln!("error: could not detach HEAD");
        let _ = err;
        return Err(GitError::Exit(1));
    }
    let committer = commit_identity_from_env("COMMITTER")?;
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
                do_reset(ctx, db, &item.arg)?;
            }
            TodoCommand::Merge => {
                let stop = do_merge(ctx, db, opts, todo, &item)?;
                match stop {
                    PickOutcome::Continue => {}
                    PickOutcome::EditStop => return Ok(()),
                    PickOutcome::Fail(code) => return Err(GitError::Exit(code)),
                }
            }
            TodoCommand::UpdateRef => {}
            TodoCommand::Noop
            | TodoCommand::Drop
            | TodoCommand::Comment
            | TodoCommand::Revert => {}
        }

        todo.current += 1;
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
    eprintln!("hint:     {}", serialize_item(item, false, db));
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
    let expected = todo_to_text(&todo.items[todo.current + 1..], false, db);
    if on_disk != expected {
        let mut reloaded = read_populate_todo(ctx, db)?;
        reloaded.done_nr = todo.done_nr;
        reloaded.total_nr = reloaded.done_nr + count_commands(&reloaded.items);
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
                    find_unique_abbrev_hex(db, &oid),
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
    let dirty = sley_worktree::short_status(&ctx.worktree_root, &ctx.git_dir, ctx.format)
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
    let head = head_commit_oid(&refs)?
        .ok_or_else(|| GitError::Command("could not read HEAD".into()))?;
    let refname = format!("refs/rewritten/{name}");
    let committer = commit_identity_from_env("COMMITTER")?;
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

fn do_reset(ctx: &Ctx, db: &FileObjectDatabase, name: &str) -> Result<()> {
    let name = name.trim();
    let target = {
        let refname = format!("refs/rewritten/{name}");
        let refs = ctx.refs();
        match refs.read_ref(&refname)? {
            Some(RefTarget::Direct(oid)) => oid,
            _ => resolve_revision(&ctx.git_dir, ctx.format, name)
                .and_then(|oid| sley_rev::peel_to_commit(db, ctx.format, &oid))?,
        }
    };
    sley_worktree::reset_index_and_worktree_to_commit(
        &ctx.worktree_root,
        &ctx.git_dir,
        ctx.format,
        &target,
    )?;
    let refs = ctx.refs();
    let old = head_commit_oid(&refs)?.unwrap_or(ObjectId::null(ctx.format));
    let committer = commit_identity_from_env("COMMITTER")?;
    detach_head_with_reflog(
        ctx,
        old,
        target,
        ctx.reflog("reset", Some(name)),
        committer,
    )
}

fn do_merge(
    _ctx: &Ctx,
    _db: &FileObjectDatabase,
    _opts: &MachineOpts,
    _todo: &mut TodoList,
    _item: &RebaseTodoItem,
) -> Result<PickOutcome> {
    eprintln!("error: 'merge' todo commands are not implemented by sley rebase yet");
    Ok(PickOutcome::Fail(1))
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
    let head = head_commit_oid(&refs)?
        .ok_or_else(|| GitError::Command("cannot read HEAD".into()))?;

    let is_fixup = item.command.is_fixup();
    let final_fixup = is_fixup && !next_is_fixup(todo);

    // Write the author script for --continue / commit amending.
    if let Some(script) = seq::format_author_script(&record.commit.author) {
        fs::write(ctx.state_path("author-script"), script)?;
    }

    let parent = record.parents.first().copied();

    // Fast-forward when the pick's parent is exactly HEAD.
    if opts.allow_ff && !is_fixup && parent == Some(head) {
        sley_worktree::reset_index_and_worktree_to_commit(
            &ctx.worktree_root,
            &ctx.git_dir,
            ctx.format,
            &oid,
        )?;
        let committer = commit_identity_from_env("COMMITTER")?;
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
                        allow_empty: true,
                        message_file: None,
                        reflog_sub: "reword",
                        original: Some(&record),
                    },
                )?;
                if let CommitOutcome::Failed(code) = res {
                    return stop_with_patch(ctx, db, opts, &record, item, code, false);
                }
                reread_todo_if_changed(ctx, db, todo)?;
                return Ok(PickOutcome::Continue);
            }
            TodoCommand::Edit => {
                eprintln!(
                    "Stopped at {}...  {}",
                    find_unique_abbrev_hex(db, &oid),
                    item.arg
                );
                return stop_with_patch(ctx, db, opts, &record, item, 0, true);
            }
            _ => return Ok(PickOutcome::Continue),
        }
    }

    // Merge the commit's change onto HEAD.
    let parent_tree = match &parent {
        Some(parent) => commit_tree_oid(db, ctx.format, parent)?,
        None => ObjectId::empty_tree(ctx.format),
    };
    let head_tree = commit_tree_oid(db, ctx.format, &head)?;
    let theirs_tree = record.commit.tree;
    let base_map = stash_tree_entry_map(db, ctx.format, &parent_tree)?;
    let ours_map = stash_tree_entry_map(db, ctx.format, &head_tree)?;
    let theirs_map = stash_tree_entry_map(db, ctx.format, &theirs_tree)?;
    let write_db = ctx.db();
    let (results, conflicts) = three_way_merge_trees(
        &write_db,
        ctx.format,
        &base_map,
        &ours_map,
        &theirs_map,
        "HEAD",
        &commit_subject(&record.commit.message),
    )?;

    // Compose the message (fixup/squash machinery).
    let mut message = record.commit.message.clone();
    if opts.signoff && !is_fixup {
        message = commands::replay::append_signoff_before_comments(
            message,
            &commit_signoff_from_env()?,
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
            println!("Auto-merging {display}");
            println!("CONFLICT (content): Merge conflict in {display}");
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

        eprintln!(
            "error: could not apply {}... {}",
            find_unique_abbrev_hex(db, &oid),
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
    let result = machine_commit(
        ctx,
        db,
        opts,
        MachineCommit {
            amend,
            edit: edit || item.command == TodoCommand::Reword,
            allow_empty,
            message_file: commit_message_file,
            reflog_sub: command_reflog_name(item.command),
            original: Some(&record),
        },
    )?;
    if let CommitOutcome::Failed(code) = result {
        if is_fixup {
            intend_to_amend(ctx)?;
            let squash = fs::read(ctx.state_path("message-squash")).unwrap_or_default();
            fs::write(ctx.state_path("message"), &squash)?;
            fs::write(ctx.git_dir.join("MERGE_MSG"), &squash)?;
        }
        return stop_with_patch(ctx, db, opts, &record, item, code, false);
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
            find_unique_abbrev_hex(db, &oid),
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
    let mut entries = Vec::new();
    for (path, result) in results {
        match result {
            MergePathResult::Resolved(Some((mode, oid))) => {
                entries.push(merge_index_entry(path, *mode, *oid, 0));
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
    for (path, result) in results {
        match result {
            MergePathResult::Resolved(Some((mode, oid))) => {
                if ours_map.get(path) != Some(&(*mode, *oid)) {
                    let content = merge_read_blob(db, oid)?;
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
    let head = head_commit_oid(&refs)?
        .ok_or_else(|| GitError::Command("cannot read HEAD".into()))?;
    fs::write(ctx.state_path("amend"), format!("{head}\n"))?;
    Ok(())
}

/// `error_with_patch` / `make_patch`: record stop state and exit.
fn stop_with_patch(
    ctx: &Ctx,
    db: &FileObjectDatabase,
    opts: &MachineOpts,
    record: &sley_rev::CommitRecord,
    item: &RebaseTodoItem,
    exit_code: i32,
    to_amend: bool,
) -> Result<PickOutcome> {
    let _ = opts;
    fs::write(ctx.state_path("stopped-sha"), format!("{}\n", record.oid))?;
    fs::write(ctx.git_dir.join("REBASE_HEAD"), format!("{}\n", record.oid))?;

    // Write the patch file: diff of the commit against its first parent.
    let parent_tree = match record.parents.first() {
        Some(parent) => commit_tree_oid(db, ctx.format, parent)?,
        None => ObjectId::empty_tree(ctx.format),
    };
    let patch = render_patch_between_trees(db, ctx.format, &parent_tree, &record.commit.tree)
        .unwrap_or_default();
    fs::write(ctx.state_path("patch"), patch)?;

    if !ctx.state_path("message").exists() {
        let mut message = record.commit.message.clone();
        if !message.ends_with(b"\n") {
            message.push(b'\n');
        }
        fs::write(ctx.state_path("message"), message)?;
    }

    if to_amend {
        intend_to_amend(ctx)?;
        eprintln!("You can amend the commit now, with");
        eprintln!();
        eprintln!("  git commit --amend ");
        eprintln!();
        eprintln!("Once you are satisfied with your changes, run");
        eprintln!();
        eprintln!("  git rebase --continue");
        return Ok(PickOutcome::EditStop);
    }
    if exit_code != 0 {
        eprintln!(
            "Could not apply {}... {}",
            find_unique_abbrev_hex(db, &record.oid),
            item.arg
        );
        return Ok(PickOutcome::Fail(exit_code));
    }
    Ok(PickOutcome::EditStop)
}

fn render_patch_between_trees(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    old_tree: &ObjectId,
    new_tree: &ObjectId,
) -> Result<Vec<u8>> {
    let entries = sley_diff_merge::diff_name_status_trees_with_options(
        db,
        format,
        old_tree,
        new_tree,
        sley_diff_merge::DiffNameStatusOptions::default(),
    )?;
    let mut out = Vec::new();
    for entry in &entries {
        let old_path = entry.old_path.as_deref().unwrap_or(&entry.path);
        let path = String::from_utf8_lossy(&entry.path).into_owned();
        let old_display = String::from_utf8_lossy(old_path).into_owned();
        out.extend_from_slice(
            format!("diff --git a/{old_display} b/{path}\n").as_bytes(),
        );
        let old_content = entry
            .old_oid
            .as_ref()
            .map(|oid| merge_read_blob(db, oid))
            .transpose()?;
        let new_content = entry
            .new_oid
            .as_ref()
            .map(|oid| merge_read_blob(db, oid))
            .transpose()?;
        match entry.status {
            sley_diff_merge::NameStatus::Added => {
                if let Some(mode) = entry.new_mode {
                    out.extend_from_slice(format!("new file mode {mode:06o}\n").as_bytes());
                }
            }
            sley_diff_merge::NameStatus::Deleted => {
                if let Some(mode) = entry.old_mode {
                    out.extend_from_slice(format!("deleted file mode {mode:06o}\n").as_bytes());
                }
            }
            _ => {}
        }
        let old_label = if old_content.is_some() {
            format!("a/{old_display}")
        } else {
            "/dev/null".to_string()
        };
        let new_label = if new_content.is_some() {
            format!("b/{path}")
        } else {
            "/dev/null".to_string()
        };
        out.extend_from_slice(format!("--- {old_label}\n+++ {new_label}\n").as_bytes());
        let old_bytes = old_content.unwrap_or_default();
        let new_bytes = new_content.unwrap_or_default();
        let old_lines = sley_diff_merge::split_lines(&old_bytes);
        let new_lines = sley_diff_merge::split_lines(&new_bytes);
        let old_count = old_lines.len();
        let new_count = new_lines.len();
        let ops = sley_diff_merge::myers_diff_lines(&old_lines, &new_lines);
        out.extend_from_slice(
            format!(
                "@@ -{},{old_count} +{},{new_count} @@\n",
                if old_count == 0 { 0 } else { 1 },
                if new_count == 0 { 0 } else { 1 }
            )
            .as_bytes(),
        );
        let mut old_index = 0usize;
        let mut new_index = 0usize;
        for op in ops {
            match op {
                sley_diff_merge::DiffOp::Equal(count) => {
                    for _ in 0..count {
                        out.push(b' ');
                        out.extend_from_slice(old_lines[old_index].content);
                        out.push(b'\n');
                        old_index += 1;
                        new_index += 1;
                    }
                }
                sley_diff_merge::DiffOp::Delete(count) => {
                    for _ in 0..count {
                        out.push(b'-');
                        out.extend_from_slice(old_lines[old_index].content);
                        out.push(b'\n');
                        old_index += 1;
                    }
                }
                sley_diff_merge::DiffOp::Insert(count) => {
                    for _ in 0..count {
                        out.push(b'+');
                        out.extend_from_slice(new_lines[new_index].content);
                        out.push(b'\n');
                        new_index += 1;
                    }
                }
            }
        }
    }
    Ok(out)
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

/// `update_squash_messages` for plain fixup/squash (no `-c`/`-C` flags).
fn update_squash_messages(
    ctx: &Ctx,
    db: &FileObjectDatabase,
    item: &RebaseTodoItem,
    record: &sley_rev::CommitRecord,
) -> Result<()> {
    let comment = comment_char(&ctx.git_dir);
    let comment_str = (comment as char).to_string();
    let count = current_fixup_count(ctx);
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
    } else {
        let refs = ctx.refs();
        let head = head_commit_oid(&refs)?
            .ok_or_else(|| GitError::Command("need a HEAD to fixup".into()))?;
        let head_record = read_rev_list_commit_record(db, ctx.format, head)?;
        let head_body = &head_record.commit.message;
        if item.command == TodoCommand::Fixup && item.flags == 0 {
            fs::write(ctx.state_path("message-fixup"), head_body)?;
        }
        buf = format!("{comment_str} This is a combination of 2 commits.\n").into_bytes();
        buf.extend_from_slice(
            format!("{comment_str} This is the 1st commit message:\n\n").as_bytes(),
        );
        buf.extend_from_slice(head_body);
    }

    let body = &record.commit.message;
    if item.command == TodoCommand::Squash {
        buf.push(b'\n');
        buf.extend_from_slice(
            format!(
                "{comment_str} This is the commit message #{}:\n\n",
                count + 2
            )
            .as_bytes(),
        );
        buf.extend_from_slice(body);
        let _ = fs::remove_file(ctx.state_path("message-fixup"));
    } else {
        buf.push(b'\n');
        buf.extend_from_slice(
            format!(
                "{comment_str} The commit message #{} will be skipped:\n\n",
                count + 2
            )
            .as_bytes(),
        );
        buf.extend_from_slice(&commented_lines(body, comment));
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
    fs::write(ctx.state_path("current-fixups"), fixups.trim_end_matches('\n'))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Native `git commit` for the machine
// ---------------------------------------------------------------------------

struct MachineCommit<'a> {
    amend: bool,
    edit: bool,
    allow_empty: bool,
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
    let head = head_commit_oid(&refs)?
        .ok_or_else(|| GitError::Command("cannot read HEAD".into()))?;
    let head_record = read_rev_list_commit_record(db, ctx.format, head)?;

    let mut message = match &commit.message_file {
        Some(path) => fs::read(path).unwrap_or_default(),
        None => head_record.commit.message.clone(),
    };

    if commit.edit {
        let path = ctx.git_dir.join("COMMIT_EDITMSG");
        fs::write(&path, &message)?;
        launch_editor(&ctx.git_dir, &path)?;
        message = fs::read(&path)?;
    }
    if commit.edit {
        message = strip_comment_lines(&message, comment_char(&ctx.git_dir));
        if message.iter().all(|b| b.is_ascii_whitespace()) {
            eprintln!("Aborting commit due to empty commit message.");
            return Ok(CommitOutcome::Failed(1));
        }
    } else {
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
    let (parents, author) = if commit.amend {
        let author = head_record.commit.author.clone();
        (head_record.commit.parents.clone(), author)
    } else {
        let author = match read_author_script_identity(ctx)? {
            Some(identity) => identity,
            None => match commit.original {
                Some(record) => record.commit.author.clone(),
                None => commit_identity_from_env("AUTHOR")?,
            },
        };
        (vec![head], author)
    };

    if !commit.amend && !commit.allow_empty {
        let parent_tree = commit_tree_oid(db, ctx.format, &head)?;
        if tree == parent_tree {
            return Ok(CommitOutcome::Failed(1));
        }
    }

    let committer = commit_identity_from_env("COMMITTER")?;
    let mut writer = ctx.db();
    let new_oid = sley_sequencer::create_commit(
        &mut writer,
        sley_sequencer::CommitCreate {
            tree,
            parents,
            author,
            committer: committer.clone(),
            message: message.clone(),
            encoding: None,
        },
    )?;

    let subject = commit_subject(&message);
    let reflog_message = if commit.reflog_sub.is_empty() {
        format!("{}: {subject}", ctx.reflog_action).into_bytes()
    } else {
        ctx.reflog(commit.reflog_sub, Some(&subject))
    };
    detach_head_with_reflog(ctx, head, new_oid, reflog_message, committer)?;

    // Post-commit cleanup.
    let _ = fs::remove_file(ctx.git_dir.join("CHERRY_PICK_HEAD"));
    let _ = fs::remove_file(ctx.git_dir.join("MERGE_MSG"));
    let _ = fs::remove_file(ctx.git_dir.join("AUTO_MERGE"));

    if let Some(old_tree) = old_tree_for_summary {
        print_branch_commit_summary(&ctx.git_dir, ctx.format, &new_oid, &message)?;
        print_commit_shortstat_between_trees(db, ctx.format, &old_tree, &tree)?;
    }

    let _ = opts;
    Ok(CommitOutcome::Committed)
}

fn read_author_script_identity(ctx: &Ctx) -> Result<Option<Vec<u8>>> {
    let Ok(text) = fs::read_to_string(ctx.state_path("author-script")) else {
        return Ok(None);
    };
    let Some((name, email, date)) = seq::parse_author_script(&text) else {
        return Ok(None);
    };
    let identity = sley_sequencer::format_commit_identity(&name, &email, &date)?;
    Ok(Some(identity))
}

// ---------------------------------------------------------------------------
// Finishing
// ---------------------------------------------------------------------------

fn finish_rebase(ctx: &Ctx, opts: &MachineOpts) -> Result<()> {
    let refs = ctx.refs();
    let head = head_commit_oid(&refs)?
        .ok_or_else(|| GitError::Command("cannot read HEAD".into()))?;
    let head_name_display;
    if let Some(head_name) = &opts.head_name {
        let committer = commit_identity_from_env("COMMITTER")?;
        let mut tx = refs.transaction();
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
        tx.update(RefUpdate {
            name: "HEAD".into(),
            expected: None,
            new: RefTarget::Symbolic(head_name.clone()),
            reflog: Some(ReflogEntry {
                old_oid: head,
                new_oid: head,
                committer,
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
        print_rebase_diffstat(&db, ctx.format, &old_tree, &new_tree)?;
    }

    apply_autostash(ctx);

    if !opts.quiet {
        eprintln!("Successfully rebased and updated {head_name_display}.");
    }

    seq::remove_merge_state(&ctx.git_dir);
    Ok(())
}

fn finish_rebase_cleanup(ctx: &Ctx) {
    let _ = fs::remove_file(ctx.git_dir.join("REBASE_HEAD"));
    let _ = fs::remove_file(ctx.git_dir.join("AUTO_MERGE"));
    apply_autostash(ctx);
    seq::remove_merge_state(&ctx.git_dir);
}

// ---------------------------------------------------------------------------
// --continue / --skip / --abort / --quit / --edit-todo
// ---------------------------------------------------------------------------

fn rebase_continue(ctx: &Ctx) -> Result<()> {
    let db = ctx.db();
    let opts = read_basic_state(ctx)?;

    // Unstaged changes gate.
    let status = sley_worktree::short_status(&ctx.worktree_root, &ctx.git_dir, ctx.format)?;
    let unmerged = status.iter().any(|entry| {
        matches!(entry.index, b'U' | b'A' | b'D') && matches!(entry.worktree, b'U' | b'A' | b'D')
    });
    let has_unstaged = status
        .iter()
        .any(|entry| entry.worktree != b' ' && entry.worktree != b'?');
    if unmerged || has_unstaged {
        println!("You must edit all merge conflicts and then");
        println!("mark them as resolved using git add");
        return Err(GitError::Exit(1));
    }

    let mut todo = read_populate_todo(ctx, &db)?;

    if commit_staged_changes(ctx, &db, &opts, &todo)? {
        return Err(GitError::Exit(1));
    }

    let _ = fs::remove_file(ctx.state_path("stopped-sha"));

    pick_commits(ctx, &db, &opts, &mut todo)
}

/// Returns `true` when the continue must abort (error already printed).
fn commit_staged_changes(
    ctx: &Ctx,
    db: &FileObjectDatabase,
    opts: &MachineOpts,
    todo: &TodoList,
) -> Result<bool> {
    let refs = ctx.refs();
    let head = head_commit_oid(&refs)?
        .ok_or_else(|| GitError::Command("cannot read HEAD".into()))?;
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
                let had_squash = lines.iter().any(|line| line.starts_with("squash "));
                lines.pop();
                fs::write(ctx.state_path("current-fixups"), lines.join("\n"))?;
                if !lines.is_empty() && !next_is_fixup_first(todo) {
                    final_fixup = true;
                    if !had_squash {
                        edit = false;
                        cleanup_only = true;
                    }
                } else if next_is_fixup_first(todo) {
                    // Update the squash message to skip the latest commit
                    // message.
                    let head_record = read_rev_list_commit_record(db, ctx.format, head)?;
                    fs::write(ctx.state_path("message-squash"), &head_record.commit.message)?;
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
            allow_empty: true,
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
    if current_fixup_count(ctx) > 0 && (final_fixup || is_clean) {
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
    let opts = read_basic_state(ctx)?;
    let refs = ctx.refs();
    let head = head_commit_oid(&refs)?
        .ok_or_else(|| GitError::Command("cannot read HEAD".into()))?;
    sley_worktree::reset_index_and_worktree_to_commit(
        &ctx.worktree_root,
        &ctx.git_dir,
        ctx.format,
        &head,
    )?;
    let _ = fs::remove_file(ctx.git_dir.join("CHERRY_PICK_HEAD"));
    let _ = fs::remove_file(ctx.git_dir.join("MERGE_MSG"));
    let _ = fs::remove_file(ctx.git_dir.join("AUTO_MERGE"));

    let mut todo = read_populate_todo(ctx, &db)?;
    if commit_staged_changes(ctx, &db, &opts, &todo)? {
        return Err(GitError::Exit(1));
    }
    let _ = fs::remove_file(ctx.state_path("stopped-sha"));
    pick_commits(ctx, &db, &opts, &mut todo)
}

fn rebase_abort(ctx: &Ctx) -> Result<()> {
    let opts = read_basic_state(ctx)?;
    let db = ctx.db();
    let target = sley_rev::peel_to_commit(&db, ctx.format, &opts.orig_head)?;
    sley_worktree::reset_index_and_worktree_to_commit(
        &ctx.worktree_root,
        &ctx.git_dir,
        ctx.format,
        &target,
    )?;
    let refs = ctx.refs();
    let committer = commit_identity_from_env("COMMITTER")?;
    let old_head = head_commit_oid(&refs)?.unwrap_or(ObjectId::null(ctx.format));
    let returning_to = match &opts.head_name {
        Some(head_name) => head_name.clone(),
        None => opts.orig_head.to_hex(),
    };
    let reflog_message = ctx.reflog("abort", Some(&format!("returning to {returning_to}")));
    let mut tx = refs.transaction();
    match &opts.head_name {
        Some(head_name) => {
            tx.update(RefUpdate {
                name: head_name.clone(),
                expected: None,
                new: RefTarget::Direct(target),
                reflog: Some(ReflogEntry {
                    old_oid: old_head,
                    new_oid: target,
                    committer: committer.clone(),
                    message: reflog_message.clone(),
                }),
            });
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
    let (items, _messages) = seq::parse_todo_buffer(
        &stripped,
        ctx.state_path("done").exists(),
        comment_char(&ctx.git_dir) as char,
        &mut resolver,
    );
    drop(resolver);
    write_todo_file(ctx, &todo_path, &items, true, true, None, None, &db)?;
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
        return Err(GitError::Exit(1));
    }
    write_todo_file(ctx, &todo_path, &new_items, false, false, None, None, &db)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Autostash
// ---------------------------------------------------------------------------

fn create_autostash(ctx: &Ctx) -> Result<()> {
    let status = sley_worktree::short_status(&ctx.worktree_root, &ctx.git_dir, ctx.format)?;
    let dirty = status
        .iter()
        .any(|entry| entry.index != b'?' && (entry.index != b' ' || entry.worktree != b' '));
    if !dirty {
        return Ok(());
    }
    let created = commands::stash::create_stash_for_autostash()?;
    let Some(oid) = created else {
        eprintln!("fatal: Cannot autostash");
        return Err(GitError::Exit(128));
    };
    let dir = seq::merge_dir(&ctx.git_dir);
    fs::create_dir_all(&dir)?;
    fs::write(dir.join("autostash"), oid.to_hex())?;
    let db = ctx.db();
    println!("Created autostash: {}", find_unique_abbrev_hex(&db, &oid));
    let refs = ctx.refs();
    let head = head_commit_oid(&refs)?
        .ok_or_else(|| GitError::Command("cannot read HEAD".into()))?;
    sley_worktree::reset_index_and_worktree_to_commit(
        &ctx.worktree_root,
        &ctx.git_dir,
        ctx.format,
        &head,
    )?;
    Ok(())
}

fn apply_autostash(ctx: &Ctx) {
    apply_save_autostash(ctx, true);
}

fn save_autostash(ctx: &Ctx) {
    apply_save_autostash(ctx, false);
}

fn apply_save_autostash(ctx: &Ctx, attempt_apply: bool) {
    let path = ctx.state_path("autostash");
    let Ok(text) = fs::read_to_string(&path) else {
        return;
    };
    let oid_text = text.trim().to_string();
    let _ = fs::remove_file(&path);
    if oid_text.is_empty() {
        return;
    }
    let Ok(oid) = ObjectId::from_hex(ctx.format, &oid_text) else {
        return;
    };
    let applied =
        attempt_apply && commands::stash::apply_stash_commit_quietly(&oid).unwrap_or(false);
    if applied {
        eprintln!("Applied autostash.");
        return;
    }
    // Store the stash for later.
    let stored = commands::stash::store_stash_commit(&oid, "autostash").is_ok();
    if !stored {
        eprintln!("error: cannot store {oid_text}");
    } else if attempt_apply {
        eprintln!("Applying autostash resulted in conflicts.");
        eprintln!("Your changes are safe in the stash.");
        eprintln!("You can run \"git stash pop\" or \"git stash drop\" at any time.");
    } else {
        eprintln!("Autostash exists; creating a new stash entry.");
        eprintln!("Your changes are safe in the stash.");
        eprintln!("You can run \"git stash pop\" or \"git stash drop\" at any time.");
    }
}

fn cleanup_autostash_and_state(ctx: &Ctx) {
    apply_autostash(ctx);
    seq::remove_merge_state(&ctx.git_dir);
}
