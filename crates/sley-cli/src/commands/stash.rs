//! `git stash` and its subcommands
//! (push/save/pop/apply/branch/clear/drop/create/store/show/list).

use sley::plumbing::{sley_config, sley_core, sley_diff_merge, sley_rev, sley_worktree};
// Command modules pull their shared plumbing from the crate root. A glob import
// works because a submodule can access its ancestor module's items (including
// private ones), so every helper, type, and re-export visible at the crate root
// is in scope here without re-listing it.
use crate::commands::merge_rebase::three_way_merge_trees_styled;
use crate::*;

#[path = "stash_options.rs"]
mod stash_options;
use sley::plumbing::sley_object::{TreeEntries, tree_entry_cmp};
use stash_options::{setup_stash_apply_options, setup_stash_list_options};
use std::io::Write;
use std::process::{Command, Stdio};
#[derive(Debug)]
pub(super) struct StashListOptions {
    format: StashListFormat,
    max_count: Option<usize>,
    skip_count: usize,
    max_age: Option<i64>,
    min_age: Option<i64>,
    min_parents: Option<usize>,
    max_parents: Option<usize>,
    abbrev_len: Option<usize>,
    date_mode: DateMode,
    date_explicit: bool,
    author_filters: Vec<SimpleLogRegex>,
    committer_filters: Vec<SimpleLogRegex>,
    reflog_filters: Vec<SimpleLogRegex>,
    grep_filters: Vec<SimpleLogRegex>,
    grep_all_match: bool,
    invert_grep: bool,
    regexp_ignore_case: bool,
    note_refs: Vec<String>,
    show_patch: bool,
    combined_patch: bool,
}

#[derive(Debug)]
pub(super) enum StashListFormat {
    Default,
    Oneline,
    Custom {
        compiled: CompiledLogFormat,
        final_newline: bool,
    },
}

pub(crate) fn cmd_stash(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("-h") | Some("--help") => {
            stash_usage_stdout();
            Err(GitError::Exit(129))
        }
        Some(value) if value.starts_with('-') && !stash_can_start_assumed_push(value) => {
            eprintln!("error: unknown option `{}`", value.trim_start_matches('-'));
            stash_usage_stderr();
            Err(GitError::Exit(129))
        }
        Some("apply") => cmd_stash_apply(cli_session, &args[1..]),
        Some("branch") => cmd_stash_branch(cli_session, &args[1..]),
        Some("clear") => cmd_stash_clear(cli_session, &args[1..]),
        Some("create") => cmd_stash_create(cli_session, &args[1..]),
        Some("drop") => cmd_stash_drop(cli_session, &args[1..]),
        Some("export") => cmd_stash_export(cli_session, &args[1..]),
        Some("import") => cmd_stash_import(cli_session, &args[1..]),
        Some("list") => cmd_stash_list(cli_session, &args[1..]),
        Some("pop") => cmd_stash_pop(cli_session, &args[1..]),
        Some("push") => cmd_stash_push(cli_session, &args[1..]),
        Some("save") => cmd_stash_save(cli_session, &args[1..]),
        Some("show") => cmd_stash_show(cli_session, &args[1..]),
        Some("store") => cmd_stash_store(cli_session, &args[1..]),
        // No subcommand: assume `git stash push` (git's `push_stash_unassumed`
        // fallback). In this "assumed" mode a bare positional token that isn't a
        // pathspec after `--` is rejected, so `git stash -q drop` errors instead
        // of silently stashing a pathspec named `drop`.
        Some(_) => {
            stash_reject_assumed_push_token(args)?;
            cmd_stash_push(cli_session, args)
        }
        None => cmd_stash_push(cli_session, &[]),
    }
}

fn stash_can_start_assumed_push(value: &str) -> bool {
    matches!(
        value,
        "--" | "-q"
            | "--quiet"
            | "--no-quiet"
            | "-u"
            | "--include-untracked"
            | "--no-include-untracked"
            | "-a"
            | "--all"
            | "--no-all"
            | "-k"
            | "--keep-index"
            | "--no-keep-index"
            | "-S"
            | "--staged"
            | "--no-staged"
            | "-p"
            | "--patch"
            | "--no-patch"
            | "--auto-advance"
            | "--no-auto-advance"
            | "-U"
            | "--unified"
            | "--inter-hunk-context"
            | "-m"
            | "--message"
            | "--no-message"
            | "--pathspec-from-file"
            | "--no-pathspec-from-file"
            | "--pathspec-file-nul"
            | "--no-pathspec-file-nul"
    ) || value.starts_with("-q")
        || value.starts_with("-p")
        || value.starts_with("-U")
        || value.starts_with("-m")
        || value.starts_with("--patch=")
        || value.starts_with("--no-patch=")
        || value.starts_with("--auto-advance=")
        || value.starts_with("--no-auto-advance=")
        || value.starts_with("--unified=")
        || value.starts_with("--inter-hunk-context=")
        || value.starts_with("--message=")
        || value.starts_with("--no-message=")
        || value.starts_with("--pathspec-from-file=")
}

fn stash_usage_stdout() {
    println!("usage: git stash list [<log-options>]");
    println!(
        "   or: git stash show [-u | --include-untracked | --only-untracked] [<diff-options>] [<stash>]"
    );
    println!("   or: git stash drop [-q | --quiet] [<stash>]");
    println!("   or: git stash pop [--index] [-q | --quiet] [<stash>]");
    println!(
        "   or: git stash apply [--index] [-q | --quiet] [--label-ours=<label>] [--label-theirs=<label>] [--label-base=<label>] [<stash>]"
    );
    println!("   or: git stash branch <branchname> [<stash>]");
    println!(
        "   or: git stash [push] [-p | --patch] [-S | --staged] [-k | --[no-]keep-index] [-q | --quiet]"
    );
    println!(
        "   or: git stash save [-p | --patch] [-S | --staged] [-k | --[no-]keep-index] [-q | --quiet]"
    );
    println!("   or: git stash clear");
    println!("   or: git stash create [<message>]");
    println!("   or: git stash store [(-m | --message) <message>] [-q | --quiet] <commit>");
    println!("   or: git stash export (--print | --to-ref <ref>) [<stash>...]");
    println!("   or: git stash import <commit>");
}

fn stash_usage_stderr() {
    eprintln!("usage: git stash list [<log-options>]");
    eprintln!(
        "   or: git stash show [-u | --include-untracked | --only-untracked] [<diff-options>] [<stash>]"
    );
    eprintln!("   or: git stash drop [-q | --quiet] [<stash>]");
    eprintln!("   or: git stash pop [--index] [-q | --quiet] [<stash>]");
    eprintln!(
        "   or: git stash apply [--index] [-q | --quiet] [--label-ours=<label>] [--label-theirs=<label>] [--label-base=<label>] [<stash>]"
    );
    eprintln!("   or: git stash branch <branchname> [<stash>]");
    eprintln!(
        "   or: git stash [push] [-p | --patch] [-S | --staged] [-k | --[no-]keep-index] [-q | --quiet]"
    );
    eprintln!(
        "   or: git stash save [-p | --patch] [-S | --staged] [-k | --[no-]keep-index] [-q | --quiet]"
    );
    eprintln!("   or: git stash clear");
    eprintln!("   or: git stash create [<message>]");
    eprintln!("   or: git stash store [(-m | --message) <message>] [-q | --quiet] <commit>");
    eprintln!("   or: git stash export (--print | --to-ref <ref>) [<stash>...]");
    eprintln!("   or: git stash import <commit>");
}

fn stash_push_usage_stdout() {
    println!(
        "usage: git stash [push] [-p | --patch] [-S | --staged] [-k | --[no-]keep-index] [-q | --quiet]"
    );
}

/// git's assumed-`push` guard: with no explicit subcommand, options are parsed up
/// to the first non-option token (`STOP_AT_NON_OPTION`); if that token is not `--`
/// (and `--patch` did not force the assume), die with the unexpected-token error.
fn stash_reject_assumed_push_token(args: &[String]) -> Result<()> {
    let mut force_assume = false;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        // `--patch`/`-p` forces the push assumption (patch mode has no positional
        // pathspec ambiguity), so a following token is allowed.
        if arg == "-p" || arg == "--patch" || (arg.starts_with("-p") && arg.len() > 2) {
            force_assume = true;
            index += 1;
            continue;
        }
        if arg == "--" {
            return Ok(());
        }
        if arg.starts_with('-') {
            // An option (possibly one that consumes the next arg). Treat
            // value-taking short/long options conservatively: `-m`/`--message`,
            // `-U`/`--unified`, `--inter-hunk-context`, `--pathspec-from-file`
            // consume the following token when given separately.
            if matches!(
                arg.as_str(),
                "-m" | "--message"
                    | "-U"
                    | "--unified"
                    | "--inter-hunk-context"
                    | "--pathspec-from-file"
            ) {
                index += 2;
            } else {
                index += 1;
            }
            continue;
        }
        // First bare positional token in assumed mode.
        if force_assume {
            return Ok(());
        }
        return Err(stash_assumed_push_unexpected_token(arg));
    }
    Ok(())
}

fn stash_assumed_push_unexpected_token(token: &str) -> GitError {
    eprintln!(
        "fatal: subcommand wasn't specified; 'push' can't be assumed due to unexpected token '{token}'"
    );
    GitError::Exit(128)
}

pub(super) struct StashApplyOptions {
    quiet: bool,
    reinstate_index: Option<bool>,
    explicit_selector: bool,
    selector: usize,
    spec: Option<String>,
    display: String,
    direct_oid: Option<ObjectId>,
    label_ours: Option<String>,
    label_theirs: Option<String>,
    label_base: Option<String>,
}

struct AppliedStash {
    common_git_dir: PathBuf,
    format: ObjectFormat,
    selector: usize,
    display: String,
}

fn cmd_stash_apply(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
    let mut options = setup_stash_apply_options(args, "apply")?;
    options.reinstate_index = Some(
        options
            .reinstate_index
            .unwrap_or(stash_index_config_default(cli_session)?),
    );
    apply_stash_entry(cli_session, options)?;
    Ok(())
}

fn cmd_stash_pop(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
    let mut options = setup_stash_apply_options(args, "pop")?;
    options.reinstate_index = Some(
        options
            .reinstate_index
            .unwrap_or(stash_index_config_default(cli_session)?),
    );
    let quiet = options.quiet;
    let applied = apply_stash_entry(cli_session, options)?;
    drop_stash_entry(
        &applied.common_git_dir,
        applied.format,
        applied.selector,
        &applied.display,
        quiet,
    )
}

fn cmd_stash_branch(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
    if args.is_empty() {
        eprintln!("No branch name specified");
        return Err(GitError::Exit(1));
    }
    if args.len() > 2 {
        eprintln!("Too many revisions specified: '{}' '{}'", args[1], args[2]);
        return Err(GitError::Exit(1));
    }
    if args[0].starts_with('-') {
        eprintln!("usage: git stash branch <branchname> [<stash>]");
        return Err(GitError::Exit(129));
    }
    let branch = &args[0];
    let display = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "refs/stash@{0}".to_string());
    let git_dir = cli_session.git_dir()?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let format = repository_object_format(&common_git_dir)?;
    let store = FileRefStore::new(&common_git_dir, format);
    let db = FileObjectDatabase::from_git_dir(&common_git_dir, format);
    let stash_oid = resolve_stash_argument(
        &common_git_dir,
        format,
        &store,
        &db,
        args.get(1).map(String::as_str),
        cli_session.replace_objects(),
    )?;
    let stash_object = db.read_object(&stash_oid)?;
    let stash_commit = Commit::parse(format, &stash_object.body)?;
    let base_oid = stash_commit
        .parents
        .first()
        .ok_or_else(|| GitError::InvalidObject(format!("stash {stash_oid} has no parent")))?;
    cmd_checkout(
        cli_session,
        &["-b".to_string(), branch.clone(), base_oid.to_hex()],
    )?;
    let applied = apply_stash_entry(
        cli_session,
        StashApplyOptions {
            quiet: false,
            reinstate_index: Some(true),
            explicit_selector: true,
            selector: match args.get(1) {
                Some(spec) => parse_stash_drop_selector(spec).unwrap_or(0),
                None => 0,
            },
            spec: args.get(1).cloned(),
            display,
            direct_oid: Some(stash_oid),
            label_ours: None,
            label_theirs: None,
            label_base: None,
        },
    )?;
    if args
        .get(1)
        .is_none_or(|spec| stash_argument_names_stash_ref(spec))
    {
        drop_stash_entry(
            &applied.common_git_dir,
            applied.format,
            applied.selector,
            &applied.display,
            false,
        )?;
    }
    Ok(())
}

fn stash_apply_parse_combined_quiet(value: &str, command: &str) -> Result<()> {
    for byte in value.as_bytes()[2..].iter().copied() {
        if byte != b'q' {
            return stash_apply_unknown_switch_error(command, byte as char);
        }
    }
    Ok(())
}

fn stash_apply_unknown_option_error<T>(command: &str, value: &str) -> Result<T> {
    eprintln!("error: unknown option `{}'", value.trim_start_matches('-'));
    stash_apply_usage(command);
    Err(GitError::Exit(129))
}

fn stash_apply_unknown_switch_error<T>(command: &str, switch: char) -> Result<T> {
    eprintln!("error: unknown switch `{switch}'");
    stash_apply_usage(command);
    Err(GitError::Exit(129))
}

fn stash_apply_usage(command: &str) {
    if command == "apply" {
        eprintln!(
            "usage: git stash {command} [--index] [-q | --quiet] [--label-ours=<label>] [--label-theirs=<label>] [--label-base=<label>] [<stash>]"
        );
    } else {
        eprintln!("usage: git stash {command} [--index] [-q | --quiet] [<stash>]");
    }
    eprintln!();
    eprintln!("    -q, --[no-]quiet      be quiet, only report errors");
    eprintln!("    --[no-]index          attempt to recreate the index");
    if command == "apply" {
        eprintln!("    --[no-]label-ours <label>");
        eprintln!("                          label for the upstream side in conflict markers");
        eprintln!("    --[no-]label-theirs <label>");
        eprintln!("                          label for the stashed side in conflict markers");
        eprintln!("    --[no-]label-base <label>");
        eprintln!("                          label for the base in diff3 conflict markers");
    }
    eprintln!();
}

fn apply_stash_entry(
    cli_session: &crate::session::CliSession,
    options: StashApplyOptions,
) -> Result<AppliedStash> {
    let git_dir = cli_session.git_dir()?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let worktree_root = worktree_root_for_git_dir(cli_session, &git_dir)?;
    let format = repository_object_format(&common_git_dir)?;
    let config = read_repo_config(&common_git_dir)?;
    let store = FileRefStore::new(&common_git_dir, format);
    let db = FileObjectDatabase::from_git_dir(&common_git_dir, format);
    let entries = store.read_reflog("refs/stash")?;
    let mut selector = options.selector;
    let stash_oid = if let Some(oid) = options.direct_oid {
        oid
    } else if let Some(spec) = options
        .spec
        .as_deref()
        .filter(|_| options.explicit_selector)
    {
        let oid = resolve_stash_argument(
            &common_git_dir,
            format,
            &store,
            &db,
            Some(spec),
            cli_session.replace_objects(),
        )?;
        if let Some((index, _entry)) = entries
            .iter()
            .enumerate()
            .rev()
            .find(|(_index, entry)| entry.new_oid == oid)
        {
            selector = entries.len() - 1 - index;
        }
        oid
    } else {
        if entries.is_empty() {
            if options.explicit_selector {
                eprintln!("error: {} is not a valid reference", options.display);
                return Err(GitError::Exit(1));
            }
            eprintln!("No stash entries found.");
            return Err(GitError::Exit(1));
        }
        if options.selector >= entries.len() {
            eprintln!("fatal: log for 'stash' only has {} entries", entries.len());
            return Err(GitError::Exit(128));
        }
        let entry_index = entries.len() - 1 - options.selector;
        entries[entry_index].new_oid
    };
    validate_stash_like_commit(&db, format, &stash_oid)?;
    let stash_object = db.read_object(&stash_oid)?;
    let stash_commit = Commit::parse(format, &stash_object.body)?;
    let head_store = FileRefStore::new(&git_dir, format);
    let Some((_head_oid, _head_commit)) = stash_head_commit(&head_store, &db, format)? else {
        eprintln!("You do not have the initial commit yet");
        return Err(GitError::Exit(1));
    };
    let base_oid = stash_commit
        .parents
        .first()
        .ok_or_else(|| GitError::InvalidObject(format!("stash {stash_oid} has no parent")))?;
    let base_object = db.read_object(base_oid)?;
    if base_object.object_type != ObjectType::Commit {
        return Err(GitError::InvalidObject(format!(
            "stash parent {base_oid} is not a commit"
        )));
    }
    let base_commit = Commit::parse(format, &base_object.body)?;
    let index_oid = stash_commit
        .parents
        .get(1)
        .ok_or_else(|| GitError::InvalidObject(format!("stash {stash_oid} has no index parent")))?;
    let index_object = db.read_object(index_oid)?;
    if index_object.object_type != ObjectType::Commit {
        return Err(GitError::InvalidObject(format!(
            "stash index parent {index_oid} is not a commit"
        )));
    }
    let index_commit = Commit::parse(format, &index_object.body)?;
    // The stash carries no tracked-tree changes when its working tree equals its
    // base tree (e.g. an untracked-only stash). merge-recursive prints
    // "Already up to date." in that case, before the status.
    let tracked_tree_unchanged = base_commit.tree == stash_commit.tree;

    // Apply the stash by 3-way-merging the stash's working tree (`theirs`) into
    // the CURRENT index/worktree (`ours`), using the stash's base tree as the
    // merge-base — exactly git's `merge_ort_nonrecursive(c_tree, w_tree, b_tree)`
    // in builtin/stash.c. This is the same merge primitive the rebase/cherry-pick
    // porcelains use, so dirty trees, content conflicts, and conflict-marker /
    // stage rendering all come for free.
    let stash_state = StashApplyState {
        worktree_root: &worktree_root,
        git_dir: &git_dir,
        base_tree: &base_commit.tree,
        stash_tree: &stash_commit.tree,
        index_tree: &index_commit.tree,
    };
    let reinstate_index = options.reinstate_index.unwrap_or(false);
    let labels = StashApplyMergeLabels {
        ours: options.label_ours.as_deref().unwrap_or("Updated upstream"),
        theirs: options.label_theirs.as_deref().unwrap_or("Stashed changes"),
        base: options.label_base.as_deref().unwrap_or("Stash base"),
        style: stash_apply_conflict_style(&git_dir),
    };
    let outcome = apply_stash_via_merge(
        &db,
        format,
        &stash_state,
        reinstate_index,
        &labels,
        &config,
        cli_session.lazy_fetch(),
    )?;

    if let Some(untracked_oid) = stash_commit.parents.get(2) {
        let untracked_object = db.read_object(untracked_oid)?;
        if untracked_object.object_type != ObjectType::Commit {
            return Err(GitError::InvalidObject(format!(
                "stash untracked parent {untracked_oid} is not a commit"
            )));
        }
        let untracked_commit = Commit::parse(format, &untracked_object.body)?;
        restore_stash_tree_to_worktree(&worktree_root, &db, format, &untracked_commit.tree)?;
    }
    if !options.quiet {
        if tracked_tree_unchanged {
            println!("Already up to date.");
        }
        cmd_status(cli_session, &[])?;
    }
    if let StashApplyOutcome::Conflict = outcome {
        // git leaves the conflict in the worktree/index and exits nonzero so the
        // user resolves it; pop must NOT drop the entry on conflict.
        if reinstate_index {
            eprintln!("Index was not unstashed.");
        }
        return Err(GitError::Exit(1));
    }
    Ok(AppliedStash {
        common_git_dir,
        format,
        selector,
        display: options.display,
    })
}

/// The trees that drive a stash apply: the stash base (`merge-base`), the stashed
/// working tree (`theirs`), and the stashed index tree (used for `--index`).
struct StashApplyState<'a> {
    worktree_root: &'a Path,
    git_dir: &'a Path,
    base_tree: &'a ObjectId,
    stash_tree: &'a ObjectId,
    index_tree: &'a ObjectId,
}

struct StashApplyMergeLabels<'a> {
    ours: &'a str,
    theirs: &'a str,
    base: &'a str,
    style: sley_diff_merge::ConflictStyle,
}

enum StashApplyOutcome {
    Clean,
    Conflict,
}

fn stash_apply_conflict_style(git_dir: &Path) -> sley_diff_merge::ConflictStyle {
    crate::read_repo_config(git_dir)
        .ok()
        .and_then(|config| {
            config
                .get("merge", None, "conflictstyle")
                .map(str::to_string)
        })
        .map(|value| match value.as_str() {
            "diff3" | "zdiff3" => sley_diff_merge::ConflictStyle::Diff3,
            _ => sley_diff_merge::ConflictStyle::Merge,
        })
        .unwrap_or(sley_diff_merge::ConflictStyle::Merge)
}

/// Run the 3-way merge that applies a stash onto the current index + worktree and
/// materialize the result. Mirrors `do_apply_stash` in git's builtin/stash.c.
fn apply_stash_via_merge(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    state: &StashApplyState<'_>,
    reinstate_index: bool,
    labels: &StashApplyMergeLabels<'_>,
    config: &GitConfig,
    lazy_fetch: bool,
) -> Result<StashApplyOutcome> {
    stash_check_index_lock(state.git_dir)?;
    // `ours` (git's `c_tree`) is the current index written as a tree. Reject a
    // half-finished merge — git refuses "cannot apply a stash in the middle of a
    // merge" when the index has unmerged (stage>0) entries.
    let mut index = read_repository_index(state.git_dir, format)?.unwrap_or(Index {
        version: 2,
        entries: Vec::new(),
        extensions: Vec::new(),
        checksum: None,
    });
    if index
        .entries
        .iter()
        .any(|entry| index_entry_stage(entry) != 0)
    {
        eprintln!("error: cannot apply a stash in the middle of a merge");
        return Err(GitError::Exit(1));
    }
    expand_sparse_stash_index_view(&mut index, db, format)?;
    let ours_map: MergeTreeMap = index
        .entries
        .iter()
        .filter(|entry| index_entry_stage(entry) == 0)
        .map(|entry| (entry.path.as_bytes().to_vec(), (entry.mode, entry.oid)))
        .collect();

    // For `--index`: stage the stash's index-side changes by 3-way-merging the
    // stash index tree onto the current index. We compute the would-be index map
    // now (before the worktree merge moves things around) and reinstate it after a
    // clean worktree merge, matching git's apply-cached-then-reset_tree dance. git
    // skips this when the stash's base and index trees match (no staged changes) or
    // when the current index already equals the stash index tree.
    let reinstated_index_map = if reinstate_index && state.base_tree != state.index_tree {
        let index_map = sley_diff_merge::flatten_tree(db, format, state.index_tree)?;
        if ours_map == index_map {
            None
        } else {
            let (idx_results, idx_conflicts) = three_way_merge_trees(
                db,
                config,
                lazy_fetch,
                format,
                &sley_diff_merge::flatten_tree(db, format, state.base_tree)?,
                &ours_map,
                &index_map,
                "Updated upstream",
                "Stashed index",
            )?;
            if !idx_conflicts.is_empty() {
                eprintln!("Conflicts in index. Try without --index.");
                return Err(GitError::Exit(1));
            }
            let mut merged: MergeTreeMap = BTreeMap::new();
            for (path, result) in &idx_results {
                if let MergePathResult::Resolved(Some(entry)) = result {
                    merged.insert(path.clone(), *entry);
                }
            }
            Some(merged)
        }
    } else {
        None
    };

    let base_map = sley_diff_merge::flatten_tree(db, format, state.base_tree)?;
    let theirs_map = sley_diff_merge::flatten_tree(db, format, state.stash_tree)?;
    let (results, conflicts) = three_way_merge_trees_styled(
        db,
        config,
        lazy_fetch,
        format,
        &base_map,
        &ours_map,
        &theirs_map,
        labels.ours,
        labels.theirs,
        labels.base,
        labels.style,
    )?;

    // Refuse to clobber local worktree modifications or untracked files that lie
    // in the way of a path the merge would write (unpack_trees' verify steps).
    verify_stash_apply_safe(state.worktree_root, format, &ours_map, &results)?;

    apply_stash_merge_results(
        state.worktree_root,
        state.git_dir,
        db,
        format,
        &ours_map,
        &results,
        lazy_fetch,
    )?;

    if !conflicts.is_empty() {
        // Conflicts stay in the worktree/index for the user to resolve; git does
        // not unstage or reinstate in this case.
        return Ok(StashApplyOutcome::Conflict);
    }

    if let Some(index_map) = reinstated_index_map {
        // `--index` with staged changes to reinstate (git's `has_index`): rewrite
        // the index to the merged index tree. git's `reset_tree(index_tree, 0, 0)`
        // touches the index alone (cache only, not the worktree); write the merged
        // index map as the new stage-0 index, carrying forward stat data for
        // entries whose (mode, oid) is unchanged.
        reinstate_stash_index(state.git_dir, format, &index_map)?;
    } else {
        // Plain `apply` — and `--index` when there were no staged changes (git's
        // `has_index == 0` case): `unstage_changes_unless_new` resets the index
        // back to `ours` for every path that already existed there, keeping only
        // brand-new paths staged. Stash restores changes to the *working tree*; the
        // index should look untouched except for newly-introduced files.
        unstage_changes_unless_new(state.git_dir, format, &ours_map, &results)?;
    }
    Ok(StashApplyOutcome::Clean)
}

/// git's `unstage_changes_unless_new(c_tree)`: after a clean plain-`apply` merge
/// (which stages every cleanly-merged path), revert each touched index entry back
/// to its pre-merge (`ours`) state — unless the path is new (absent from `ours`),
/// in which case the staged addition is kept. The worktree is left as the merge
/// materialized it.
fn unstage_changes_unless_new(
    git_dir: &Path,
    format: ObjectFormat,
    ours_map: &MergeTreeMap,
    results: &BTreeMap<Vec<u8>, MergePathResult>,
) -> Result<()> {
    let index_path = sley_worktree::repository_index_path(git_dir);
    if !index_path.exists() {
        return Ok(());
    }
    let mut index = Index::parse(&fs::read(&index_path)?, format)?;
    let mut stat_cache: BTreeMap<Vec<u8>, IndexEntry> = BTreeMap::new();
    for entry in &index.entries {
        if index_entry_stage(entry) == 0 {
            stat_cache.insert(entry.path.clone().into_bytes(), entry.clone());
        }
    }
    let mut stage0: BTreeMap<Vec<u8>, IndexEntry> = stat_cache.clone();
    for (path, result) in results {
        // Only cleanly-merged paths were staged at stage 0 by the merge; conflicts
        // keep their stage 1/2/3 entries untouched.
        if let MergePathResult::Conflict { .. } = result {
            continue;
        }
        // Existed in `ours`: restore the index entry to ours' version. A path
        // absent from `ours` (newly added by the stash) keeps whatever the merge
        // staged.
        if let Some((mode, oid)) = ours_map.get(path) {
            let entry = match stat_cache.get(path) {
                Some(old) if old.mode == *mode && old.oid == *oid => old.clone(),
                _ => merge_index_entry(path, *mode, *oid, 0),
            };
            stage0.insert(path.clone(), entry);
        }
    }
    let mut entries: Vec<IndexEntry> = index
        .entries
        .iter()
        .filter(|entry| index_entry_stage(entry) != 0)
        .cloned()
        .collect();
    entries.extend(stage0.into_values());
    entries.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| index_entry_stage(left).cmp(&index_entry_stage(right)))
    });
    index.entries = entries;
    index.extensions = Vec::new();
    index.checksum = None;
    index.upgrade_version_for_flags();
    fs::write(&index_path, index.write(format)?)?;
    Ok(())
}

/// Rewrite the index to exactly `index_map` (stage 0). The worktree is left
/// untouched — this is git's `reset_tree(index_tree, 0, 0)` for `--index`.
fn reinstate_stash_index(
    git_dir: &Path,
    format: ObjectFormat,
    index_map: &MergeTreeMap,
) -> Result<()> {
    let index_path = sley_worktree::repository_index_path(git_dir);
    let prior: BTreeMap<Vec<u8>, IndexEntry> = if index_path.exists() {
        Index::parse(&fs::read(&index_path)?, format)?
            .entries
            .into_iter()
            .filter(|entry| index_entry_stage(entry) == 0)
            .map(|entry| (entry.path.clone().into_bytes(), entry))
            .collect()
    } else {
        BTreeMap::new()
    };
    let mut entries: Vec<IndexEntry> = index_map
        .iter()
        .map(|(path, (mode, oid))| match prior.get(path) {
            Some(old) if old.mode == *mode && old.oid == *oid => old.clone(),
            _ => merge_index_entry(path, *mode, *oid, 0),
        })
        .collect();
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    let mut index = Index {
        version: 2,
        entries,
        extensions: Vec::new(),
        checksum: None,
    };
    index.upgrade_version_for_flags();
    fs::write(&index_path, index.write(format)?)?;
    Ok(())
}

/// Pre-flight clobber check: a path whose on-disk content diverges from `ours`
/// (the index) cannot be safely overwritten, and an untracked file may not be
/// clobbered by a newly-introduced path. Matches git's refusal to lose local
/// changes during `stash apply`.
/// The (mode, blob-oid) the worktree path would hash to, lstat-aware: a regular
/// file hashes its content (mode 100644/100755); a symlink hashes its link target
/// string (mode 120000), never the file it points at. `None` when the path is
/// absent or a directory. Mirrors git's `index_path` for the on-disk comparison.
fn worktree_blob_oid(format: ObjectFormat, full: &Path) -> Result<Option<(u32, ObjectId)>> {
    let metadata = match fs::symlink_metadata(full) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        let target = fs::read_link(full)?;
        #[cfg(unix)]
        let body = {
            use std::os::unix::ffi::OsStrExt;
            target.as_os_str().as_bytes().to_vec()
        };
        #[cfg(not(unix))]
        let body = target.to_string_lossy().replace('\\', "/").into_bytes();
        let oid = sley_core::object_id_for_bytes(format, "blob", &body)?;
        return Ok(Some((0o120000, oid)));
    }
    if metadata.is_dir() {
        return Ok(None);
    }
    let bytes = fs::read(full)?;
    let oid = sley_core::object_id_for_bytes(format, "blob", &bytes)?;
    #[cfg(unix)]
    let mode = {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 != 0 {
            0o100755
        } else {
            0o100644
        }
    };
    #[cfg(not(unix))]
    let mode = 0o100644;
    Ok(Some((mode, oid)))
}

fn verify_stash_apply_safe(
    worktree_root: &Path,
    format: ObjectFormat,
    ours_map: &MergeTreeMap,
    results: &BTreeMap<Vec<u8>, MergePathResult>,
) -> Result<()> {
    let mut local_changes: Vec<Vec<u8>> = Vec::new();
    let mut untracked: Vec<Vec<u8>> = Vec::new();
    for (path, result) in results {
        let (target, changes) = match result {
            MergePathResult::Resolved(entry) => (*entry, ours_map.get(path) != entry.as_ref()),
            MergePathResult::Conflict { .. } => (None, true),
        };
        if !changes {
            continue;
        }
        let Ok(rel) = std::str::from_utf8(path) else {
            continue;
        };
        let full = worktree_root.join(rel);
        match ours_map.get(path) {
            Some((ours_mode, ours_oid)) => {
                // Compute the on-disk blob the way git does: lstat, and for a
                // symlink hash the *link target* (mode 120000), not the file it
                // points at (`fs::read` would follow it and read the target's
                // content). Without this a symlink-vs-file path looks "dirty".
                let Some((on_disk_mode, on_disk)) = worktree_blob_oid(format, &full)? else {
                    continue;
                };
                if &on_disk != ours_oid || on_disk_mode != *ours_mode {
                    local_changes.push(path.clone());
                }
            }
            None => {
                let would_write =
                    target.is_some() || matches!(result, MergePathResult::Conflict { .. });
                if would_write && fs::symlink_metadata(&full).is_ok() {
                    untracked.push(path.clone());
                }
            }
        }
    }
    if !local_changes.is_empty() {
        eprintln!(
            "error: Your local changes to the following files would be overwritten by merge:"
        );
        for path in &local_changes {
            eprintln!("\t{}", String::from_utf8_lossy(path));
        }
        eprintln!("Please commit your changes or stash them before you merge.");
        eprintln!("Aborting");
        return Err(GitError::Exit(1));
    }
    if !untracked.is_empty() {
        eprintln!(
            "error: The following untracked working tree files would be overwritten by merge:"
        );
        for path in &untracked {
            eprintln!("\t{}", String::from_utf8_lossy(path));
        }
        eprintln!("Please move or remove them before you merge.");
        eprintln!("Aborting");
        return Err(GitError::Exit(1));
    }
    Ok(())
}

/// Apply merge results to the index (stage-0 for resolved paths, stages 1/2/3 for
/// conflicts) and the worktree (write resolved/conflict-marker blobs, remove
/// deletions). Mirrors replay.rs' `apply_merge_results_to_index_and_worktree`.
fn apply_stash_merge_results(
    worktree_root: &Path,
    git_dir: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    ours_map: &MergeTreeMap,
    results: &BTreeMap<Vec<u8>, MergePathResult>,
    lazy_fetch: bool,
) -> Result<()> {
    let index_path = sley_worktree::repository_index_path(git_dir);
    let mut old_index = if index_path.exists() {
        Index::parse(&fs::read(&index_path)?, format)?
    } else {
        Index {
            version: 2,
            entries: Vec::new(),
            extensions: Vec::new(),
            checksum: None,
        }
    };
    expand_sparse_stash_index_view(&mut old_index, db, format)?;
    let mut old_entries: BTreeMap<Vec<u8>, IndexEntry> = BTreeMap::new();
    for entry in &old_index.entries {
        if index_entry_stage(entry) == 0 {
            old_entries.insert(entry.path.clone().into_bytes(), entry.clone());
        }
    }

    // Materialize the worktree BEFORE building the index so freshly-resolved
    // stage-0 entries can record the on-disk stat (git refreshes cleanly-merged
    // results via fill_stat_cache_info; a zeroed stat makes diff-files report the
    // path as modified). Reused entries whose (mode, oid) match `ours` are not
    // rewritten and keep their existing stat.
    for (path, result) in results {
        match result {
            MergePathResult::Resolved(Some((mode, oid))) => {
                if ours_map.get(path) != Some(&(*mode, *oid)) {
                    let content = merge_read_blob(db, oid, lazy_fetch)?;
                    merge_write_worktree_file(worktree_root, path, &content, *mode)?;
                }
            }
            MergePathResult::Resolved(None) => {
                if ours_map.contains_key(path) {
                    merge_remove_worktree_file(worktree_root, path)?;
                }
            }
            MergePathResult::Conflict { worktree, .. } => match worktree {
                Some((mode, content)) => {
                    merge_write_worktree_file(worktree_root, path, content, *mode)?
                }
                None => merge_remove_worktree_file(worktree_root, path)?,
            },
        }
    }

    let mut entries: Vec<IndexEntry> = Vec::new();
    for (path, result) in results {
        match result {
            MergePathResult::Resolved(Some((mode, oid))) => {
                if let Some(old) = old_entries.get(path)
                    && old.mode == *mode
                    && old.oid == *oid
                {
                    entries.push(old.clone());
                } else {
                    let mut entry = merge_index_entry(path, *mode, *oid, 0);
                    if !sley_index::is_gitlink(*mode)
                        && let Ok(rel) = std::str::from_utf8(path)
                        && let Ok(metadata) = fs::symlink_metadata(worktree_root.join(rel))
                    {
                        sley_worktree::fill_index_entry_stat_cache(&mut entry, &metadata);
                    }
                    entries.push(entry);
                }
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
    // Preserve stage-0 index entries for paths the merge did not touch.
    for (path, entry) in &old_entries {
        if !results.contains_key(path) {
            entries.push(entry.clone());
        }
    }
    entries.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| index_entry_stage(left).cmp(&index_entry_stage(right)))
    });
    let mut index = Index {
        version: 2,
        entries,
        extensions: Vec::new(),
        checksum: None,
    };
    index.upgrade_version_for_flags();
    fs::write(&index_path, index.write(format)?)?;
    Ok(())
}

/// Expand a sparse index into semantic leaf entries without touching disk.
/// Stash tree merges are defined over repository paths; synthetic directory
/// rows are an index storage detail and would otherwise be treated as files.
fn expand_sparse_stash_index_view(
    index: &mut Index,
    db: &FileObjectDatabase,
    format: ObjectFormat,
) -> Result<()> {
    if !index.is_sparse() {
        return Ok(());
    }
    for entry in &mut index.entries {
        if entry.mode == sley_index::SPARSE_DIR_MODE && entry.path.as_bytes().ends_with(b"/") {
            entry.set_skip_worktree(true);
        }
    }
    sley_worktree::expand_sparse_index_view(index, db, format).map(|_| ())
}

fn stash_tree_changed_paths(
    left: &BTreeMap<Vec<u8>, (u32, ObjectId)>,
    right: &BTreeMap<Vec<u8>, (u32, ObjectId)>,
) -> BTreeSet<Vec<u8>> {
    let mut paths = BTreeSet::new();
    paths.extend(left.keys().cloned());
    paths.extend(right.keys().cloned());
    paths
        .into_iter()
        .filter(|path| left.get(path) != right.get(path))
        .collect()
}

fn stash_changed_pathbufs(paths: &BTreeSet<Vec<u8>>) -> Result<Vec<PathBuf>> {
    paths
        .iter()
        .map(|path| stash_repo_path_to_os_path(path))
        .collect()
}

fn restore_stash_tree_to_worktree(
    worktree_root: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &ObjectId,
) -> Result<()> {
    restore_stash_tree_entries_to_worktree(worktree_root, db, format, tree_oid, Vec::new())
}

fn restore_stash_tree_entries_to_worktree(
    worktree_root: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    prefix: Vec<u8>,
) -> Result<()> {
    let object = db.read_object(tree_oid)?;
    if object.object_type != ObjectType::Tree {
        return Err(GitError::InvalidObject(format!(
            "expected tree {tree_oid}, found {}",
            object.object_type.as_str()
        )));
    }
    for entry in TreeEntries::new(format, &object.body) {
        let entry = entry?;
        let mut path = prefix.clone();
        if !path.is_empty() {
            path.push(b'/');
        }
        path.extend_from_slice(entry.name);
        if entry.mode == 0o040000 {
            restore_stash_tree_entries_to_worktree(worktree_root, db, format, &entry.oid, path)?;
            continue;
        }
        let object = db.read_object(&entry.oid)?;
        if object.object_type != ObjectType::Blob {
            return Err(GitError::InvalidObject(format!(
                "expected blob {}, found {}",
                entry.oid,
                object.object_type.as_str()
            )));
        }
        merge_write_worktree_file(worktree_root, &path, &object.body, entry.mode)?;
    }
    Ok(())
}

fn cmd_stash_clear(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
    if let Some(arg) = args.first() {
        if arg.starts_with('-') {
            eprintln!("error: unknown option `{}'", arg.trim_start_matches('-'));
            eprintln!("usage: git stash clear");
            eprintln!();
            return Err(GitError::Exit(129));
        }
        eprintln!("error: git stash clear with arguments is unimplemented");
        return Err(GitError::Exit(1));
    }
    let git_dir = cli_session.git_dir()?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let format = repository_object_format(&common_git_dir)?;
    let store = FileRefStore::new(&common_git_dir, format);
    match store.delete_ref("refs/stash") {
        Ok(_) | Err(GitError::NotFound(_)) => {
            let _ = fs::remove_file(common_git_dir.join("logs").join("refs/stash"));
            Ok(())
        }
        Err(err) => Err(err),
    }
}

fn cmd_stash_drop(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
    let mut quiet = false;
    let mut specs = Vec::new();
    for arg in args {
        match arg.as_str() {
            "-q" | "--quiet" => quiet = true,
            "--no-quiet" => quiet = false,
            value if value.starts_with('-') => {
                eprintln!("error: unknown option `{}'", value.trim_start_matches('-'));
                stash_drop_usage();
                return Err(GitError::Exit(129));
            }
            value => specs.push(value.to_string()),
        }
    }
    if specs.len() > 1 {
        eprintln!(
            "Too many revisions specified: '{}' '{}'",
            specs[0], specs[1]
        );
        return Err(GitError::Exit(1));
    }
    let display = specs
        .first()
        .cloned()
        .unwrap_or_else(|| "refs/stash@{0}".to_string());
    let selector = match specs.first() {
        Some(spec) => parse_stash_drop_selector(spec)?,
        None => 0,
    };

    let git_dir = cli_session.git_dir()?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let format = repository_object_format(&common_git_dir)?;
    drop_stash_entry(&common_git_dir, format, selector, &display, quiet)
}

fn drop_stash_entry(
    common_git_dir: &Path,
    format: ObjectFormat,
    selector: usize,
    display: &str,
    quiet: bool,
) -> Result<()> {
    let store = FileRefStore::new(common_git_dir, format);
    let plan =
        match sley_sequencer::stash::plan_stash_drop(store.read_reflog("refs/stash")?, selector) {
            Ok(plan) => plan,
            Err(sley_sequencer::stash::StashDropError::Empty) => {
                eprintln!("No stash entries found.");
                return Err(GitError::Exit(1));
            }
            Err(sley_sequencer::stash::StashDropError::OutOfRange { available }) => {
                eprintln!("fatal: log for 'stash' only has {available} entries");
                return Err(GitError::Exit(128));
            }
        };
    if plan.remaining.is_empty() {
        match store.delete_ref("refs/stash") {
            Ok(_) | Err(GitError::NotFound(_)) => {
                let _ = fs::remove_file(common_git_dir.join("logs").join("refs/stash"));
            }
            Err(err) => return Err(err),
        }
    } else {
        let new_top = plan
            .new_tip
            .ok_or_else(|| GitError::InvalidFormat("stash reflog has no top entry".into()))?;
        let mut tx = store.transaction();
        tx.update(RefUpdate {
            name: "refs/stash".to_string(),
            expected: None,
            new: RefTarget::Direct(new_top),
            reflog: None,
        });
        tx.commit()?;
        store.write_reflog("refs/stash", &plan.remaining)?;
    }
    if !quiet {
        println!("Dropped {display} ({})", plan.dropped.new_oid.to_hex());
    }
    Ok(())
}

fn parse_stash_drop_selector(spec: &str) -> Result<usize> {
    // git accepts `stash@{n}`, `refs/stash@{n}`, and the bare shorthand `n`
    // (`git stash drop 1`). A bare numeric token is treated as `stash@{n}`.
    let Some(selector) = stash_numeric_selector(spec) else {
        return Err(stash_invalid_reference_error(spec));
    };
    selector
}

fn stash_numeric_selector(spec: &str) -> Option<Result<usize>> {
    let selector = spec
        .strip_prefix("stash@{")
        .or_else(|| spec.strip_prefix("refs/stash@{"))
        .and_then(|rest| rest.strip_suffix('}'))
        .or_else(|| spec.chars().all(|c| c.is_ascii_digit()).then_some(spec));
    selector
        .filter(|value| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
        .map(|selector| {
            selector
                .parse::<usize>()
                .map_err(|_| stash_invalid_reference_error(spec))
        })
}

fn stash_invalid_reference_error(spec: &str) -> GitError {
    eprintln!("error: {spec} is not a valid reference");
    GitError::Exit(1)
}

fn stash_drop_usage() {
    eprintln!("usage: git stash drop [-q | --quiet] [<stash>]");
    eprintln!();
    eprintln!("    -q, --[no-]quiet      be quiet, only report errors");
    eprintln!();
}

fn cmd_stash_create(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
    if let Some(created) = create_stash_commit(
        cli_session,
        args,
        false,
        false,
        StashCreateMode::Worktree,
        &[],
        false,
    )? {
        println!("{}", created.oid);
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StashCreateMode {
    Worktree,
    Staged,
}

fn cmd_stash_push(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
    let mut quiet = false;
    let mut include_untracked = false;
    let mut include_ignored = false;
    let mut keep_index = false;
    let mut create_mode = StashCreateMode::Worktree;
    let mut patch = false;
    let mut no_auto_advance = false;
    let mut unified_context: Option<i64> = None;
    let mut inter_hunk_context: Option<i64> = None;
    let mut message_args = Vec::new();
    let mut pathspecs = Vec::new();
    let mut pathspec_from_file = None;
    let mut pathspec_file_nul = false;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        match arg.as_str() {
            "-q" | "--quiet" => quiet = true,
            "--no-quiet" => quiet = false,
            "-u" | "--include-untracked" => include_untracked = true,
            "--no-include-untracked" => {
                include_untracked = false;
                include_ignored = false;
            }
            "-a" | "--all" => {
                include_untracked = true;
                include_ignored = true;
            }
            "--no-all" => {
                include_untracked = false;
                include_ignored = false;
            }
            "-k" | "--keep-index" => keep_index = true,
            "--no-keep-index" => keep_index = false,
            "-S" | "--staged" => create_mode = StashCreateMode::Staged,
            "--no-staged" => create_mode = StashCreateMode::Worktree,
            "-p" | "--patch" => patch = true,
            "--no-patch" => patch = false,
            value if value.starts_with("--patch=") => {
                return stash_option_takes_no_value_error("patch");
            }
            value if value.starts_with("--no-patch=") => {
                return stash_option_takes_no_value_error("no-patch");
            }
            "--auto-advance" => {}
            "--no-auto-advance" => no_auto_advance = true,
            value if value.starts_with("--auto-advance=") => {
                return stash_option_takes_no_value_error("auto-advance");
            }
            value if value.starts_with("--no-auto-advance=") => {
                return stash_option_takes_no_value_error("no-auto-advance");
            }
            "-U" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return commit_unified_requires_value_error(true);
                };
                patch_validate_unified_context(value, true)?;
                unified_context = value.parse::<i64>().ok();
            }
            value if value.starts_with("-U") && value.len() > 2 => {
                let value = &value[2..];
                patch_validate_unified_context(value, true)?;
                unified_context = value.parse::<i64>().ok();
            }
            "--unified" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return commit_unified_requires_value_error(false);
                };
                patch_validate_unified_context(value, false)?;
                unified_context = value.parse::<i64>().ok();
            }
            "--unified=" => {
                return commit_unified_expects_numerical_value_error(false);
            }
            value if value.starts_with("--unified=") => {
                let value = &value["--unified=".len()..];
                patch_validate_unified_context(value, false)?;
                unified_context = value.parse::<i64>().ok();
            }
            "--inter-hunk-context" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return commit_inter_hunk_context_requires_value_error();
                };
                patch_validate_inter_hunk_context(value)?;
                inter_hunk_context = value.parse::<i64>().ok();
            }
            "--inter-hunk-context=" => {
                return commit_inter_hunk_context_expects_numerical_value_error();
            }
            value if value.starts_with("--inter-hunk-context=") => {
                let value = &value["--inter-hunk-context=".len()..];
                patch_validate_inter_hunk_context(value)?;
                inter_hunk_context = value.parse::<i64>().ok();
            }
            "-m" | "--message" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    let option = arg.trim_start_matches('-');
                    if arg.starts_with("--") {
                        eprintln!("error: option `{option}' requires a value");
                    } else {
                        eprintln!("error: switch `{option}' requires a value");
                    }
                    return Err(GitError::Exit(129));
                };
                message_args = vec![value.clone()];
            }
            value if let Some(value) = value.strip_prefix("--message=") => {
                message_args = vec![value.to_string()];
            }
            value if value.starts_with("-m") && value.len() > 2 => {
                message_args = vec![value[2..].to_string()];
            }
            "--no-message" => message_args.clear(),
            value if value.starts_with("--no-message=") => {
                return stash_option_takes_no_value_error("no-message");
            }
            "--" => {
                if pathspec_from_file.is_some() && index + 1 < args.len() {
                    return stash_pathspec_from_file_with_inline_pathspec_error();
                }
                pathspecs.extend(args[index + 1..].iter().cloned());
                break;
            }
            "--pathspec-from-file" => {
                if !pathspecs.is_empty() {
                    return stash_pathspec_from_file_with_inline_pathspec_error();
                }
                index += 1;
                let Some(value) = args.get(index) else {
                    return stash_pathspec_from_file_requires_value_error();
                };
                pathspec_from_file = Some(PathBuf::from(value));
            }
            value if let Some(value) = value.strip_prefix("--pathspec-from-file=") => {
                if !pathspecs.is_empty() {
                    return stash_pathspec_from_file_with_inline_pathspec_error();
                }
                pathspec_from_file = Some(PathBuf::from(value));
            }
            "--no-pathspec-from-file" => {}
            value if value.starts_with("--no-pathspec-from-file=") => {
                return stash_option_takes_no_value_error("no-pathspec-from-file");
            }
            "--pathspec-file-nul" => pathspec_file_nul = true,
            "--no-pathspec-file-nul" => pathspec_file_nul = false,
            value if value.starts_with("--pathspec-file-nul=") => {
                return stash_option_takes_no_value_error("pathspec-file-nul");
            }
            value if value.starts_with("--no-pathspec-file-nul=") => {
                return stash_option_takes_no_value_error("no-pathspec-file-nul");
            }
            value if value.starts_with('-') => {
                eprintln!("error: unknown option `{}`", value.trim_start_matches('-'));
                stash_push_usage_stdout();
                return Err(GitError::Exit(129));
            }
            value => {
                if pathspec_from_file.is_some() {
                    return stash_pathspec_from_file_with_inline_pathspec_error();
                }
                pathspecs.push(value.to_string());
            }
        }
        index += 1;
    }
    if pathspec_file_nul && pathspec_from_file.is_none() {
        eprintln!("fatal: the option '--pathspec-file-nul' requires '--pathspec-from-file'");
        return Err(GitError::Exit(128));
    }
    if let Some(pathspec_file) = pathspec_from_file.as_deref() {
        pathspecs.extend(
            read_commit_pathspecs_from_file(pathspec_file, pathspec_file_nul)?
                .into_iter()
                .map(|path| path.to_string_lossy().into_owned()),
        );
    }
    if create_mode == StashCreateMode::Staged && include_untracked {
        eprintln!("Can't use --staged and --include-untracked or --all at the same time");
        return Err(GitError::Exit(1));
    }
    if no_auto_advance && !patch {
        return stash_patch_option_requires_patch_error("no-auto-advance");
    }
    if unified_context.is_some() && !patch {
        return stash_patch_option_requires_patch_error("unified");
    }
    if inter_hunk_context.is_some() && !patch {
        return stash_patch_option_requires_patch_error("inter-hunk-context");
    }
    if patch {
        if include_untracked {
            eprintln!("Can't use --patch and --include-untracked or --all at the same time");
            return Err(GitError::Exit(1));
        }
        return stash_push_patch(
            cli_session,
            &message_args,
            &pathspecs,
            quiet,
            unified_context,
            inter_hunk_context,
            !no_auto_advance,
        );
    }
    let Some(created) = create_stash_commit(
        cli_session,
        &message_args,
        include_untracked,
        include_ignored,
        create_mode,
        &pathspecs,
        quiet,
    )?
    else {
        if !quiet {
            println!("No local changes to save");
        }
        return Ok(());
    };

    store_created_stash(created, quiet, keep_index)
}

fn stash_pathspec_from_file_requires_value_error<T>() -> Result<T> {
    eprintln!("error: option `pathspec-from-file' requires a value");
    Err(GitError::Exit(129))
}

fn stash_pathspec_from_file_with_inline_pathspec_error<T>() -> Result<T> {
    eprintln!("fatal: '--pathspec-from-file' and pathspec arguments cannot be used together");
    Err(GitError::Exit(128))
}

fn stash_option_takes_no_value_error<T>(option: &str) -> Result<T> {
    eprintln!("error: option `{option}' takes no value");
    Err(GitError::Exit(129))
}

fn stash_patch_option_requires_patch_error<T>(option: &str) -> Result<T> {
    eprintln!("fatal: the option '--{option}' requires '--patch'");
    Err(GitError::Exit(128))
}

fn stash_push_patch(
    cli_session: &crate::session::CliSession,
    message_args: &[String],
    pathspecs: &[String],
    quiet: bool,
    unified_context: Option<i64>,
    inter_hunk_context: Option<i64>,
    auto_advance: bool,
) -> Result<()> {
    let git_dir = cli_session.git_dir()?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let worktree_root = worktree_root_for_git_dir(cli_session, &git_dir)?;
    let format = repository_object_format(&common_git_dir)?;
    stash_check_index_lock_quiet(&git_dir, quiet)?;
    let index_path = sley_worktree::repository_index_path(&git_dir);
    let original_index_bytes = fs::read(&index_path).ok();
    let original_index = read_repository_index(&git_dir, format)?.unwrap_or(Index {
        version: 2,
        entries: Vec::new(),
        extensions: Vec::new(),
        checksum: None,
    });
    let original_entries = original_index
        .entries
        .iter()
        .filter(|entry| index_entry_stage(entry) == 0)
        .cloned()
        .collect::<Vec<_>>();
    if original_entries.iter().any(IndexEntry::is_intent_to_add) {
        if !quiet {
            eprintln!("Cannot save the current index state");
        }
        return Err(GitError::Exit(1));
    }

    let applied = match commands::add_interactive::cmd_stash_patch(
        cli_session,
        pathspecs,
        unified_context,
        inter_hunk_context,
        auto_advance,
        quiet,
    ) {
        Ok(applied) => applied,
        Err(err) => {
            stash_restore_index_bytes(&index_path, original_index_bytes.as_deref())?;
            return Err(err);
        }
    };
    if !applied {
        stash_restore_index_bytes(&index_path, original_index_bytes.as_deref())?;
        return Ok(());
    }

    let result = stash_record_selected_patch(
        message_args,
        quiet,
        &git_dir,
        &common_git_dir,
        &worktree_root,
        &sley_core::original_cwd().unwrap_or_else(|| cli_session.cwd().to_path_buf()),
        format,
        &original_entries,
        pathspecs,
    );
    let restore_result = stash_restore_index_bytes(&index_path, original_index_bytes.as_deref());
    match (result, restore_result) {
        (Err(err), _) => Err(err),
        (Ok(()), Err(err)) => Err(err),
        (Ok(()), Ok(())) => Ok(()),
    }
}

#[allow(clippy::too_many_arguments)]
fn stash_record_selected_patch(
    message_args: &[String],
    quiet: bool,
    git_dir: &Path,
    common_git_dir: &Path,
    worktree_root: &Path,
    preserved_cwd: &Path,
    format: ObjectFormat,
    original_entries: &[IndexEntry],
    pathspecs: &[String],
) -> Result<()> {
    let mut db = FileObjectDatabase::from_git_dir(common_git_dir, format);
    let store = FileRefStore::new(git_dir, format);
    let Some((head_oid, head_commit)) = stash_head_commit(&store, &db, format)? else {
        if !quiet {
            eprintln!("You do not have the initial commit yet");
        }
        return Err(GitError::Exit(1));
    };
    let selected_index = read_repository_index(git_dir, format)?.unwrap_or(Index {
        version: 2,
        entries: Vec::new(),
        extensions: Vec::new(),
        checksum: None,
    });
    let selected_entries = selected_index
        .entries
        .iter()
        .filter(|entry| index_entry_stage(entry) == 0)
        .cloned()
        .collect::<Vec<_>>();
    let original_tree = stash_write_tree_from_entries(&mut db, original_entries)?;
    let selected_tree = stash_write_tree_from_entries(&mut db, &selected_entries)?;
    if original_tree == selected_tree {
        if !quiet {
            println!("No local changes to save");
        }
        return Ok(());
    }

    let reverse_patch = stash_selected_patch(pathspecs)?;
    let branch = store
        .current_branch()?
        .unwrap_or_else(|| "(no branch)".to_string());
    let head_name = format_log_oid(&head_oid, Some(7));
    let head_subject = commit_subject(&head_commit.message);
    let config = read_repo_config(git_dir).unwrap_or_default();
    let author = stash_identity_from_env("AUTHOR", &config)?;
    let committer = stash_identity_from_env("COMMITTER", &config)?;
    let index_commit = sley_sequencer::create_commit(
        &mut db,
        sley_sequencer::CommitCreate {
            tree: original_tree,
            parents: vec![head_oid],
            author: author.clone(),
            committer: committer.clone(),
            message: format!("index on {branch}: {head_name} {head_subject}\n").into_bytes(),
            encoding: None,
            signature: None,
        },
    )?;
    let message = if message_args.is_empty() {
        format!("WIP on {branch}: {head_name} {head_subject}")
    } else {
        format!("On {branch}: {}", message_args.join(" "))
    };
    let stash_oid = sley_sequencer::create_commit(
        &mut db,
        sley_sequencer::CommitCreate {
            tree: selected_tree,
            parents: vec![head_oid, index_commit],
            author,
            committer: committer.clone(),
            message: message.as_bytes().to_vec(),
            encoding: None,
            signature: None,
        },
    )?;
    let created = CreatedStash {
        oid: stash_oid,
        message,
        committer,
        head_oid,
        index_oid: index_commit,
        git_dir: git_dir.to_path_buf(),
        common_git_dir: common_git_dir.to_path_buf(),
        worktree_root: worktree_root.to_path_buf(),
        preserved_cwd: preserved_cwd.to_path_buf(),
        untracked_paths: Vec::new(),
        pathspec_paths: Vec::new(),
        has_pathspec: false,
        staged_worktree_conflicts: Vec::new(),
        format,
    };
    record_created_stash(&created)?;
    stash_reverse_apply_selected_patch(&reverse_patch)?;
    if !quiet {
        println!(
            "Saved working directory and index state {}",
            created.message
        );
    }
    Ok(())
}

fn stash_selected_patch(pathspecs: &[String]) -> Result<Vec<u8>> {
    let mut args = vec!["diff".to_string(), "--cached".to_string(), "-p".to_string()];
    if !pathspecs.is_empty() {
        args.push("--".to_string());
        args.extend(pathspecs.iter().cloned());
    }
    let output = Command::new(env::current_exe().unwrap_or_else(|_| PathBuf::from("sley")))
        .args(args)
        .output()
        .map_err(|err| GitError::Io(err.to_string()))?;
    if !output.status.success() {
        return Err(GitError::Exit(output.status.code().unwrap_or(1)));
    }
    Ok(output.stdout)
}

fn stash_reverse_apply_selected_patch(patch: &[u8]) -> Result<()> {
    let mut child = Command::new(env::current_exe().unwrap_or_else(|_| PathBuf::from("sley")))
        .args(["apply", "-R"])
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|err| GitError::Io(err.to_string()))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(patch)
            .map_err(|err| GitError::Io(err.to_string()))?;
    }
    let status = child.wait().map_err(|err| GitError::Io(err.to_string()))?;
    if !status.success() {
        return Err(GitError::Exit(status.code().unwrap_or(1)));
    }
    Ok(())
}

fn stash_restore_index_bytes(index_path: &Path, original: Option<&[u8]>) -> Result<()> {
    if let Some(bytes) = original {
        fs::write(index_path, bytes)?;
    } else {
        match fs::remove_file(index_path) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
    }
    Ok(())
}

fn cmd_stash_save(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
    let mut quiet = false;
    let mut include_untracked = false;
    let mut include_ignored = false;
    let mut keep_index = false;
    let mut create_mode = StashCreateMode::Worktree;
    let mut patch = false;
    let mut no_auto_advance = false;
    let mut unified_context: Option<i64> = None;
    let mut inter_hunk_context: Option<i64> = None;
    let mut explicit_message = Vec::new();
    let mut positional_message = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        match arg.as_str() {
            "-q" | "--quiet" => quiet = true,
            "--no-quiet" => quiet = false,
            "-h" | "--help" => {
                stash_push_usage_stdout();
                return Err(GitError::Exit(129));
            }
            "-u" | "--include-untracked" => include_untracked = true,
            "--no-include-untracked" => {
                include_untracked = false;
                include_ignored = false;
            }
            "-a" | "--all" => {
                include_untracked = true;
                include_ignored = true;
            }
            "--no-all" => {
                include_untracked = false;
                include_ignored = false;
            }
            "-k" | "--keep-index" => keep_index = true,
            "--no-keep-index" => keep_index = false,
            "-S" | "--staged" => create_mode = StashCreateMode::Staged,
            "--no-staged" => create_mode = StashCreateMode::Worktree,
            "-p" | "--patch" => patch = true,
            "--no-patch" => patch = false,
            value if value.starts_with("--patch=") => {
                return stash_option_takes_no_value_error("patch");
            }
            value if value.starts_with("--no-patch=") => {
                return stash_option_takes_no_value_error("no-patch");
            }
            "--auto-advance" => {}
            "--no-auto-advance" => no_auto_advance = true,
            value if value.starts_with("--auto-advance=") => {
                return stash_option_takes_no_value_error("auto-advance");
            }
            value if value.starts_with("--no-auto-advance=") => {
                return stash_option_takes_no_value_error("no-auto-advance");
            }
            "-U" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return commit_unified_requires_value_error(true);
                };
                patch_validate_unified_context(value, true)?;
                unified_context = value.parse::<i64>().ok();
            }
            value if value.starts_with("-U") && value.len() > 2 => {
                let value = &value[2..];
                patch_validate_unified_context(value, true)?;
                unified_context = value.parse::<i64>().ok();
            }
            "--unified" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return commit_unified_requires_value_error(false);
                };
                patch_validate_unified_context(value, false)?;
                unified_context = value.parse::<i64>().ok();
            }
            "--unified=" => {
                return commit_unified_expects_numerical_value_error(false);
            }
            value if value.starts_with("--unified=") => {
                let value = &value["--unified=".len()..];
                patch_validate_unified_context(value, false)?;
                unified_context = value.parse::<i64>().ok();
            }
            "--inter-hunk-context" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return commit_inter_hunk_context_requires_value_error();
                };
                patch_validate_inter_hunk_context(value)?;
                inter_hunk_context = value.parse::<i64>().ok();
            }
            "--inter-hunk-context=" => {
                return commit_inter_hunk_context_expects_numerical_value_error();
            }
            value if value.starts_with("--inter-hunk-context=") => {
                let value = &value["--inter-hunk-context=".len()..];
                patch_validate_inter_hunk_context(value)?;
                inter_hunk_context = value.parse::<i64>().ok();
            }
            "-m" | "--message" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    let option = arg.trim_start_matches('-');
                    if arg.starts_with("--") {
                        eprintln!("error: option `{option}' requires a value");
                    } else {
                        eprintln!("error: switch `{option}' requires a value");
                    }
                    return Err(GitError::Exit(129));
                };
                explicit_message = vec![value.clone()];
            }
            value if let Some(value) = value.strip_prefix("--message=") => {
                explicit_message = vec![value.to_string()];
            }
            value if value.starts_with("-m") && value.len() > 2 => {
                explicit_message = vec![value[2..].to_string()];
            }
            "--no-message" => explicit_message.clear(),
            value if value.starts_with("--no-message=") => {
                return stash_option_takes_no_value_error("no-message");
            }
            "--" => {
                positional_message.extend(args[index..].iter().cloned());
                break;
            }
            value if value.starts_with('-') => {
                eprintln!("error: unknown option `{}`", value.trim_start_matches('-'));
                stash_push_usage_stdout();
                return Err(GitError::Exit(129));
            }
            value => positional_message.push(value.to_string()),
        }
        index += 1;
    }
    let message_args = if positional_message.is_empty() {
        explicit_message
    } else {
        positional_message
    };
    if create_mode == StashCreateMode::Staged && include_untracked {
        eprintln!("Can't use --staged and --include-untracked or --all at the same time");
        return Err(GitError::Exit(1));
    }
    if no_auto_advance && !patch {
        return stash_patch_option_requires_patch_error("no-auto-advance");
    }
    if unified_context.is_some() && !patch {
        return stash_patch_option_requires_patch_error("unified");
    }
    if inter_hunk_context.is_some() && !patch {
        return stash_patch_option_requires_patch_error("inter-hunk-context");
    }
    if patch {
        if include_untracked {
            eprintln!("Can't use --patch and --include-untracked or --all at the same time");
            return Err(GitError::Exit(1));
        }
        return stash_push_patch(
            cli_session,
            &message_args,
            &[],
            quiet,
            unified_context,
            inter_hunk_context,
            !no_auto_advance,
        );
    }
    let Some(created) = create_stash_commit(
        cli_session,
        &message_args,
        include_untracked,
        include_ignored,
        create_mode,
        &[],
        quiet,
    )?
    else {
        if !quiet {
            println!("No local changes to save");
        }
        return Ok(());
    };
    store_created_stash(created, quiet, keep_index)
}

fn store_created_stash(created: CreatedStash, quiet: bool, keep_index: bool) -> Result<()> {
    record_created_stash(&created)?;
    if !quiet {
        println!(
            "Saved working directory and index state {}",
            created.message
        );
    }
    cleanup_stored_stash(created, quiet, keep_index)
}

fn record_created_stash(created: &CreatedStash) -> Result<()> {
    let store = FileRefStore::new(&created.common_git_dir, created.format);
    commit_stash_store_plan(
        &store,
        created.oid,
        created.committer.clone(),
        created.message.as_bytes().to_vec(),
    )?;
    Ok(())
}

fn commit_stash_store_plan(
    store: &FileRefStore,
    stash_oid: ObjectId,
    committer: Vec<u8>,
    message: Vec<u8>,
) -> Result<bool> {
    let plan = sley_sequencer::stash::plan_stash_store(sley_sequencer::stash::StashStoreOptions {
        current: store.read_ref("refs/stash")?,
        stash_oid,
        committer,
        message,
    });
    let Some(update) = plan.update else {
        return Ok(false);
    };
    let mut tx = store.transaction();
    tx.update(update);
    tx.commit()?;
    Ok(true)
}

fn cleanup_stored_stash(created: CreatedStash, quiet: bool, keep_index: bool) -> Result<()> {
    if !created.staged_worktree_conflicts.is_empty() {
        report_stash_staged_worktree_conflicts(&created.staged_worktree_conflicts, quiet);
        return Err(GitError::Exit(1));
    }

    for path in &created.untracked_paths {
        remove_stashed_untracked_path(&created.worktree_root, path, &created.preserved_cwd)?;
    }
    prune_empty_untracked_dirs_preserving_cwd(&created.worktree_root, &created.preserved_cwd)?;

    if created.pathspec_paths.is_empty() {
        if !created.has_pathspec {
            let reset_oid = if keep_index {
                &created.index_oid
            } else {
                &created.head_oid
            };
            sley_worktree::reset_index_and_worktree_to_commit(
                &created.worktree_root,
                &created.git_dir,
                created.format,
                reset_oid,
            )?;
        }
    } else if keep_index {
        sley_worktree::restore_worktree_paths(
            &created.worktree_root,
            &created.git_dir,
            created.format,
            &created.pathspec_paths,
        )?;
    } else {
        sley_worktree::restore_index_and_worktree_paths_from_head(
            &created.worktree_root,
            &created.git_dir,
            created.format,
            &created.pathspec_paths,
            false,
        )?;
    }
    Ok(())
}

fn report_stash_staged_worktree_conflicts(paths: &[Vec<u8>], quiet: bool) {
    for path in paths {
        let path = String::from_utf8_lossy(path);
        eprintln!("error: patch failed: {path}:1");
        eprintln!("error: {path}: patch does not apply");
    }
    if !quiet {
        eprintln!("Cannot remove worktree changes");
    }
}

struct CreatedStash {
    oid: ObjectId,
    message: String,
    committer: Vec<u8>,
    head_oid: ObjectId,
    index_oid: ObjectId,
    git_dir: PathBuf,
    common_git_dir: PathBuf,
    worktree_root: PathBuf,
    preserved_cwd: PathBuf,
    untracked_paths: Vec<Vec<u8>>,
    pathspec_paths: Vec<PathBuf>,
    has_pathspec: bool,
    staged_worktree_conflicts: Vec<Vec<u8>>,
    format: ObjectFormat,
}

fn create_stash_commit(
    cli_session: &crate::session::CliSession,
    args: &[String],
    include_untracked: bool,
    include_ignored: bool,
    mode: StashCreateMode,
    pathspecs: &[String],
    quiet: bool,
) -> Result<Option<CreatedStash>> {
    let git_dir = cli_session.git_dir()?;
    let worktree_root = worktree_root_for_git_dir(cli_session, &git_dir)?;
    create_stash_commit_at(
        &git_dir,
        &worktree_root,
        cli_session.cwd(),
        args,
        include_untracked,
        include_ignored,
        mode,
        pathspecs,
        quiet,
        effective_pathspec_flags(cli_session),
    )
}

#[allow(clippy::too_many_arguments)]
fn create_stash_commit_at(
    git_dir: &Path,
    worktree_root: &Path,
    cwd: &Path,
    args: &[String],
    include_untracked: bool,
    include_ignored: bool,
    mode: StashCreateMode,
    pathspecs: &[String],
    quiet: bool,
    pathspec_magic: sley_worktree::PathspecMatchMagic,
) -> Result<Option<CreatedStash>> {
    let cwd = cwd.to_path_buf();
    let git_dir = git_dir.to_path_buf();
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let worktree_root = worktree_root.to_path_buf();
    let format = repository_object_format(&common_git_dir)?;
    stash_check_index_lock_quiet(&git_dir, quiet)?;
    let mut db = FileObjectDatabase::from_git_dir(&common_git_dir, format);
    let store = FileRefStore::new(&git_dir, format);
    let Some((head_oid, head_commit)) = stash_head_commit(&store, &db, format)? else {
        if !quiet {
            eprintln!("You do not have the initial commit yet");
        }
        return Err(GitError::Exit(1));
    };
    let mut index = read_repository_index(&git_dir, format)?.unwrap_or(Index {
        version: 2,
        entries: Vec::new(),
        extensions: Vec::new(),
        checksum: None,
    });
    // Stash trees contain tracked leaf entries. A sparse index may represent an
    // excluded subtree with a synthetic `040000 path/` entry, which is not a
    // valid tree-builder leaf and cannot describe the staged/worktree delta.
    // Expand only this in-memory snapshot; the repository index stays sparse.
    expand_sparse_stash_index_view(&mut index, &db, format)?;
    let index_entries = index
        .entries
        .iter()
        .filter(|entry| index_entry_stage(entry) == 0)
        .cloned()
        .collect::<Vec<_>>();
    if index_entries.iter().any(IndexEntry::is_intent_to_add) {
        if !quiet {
            eprintln!("Cannot save the current index state");
        }
        return Err(GitError::Exit(1));
    }
    let pathspec = if pathspecs.is_empty() {
        None
    } else {
        Some(LsFilesPathspec::new(
            &cwd,
            &worktree_root,
            true,
            pathspecs,
            pathspec_magic,
        )?)
    };
    let head_entries = sley_diff_merge::flatten_tree(&db, format, &head_commit.tree)?;
    let index_tree = stash_write_tree_from_entries(&mut db, &index_entries)?;
    let worktree_entries = stash_worktree_entries(
        &worktree_root,
        &mut db,
        &index_entries,
        &head_entries,
        pathspec.as_ref(),
    )?;
    let worktree_tree = stash_write_tree_from_entries(&mut db, &worktree_entries)?;
    let mut untracked_paths = if include_untracked {
        sley_worktree::untracked_paths_with_options(
            &worktree_root,
            &git_dir,
            format,
            sley_worktree::UntrackedPathOptions {
                directory: false,
                no_empty_directory: false,
                preserve_ignored_directories: false,
                exclude_standard: !include_ignored,
                ignored_only: false,
                exclude_patterns: Vec::new(),
                exclude_per_directory: Vec::new(),
                pathspecs: Vec::new(),
            },
        )?
    } else {
        Vec::new()
    };
    if let Some(pathspec) = pathspec.as_ref() {
        untracked_paths.retain(|path| pathspec.matches(path));
    }
    let untracked_entries = stash_untracked_entries(&worktree_root, &mut db, &untracked_paths)?;
    let untracked_tree = if untracked_entries.is_empty() {
        None
    } else {
        Some(stash_write_tree_from_entries(&mut db, &untracked_entries)?)
    };
    let mut pathspec_paths = Vec::new();
    let mut staged_worktree_conflicts = Vec::new();
    if let Some(pathspec) = pathspec.as_ref() {
        let index_entry_map = stash_index_entry_map(&index_entries);
        let worktree_entry_map = stash_index_entry_map(&worktree_entries);
        let tracked_change_paths = stash_pathspec_tracked_change_paths(
            pathspec,
            &head_entries,
            &index_entry_map,
            &worktree_entry_map,
        )?;
        if !include_untracked {
            pathspec.exit_if_unmatched()?;
        }
        if tracked_change_paths.is_empty() && untracked_tree.is_none() {
            return Ok(None);
        }
        pathspec_paths = tracked_change_paths;
    }
    if index_tree == head_commit.tree
        && worktree_tree == head_commit.tree
        && untracked_tree.is_none()
    {
        return Ok(None);
    }
    if mode == StashCreateMode::Staged {
        if index_tree == head_commit.tree {
            if worktree_tree != head_commit.tree {
                eprintln!("No staged changes");
                return Err(GitError::Exit(1));
            }
            return Ok(None);
        }
        let index_entry_map = stash_index_entry_map(&index_entries);
        let worktree_entry_map = stash_index_entry_map(&worktree_entries);
        let staged_change_paths = stash_tree_changed_paths(&head_entries, &index_entry_map);
        let unstaged_change_paths = stash_tree_changed_paths(&index_entry_map, &worktree_entry_map);
        staged_worktree_conflicts = staged_change_paths
            .iter()
            .filter(|path| unstaged_change_paths.contains(*path))
            .cloned()
            .collect();
        pathspec_paths = stash_changed_pathbufs(&staged_change_paths)?;
    }

    let branch = store
        .current_branch()?
        .unwrap_or_else(|| "(no branch)".to_string());
    let head_name = format_log_oid(&head_oid, Some(7));
    let head_subject = commit_subject(&head_commit.message);
    let config = read_repo_config(&git_dir).unwrap_or_default();
    let author = stash_identity_from_env("AUTHOR", &config)?;
    let committer = stash_identity_from_env("COMMITTER", &config)?;
    let index_commit = sley_sequencer::create_commit(
        &mut db,
        sley_sequencer::CommitCreate {
            tree: index_tree.clone(),
            parents: vec![head_oid],
            author: author.clone(),
            committer: committer.clone(),
            message: format!("index on {branch}: {head_name} {head_subject}\n").into_bytes(),
            encoding: None,
            signature: None,
        },
    )?;
    let untracked_commit = if let Some(tree) = untracked_tree {
        Some(sley_sequencer::create_commit(
            &mut db,
            sley_sequencer::CommitCreate {
                tree,
                parents: Vec::new(),
                author: author.clone(),
                committer: committer.clone(),
                message: format!("untracked files on {branch}: {head_name} {head_subject}\n")
                    .into_bytes(),
                encoding: None,
                signature: None,
            },
        )?)
    } else {
        None
    };
    let message = if args.is_empty() {
        format!("WIP on {branch}: {head_name} {head_subject}")
    } else {
        format!("On {branch}: {}", args.join(" "))
    };
    let mut parents = vec![head_oid, index_commit.clone()];
    if let Some(untracked_commit) = untracked_commit {
        parents.push(untracked_commit);
    }
    let stash_oid = sley_sequencer::create_commit(
        &mut db,
        sley_sequencer::CommitCreate {
            tree: if mode == StashCreateMode::Staged {
                index_tree
            } else {
                worktree_tree
            },
            parents,
            author,
            committer: committer.clone(),
            message: message.as_bytes().to_vec(),
            encoding: None,
            signature: None,
        },
    )?;
    Ok(Some(CreatedStash {
        oid: stash_oid,
        message,
        committer,
        head_oid,
        index_oid: index_commit,
        git_dir,
        common_git_dir,
        preserved_cwd: sley_core::original_cwd().unwrap_or_else(|| cwd.clone()),
        worktree_root,
        untracked_paths,
        pathspec_paths,
        has_pathspec: !pathspecs.is_empty(),
        staged_worktree_conflicts,
        format,
    }))
}

fn stash_check_index_lock(git_dir: &Path) -> Result<()> {
    stash_check_index_lock_quiet(git_dir, false)
}

fn stash_check_index_lock_quiet(git_dir: &Path, quiet: bool) -> Result<()> {
    let lock_path = sley_worktree::repository_index_path(git_dir).with_file_name("index.lock");
    if lock_path.exists() {
        if !quiet {
            eprintln!("error: could not write index");
            eprintln!(
                "error: Unable to create '{}': File exists.",
                lock_path.display()
            );
        }
        return Err(GitError::Exit(1));
    }
    Ok(())
}

fn stash_identity_from_env(role: &str, effective_config: &GitConfig) -> Result<Vec<u8>> {
    let env_name = env::var(format!("GIT_{role}_NAME")).ok();
    let env_email = env::var(format!("GIT_{role}_EMAIL")).ok();
    let mut config = if env_name.is_none() || env_email.is_none() {
        IdentityConfig::Loaded(effective_config)
    } else {
        IdentityConfig::Skip
    };
    let name = env_name
        .or_else(|| identity_config_value("user.name", &mut config))
        .unwrap_or_else(|| "git stash".into());
    let email = env_email
        .or_else(|| identity_config_value("user.email", &mut config))
        .unwrap_or_else(|| "git@stash".into());
    let date = env::var(format!("GIT_{role}_DATE")).unwrap_or_else(|_| "@0 +0000".into());
    let date = canonicalize_commit_date(&date);
    sley_sequencer::format_commit_identity(&name, &email, &date)
}

fn stash_index_config_default(cli_session: &crate::session::CliSession) -> Result<bool> {
    if let Some(value) = global_config_value("stash.index")? {
        return Ok(sley_config::parse_config_bool(&value).unwrap_or(false));
    }
    let cwd = cli_session.cwd();
    let git_dir = cli_session.git_dir()?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let context = sley_config::ConfigIncludeContext::new(
        Some(common_git_dir.clone()),
        repo_current_branch_name(&git_dir),
    );
    let Ok(mut config) = sley_config::load_effective_config(&common_git_dir, &context) else {
        return Ok(false);
    };
    let parameters_env = effective_config_parameters_env();
    if let Ok(parameters) = sley_config::injected_config_parameters(parameters_env.as_deref()) {
        let _ = sley_config::append_injected_config_sections_with_includes(
            &mut config,
            &parameters,
            &context,
            cwd,
        );
    }
    Ok(config.get_bool("stash", None, "index").unwrap_or(false))
}

fn stash_show_include_untracked_config_default(
    cwd: &Path,
    git_dir: &Path,
    common_git_dir: &Path,
) -> Result<bool> {
    if let Some(value) = global_config_value("stash.showIncludeUntracked")? {
        return Ok(sley_config::parse_config_bool(&value).unwrap_or(false));
    }
    let context = sley_config::ConfigIncludeContext::new(
        Some(common_git_dir.to_path_buf()),
        repo_current_branch_name(git_dir),
    );
    let Ok(mut config) = sley_config::load_effective_config(common_git_dir, &context) else {
        return Ok(false);
    };
    let parameters_env = effective_config_parameters_env();
    if let Ok(parameters) = sley_config::injected_config_parameters(parameters_env.as_deref()) {
        let _ = sley_config::append_injected_config_sections_with_includes(
            &mut config,
            &parameters,
            &context,
            cwd,
        );
    }
    Ok(config
        .get_bool("stash", None, "showIncludeUntracked")
        .unwrap_or(false))
}

fn stash_argument_names_stash_ref(spec: &str) -> bool {
    spec.bytes().all(|byte| byte.is_ascii_digit())
        || spec.starts_with("stash@{")
        || spec.starts_with("refs/stash@{")
        || spec == "stash"
        || spec == "refs/stash"
}

fn stash_head_commit(
    store: &FileRefStore,
    db: &FileObjectDatabase,
    format: ObjectFormat,
) -> Result<Option<(ObjectId, Commit)>> {
    let target = match store.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(name)) => store.read_ref(&name)?,
        direct => direct,
    };
    let Some(RefTarget::Direct(oid)) = target else {
        return Ok(None);
    };
    let object = db.read_object(&oid)?;
    if object.object_type != ObjectType::Commit {
        return Err(GitError::InvalidObject(format!(
            "expected commit {}, found {}",
            oid,
            object.object_type.as_str()
        )));
    }
    Ok(Some((oid, Commit::parse(format, &object.body)?)))
}

#[derive(Default)]
struct StashTreeNode {
    files: Vec<TreeEntry>,
    directories: BTreeMap<Vec<u8>, StashTreeNode>,
}

impl StashTreeNode {
    fn insert(&mut self, entry: &IndexEntry) -> Result<()> {
        let components = entry.path.split(|byte| *byte == b'/').collect::<Vec<_>>();
        if components.iter().any(|component| component.is_empty()) {
            return Err(GitError::InvalidPath(format!(
                "invalid index path {}",
                String::from_utf8_lossy(&entry.path)
            )));
        }
        self.insert_components(&components, entry)
    }

    fn insert_components(&mut self, components: &[&[u8]], entry: &IndexEntry) -> Result<()> {
        match components {
            [] => Err(GitError::InvalidPath("empty index path".into())),
            [name] => {
                self.files.push(TreeEntry {
                    mode: entry.mode,
                    name: BString::from(*name),
                    oid: entry.oid,
                });
                Ok(())
            }
            [directory, rest @ ..] => self
                .directories
                .entry(directory.to_vec())
                .or_default()
                .insert_components(rest, entry),
        }
    }
}

fn stash_write_tree_from_entries(
    db: &mut FileObjectDatabase,
    entries: &[IndexEntry],
) -> Result<ObjectId> {
    let mut root = StashTreeNode::default();
    for entry in entries {
        root.insert(entry)?;
    }
    stash_write_tree_node(db, &root)
}

fn stash_write_tree_node(db: &mut FileObjectDatabase, node: &StashTreeNode) -> Result<ObjectId> {
    let mut entries = Vec::with_capacity(node.files.len() + node.directories.len());
    entries.extend(node.files.iter().cloned());
    for (name, child) in &node.directories {
        entries.push(TreeEntry {
            mode: 0o040000,
            name: BString::from(name.clone()),
            oid: stash_write_tree_node(db, child)?,
        });
    }
    entries.sort_by(|left, right| {
        tree_entry_cmp(
            left.name.as_bytes(),
            left.mode,
            right.name.as_bytes(),
            right.mode,
        )
    });
    db.write_object(EncodedObject::new(
        ObjectType::Tree,
        Tree { entries }.write(),
    ))
}

fn stash_index_entry_map(entries: &[IndexEntry]) -> BTreeMap<Vec<u8>, (u32, ObjectId)> {
    entries
        .iter()
        .map(|entry| (entry.path.as_bytes().to_vec(), (entry.mode, entry.oid)))
        .collect()
}

fn stash_pathspec_tracked_change_paths(
    pathspec: &LsFilesPathspec,
    head_entries: &BTreeMap<Vec<u8>, (u32, ObjectId)>,
    index_entries: &BTreeMap<Vec<u8>, (u32, ObjectId)>,
    worktree_entries: &BTreeMap<Vec<u8>, (u32, ObjectId)>,
) -> Result<Vec<PathBuf>> {
    let mut paths = BTreeSet::new();
    paths.extend(head_entries.keys().cloned());
    paths.extend(index_entries.keys().cloned());
    paths.extend(worktree_entries.keys().cloned());
    let mut changed = Vec::new();
    for path in paths {
        if pathspec.matches(&path)
            && (head_entries.get(&path) != index_entries.get(&path)
                || index_entries.get(&path) != worktree_entries.get(&path))
        {
            changed.push(stash_repo_path_to_os_path(&path)?);
        }
    }
    Ok(changed)
}

fn stash_worktree_entries(
    worktree_root: &Path,
    db: &mut FileObjectDatabase,
    index_entries: &[IndexEntry],
    head_entries: &BTreeMap<Vec<u8>, (u32, ObjectId)>,
    pathspec: Option<&LsFilesPathspec>,
) -> Result<Vec<IndexEntry>> {
    let mut entries = Vec::new();
    let mut seen = BTreeSet::new();
    for entry in index_entries {
        seen.insert(entry.path.as_bytes().to_vec());
        if pathspec.is_some_and(|pathspec| !pathspec.matches(&entry.path)) {
            entries.push(entry.clone());
            continue;
        }
        if entry.is_skip_worktree() {
            entries.push(entry.clone());
            continue;
        }
        let path = stash_repo_path_to_os_path(&entry.path)?;
        let absolute = worktree_root.join(path);
        // lstat: a tracked path that became a symlink must be captured as the link
        // (mode 120000, blob = target), not followed (which `fs::metadata` does,
        // dropping a dangling symlink entirely).
        let Ok(metadata) = fs::symlink_metadata(&absolute) else {
            continue;
        };
        if let Some(stashed) =
            stash_capture_worktree_entry(db, &absolute, &metadata, entry.path.clone())?
        {
            entries.push(stashed);
        } else {
            entries.push(entry.clone());
        }
    }
    for (path, (mode, oid)) in head_entries {
        if seen.contains(path) {
            continue;
        }
        if pathspec.is_some_and(|pathspec| !pathspec.matches(path)) {
            entries.push(merge_index_entry(path, *mode, *oid, 0));
            continue;
        }
        let os_path = stash_repo_path_to_os_path(path)?;
        let absolute = worktree_root.join(os_path);
        let Ok(metadata) = fs::symlink_metadata(&absolute) else {
            continue;
        };
        if let Some(stashed) =
            stash_capture_worktree_entry(db, &absolute, &metadata, BString::from(path.as_slice()))?
        {
            entries.push(stashed);
        }
    }
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(entries)
}

/// Capture one worktree path as a stash index entry. Regular files record their
/// content blob; symlinks record the link target as a mode-120000 blob (matching
/// git's index_path). Anything else (a directory / gitlink) returns `None` so the
/// caller keeps the original index entry.
fn stash_capture_worktree_entry(
    db: &mut FileObjectDatabase,
    absolute: &Path,
    metadata: &fs::Metadata,
    path: BString,
) -> Result<Option<IndexEntry>> {
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        let target = fs::read_link(absolute)?;
        #[cfg(unix)]
        let body = {
            use std::os::unix::ffi::OsStrExt;
            target.as_os_str().as_bytes().to_vec()
        };
        #[cfg(not(unix))]
        let body = target.to_string_lossy().replace('\\', "/").into_bytes();
        let oid = db.write_object(EncodedObject::new(ObjectType::Blob, body))?;
        let mut index_entry = stash_index_entry_from_metadata(path, oid, metadata);
        index_entry.mode = 0o120000;
        return Ok(Some(index_entry));
    }
    if !metadata.is_file() {
        return Ok(None);
    }
    let oid = db.write_object(EncodedObject::new(ObjectType::Blob, fs::read(absolute)?))?;
    Ok(Some(stash_index_entry_from_metadata(path, oid, metadata)))
}

fn stash_untracked_entries(
    worktree_root: &Path,
    db: &mut FileObjectDatabase,
    paths: &[Vec<u8>],
) -> Result<Vec<IndexEntry>> {
    let mut entries = Vec::new();
    for path in paths {
        let absolute = worktree_root.join(stash_repo_path_to_os_path(path)?);
        // lstat so an untracked symlink is stashed as a link, not followed.
        let metadata = fs::symlink_metadata(&absolute)?;
        if let Some(stashed) =
            stash_capture_worktree_entry(db, &absolute, &metadata, BString::from(path.as_slice()))?
        {
            entries.push(stashed);
        }
    }
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(entries)
}

fn remove_stashed_untracked_path(
    worktree_root: &Path,
    path: &[u8],
    preserved_cwd: &Path,
) -> Result<()> {
    let path = worktree_root.join(stash_repo_path_to_os_path(path)?);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    if metadata.file_type().is_dir() {
        if path.join(".git").exists() {
            eprintln!(
                "Ignoring path {}/",
                stash_untracked_display_path(worktree_root, &path)
            );
            return Ok(());
        }
        match fs::remove_dir(&path) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(err) if err.kind() == io::ErrorKind::DirectoryNotEmpty => return Ok(()),
            Err(err) => return Err(err.into()),
        }
    } else {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(err.into()),
        }
    }
    let mut parent = path.parent();
    while let Some(directory) = parent {
        if directory == worktree_root
            || directory.join(".git").exists()
            || stash_path_is_original_cwd(directory, preserved_cwd)
        {
            break;
        }
        match fs::remove_dir(directory) {
            Ok(()) => parent = directory.parent(),
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::DirectoryNotEmpty
                ) =>
            {
                break;
            }
            Err(err) => return Err(err.into()),
        }
    }
    Ok(())
}

fn stash_untracked_display_path(worktree_root: &Path, path: &Path) -> String {
    path.strip_prefix(worktree_root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn stash_path_is_original_cwd(path: &Path, preserved_cwd: &Path) -> bool {
    let cwd = fs::canonicalize(preserved_cwd).unwrap_or_else(|_| preserved_cwd.to_path_buf());
    let path = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    path == cwd
}

fn prune_empty_untracked_dirs_preserving_cwd(
    worktree_root: &Path,
    preserved_cwd: &Path,
) -> Result<()> {
    fn visit(root: &Path, dir: &Path, preserved_cwd: &Path) -> Result<()> {
        let read = match fs::read_dir(dir) {
            Ok(read) => read,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(err.into()),
        };
        for entry in read {
            let entry = entry?;
            let path = entry.path();
            if path == root || path.file_name().is_some_and(|name| name == ".git") {
                continue;
            }
            if entry.file_type()?.is_dir() {
                visit(root, &path, preserved_cwd)?;
                if !stash_path_is_original_cwd(&path, preserved_cwd) {
                    match fs::remove_dir(&path) {
                        Ok(()) => {}
                        Err(err)
                            if matches!(
                                err.kind(),
                                io::ErrorKind::NotFound | io::ErrorKind::DirectoryNotEmpty
                            ) => {}
                        Err(err) => return Err(err.into()),
                    }
                }
            }
        }
        Ok(())
    }
    visit(worktree_root, worktree_root, preserved_cwd)
}

fn stash_repo_path_to_os_path(path: &[u8]) -> Result<PathBuf> {
    let path = std::str::from_utf8(path)
        .map_err(|_| GitError::InvalidPath("index path is not utf8".into()))?;
    Ok(path.split('/').collect())
}

fn stash_index_entry_from_metadata(
    path: impl Into<BString>,
    oid: ObjectId,
    metadata: &fs::Metadata,
) -> IndexEntry {
    let path = path.into();
    let modified = metadata.modified().ok();
    let duration = modified
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .unwrap_or_default();
    let flags = path.len().min(0x0fff) as u16;
    IndexEntry {
        ctime_seconds: duration.as_secs().min(u32::MAX as u64) as u32,
        ctime_nanoseconds: duration.subsec_nanos(),
        mtime_seconds: duration.as_secs().min(u32::MAX as u64) as u32,
        mtime_nanoseconds: duration.subsec_nanos(),
        dev: 0,
        ino: 0,
        mode: stash_file_mode(metadata),
        uid: 0,
        gid: 0,
        size: metadata.len().min(u32::MAX as u64) as u32,
        oid,
        flags,
        flags_extended: 0,
        path,
    }
}

#[cfg(unix)]
fn stash_file_mode(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;

    if metadata.permissions().mode() & 0o111 != 0 {
        0o100755
    } else {
        0o100644
    }
}

#[cfg(not(unix))]
fn stash_file_mode(_metadata: &fs::Metadata) -> u32 {
    0o100644
}

fn cmd_stash_store(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
    let mut message = b"Created via \"git stash store\".".to_vec();
    let mut commits = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        match arg.as_str() {
            "-q" | "--quiet" | "--no-quiet" => {}
            "-m" | "--message" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    let option = arg.trim_start_matches('-');
                    if arg.starts_with("--") {
                        eprintln!("error: option `{option}' requires a value");
                    } else {
                        eprintln!("error: switch `{option}' requires a value");
                    }
                    return Err(GitError::Exit(129));
                };
                message = value.as_bytes().to_vec();
            }
            value if let Some(value) = value.strip_prefix("--message=") => {
                message = value.as_bytes().to_vec();
            }
            value if value.starts_with("-m") && value.len() > 2 => {
                message = value.as_bytes()[2..].to_vec();
            }
            value => commits.push(value.to_string()),
        }
        index += 1;
    }
    if commits.len() != 1 {
        eprintln!("\"git stash store\" requires one <commit> argument");
        return Err(GitError::Exit(1));
    }
    let commit = &commits[0];

    let git_dir = cli_session.git_dir()?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let format = repository_object_format(&common_git_dir)?;
    let db = FileObjectDatabase::from_git_dir(&common_git_dir, format);
    let stash_oid = match resolve_revision(
        &common_git_dir,
        format,
        commit,
        cli_session.replace_objects(),
    ) {
        Ok(oid) => oid,
        Err(_) => {
            eprintln!("Cannot update refs/stash with {commit}");
            return Err(GitError::Exit(1));
        }
    };
    validate_stash_like_commit(&db, format, &stash_oid)?;

    let store = FileRefStore::new(&common_git_dir, format);
    commit_stash_store_plan(&store, stash_oid, default_committer(), message)?;
    Ok(())
}

/// Resolve a `git stash <subcmd> [<stash>]` argument to its stash commit oid,
/// mirroring git's `parse_stash_revision` + `repo_get_oid` + `assert_stash_like`:
///   * no arg → `stash@{0}` (errors "No stash entries found." if `refs/stash` is
///     absent);
///   * a bare number `n` → `stash@{n}`;
///   * anything else → resolved as a general revision (a raw oid, `stash@{n}`,
///     any commit-ish), then validated to be stash-like (a merge commit).
/// This is what lets `stash show <oid>` / `stash branch <name> <oid>` accept a
/// "stash-like argument" produced by `git stash create`.
fn resolve_stash_argument(
    common_git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    db: &FileObjectDatabase,
    spec: Option<&str>,
    replace_objects: bool,
) -> Result<ObjectId> {
    // git's `parse_stash_revision` expands to the FULL ref `refs/stash@{n}`
    // (`ref_stash`), so the reflog lookup resolves without depending on a `stash`
    // → `refs/stash` dwim that the rev parser may not apply to the short form.
    let revision = match spec {
        None => {
            if store.read_ref("refs/stash")?.is_none() {
                eprintln!("No stash entries found.");
                return Err(GitError::Exit(1));
            }
            "refs/stash@{0}".to_string()
        }
        Some(value) if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) => {
            format!("refs/stash@{{{value}}}")
        }
        Some(value) => value.to_string(),
    };
    let oid = match resolve_revision(common_git_dir, format, &revision, replace_objects) {
        Ok(oid) => oid,
        Err(err) => {
            // A reflog selector whose log is too short (`stash@{99}`) dies the way
            // git's rev parser does — `fatal: log for '<base>' only has N entries`,
            // exit 128 — rather than the generic "is not a valid reference" git
            // reserves for revisions that don't name a reflog at all (`bad`).
            if let Some(message) = err
                .not_found_kind()
                .map(ToString::to_string)
                .filter(|message| message.contains(" only has "))
            {
                eprintln!("fatal: {message}");
                return Err(GitError::Exit(128));
            }
            eprintln!("error: {revision} is not a valid reference");
            return Err(GitError::Exit(1));
        }
    };
    validate_stash_like_commit(db, format, &oid)?;
    Ok(oid)
}

fn validate_stash_like_commit(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    oid: &ObjectId,
) -> Result<()> {
    let object = match db.read_object(oid) {
        Ok(object) => object,
        Err(GitError::NotFound(_)) => {
            eprintln!("fatal: '{oid}' is not a stash-like commit");
            return Err(GitError::Exit(128));
        }
        Err(err) => return Err(err),
    };
    if object.object_type != ObjectType::Commit {
        eprintln!(
            "error: object {oid} is a {}, not a commit",
            object.object_type.as_str()
        );
        eprintln!("fatal: '{oid}' is not a stash-like commit");
        return Err(GitError::Exit(128));
    }
    let commit = Commit::parse(format, &object.body)?;
    if commit.parents.len() < 2 {
        eprintln!("fatal: '{oid}' is not a stash-like commit");
        return Err(GitError::Exit(128));
    }
    for parent in &commit.parents[..2] {
        let Ok(parent_object) = db.read_object(parent) else {
            eprintln!("fatal: '{oid}' is not a stash-like commit");
            return Err(GitError::Exit(128));
        };
        if parent_object.object_type != ObjectType::Commit {
            eprintln!("fatal: '{oid}' is not a stash-like commit");
            return Err(GitError::Exit(128));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
enum StashShowMode {
    Stat,
    NameOnly,
    NameStatus,
    NoPatch,
}

fn cmd_stash_show(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
    let mut mode = StashShowMode::Stat;
    let mut quiet = false;
    let mut exit_code = false;
    let mut show_stat = false;
    let mut show_raw = false;
    let mut show_numstat = false;
    let mut show_shortstat = false;
    let mut show_summary = false;
    let mut compact_summary = false;
    let mut show_patch = false;
    let mut raw_abbrev = Some(Some(7usize));
    let mut patch_abbrev = None;
    let mut patch_full_index = false;
    let mut include_untracked = false;
    let mut only_untracked = false;
    let mut untracked_option_seen = false;
    let mut diff_filter = DiffFilter::default();
    let mut diff_filter_seen = false;
    let mut specs = Vec::new();
    let mut idx = 0;
    while idx < args.len() {
        let arg = &args[idx];
        match arg.as_str() {
            "--stat" => {
                if !matches!(mode, StashShowMode::NameOnly | StashShowMode::NameStatus) {
                    mode = StashShowMode::Stat;
                    show_stat = true;
                }
            }
            value if value.starts_with("--stat=") => {
                eprintln!(
                    "error: invalid --stat value: {}",
                    value.trim_start_matches("--stat=")
                );
                return Err(GitError::Exit(129));
            }
            "--no-stat" | "--no-raw" | "--no-name-only" | "--no-name-status" | "--no-numstat"
            | "--no-shortstat" | "--no-summary" => {
                stash_show_usage();
                return Err(GitError::Exit(129));
            }
            value
                if value.starts_with("--no-stat=")
                    || value.starts_with("--no-raw=")
                    || value.starts_with("--no-name-only=")
                    || value.starts_with("--no-name-status=")
                    || value.starts_with("--no-numstat=")
                    || value.starts_with("--no-shortstat=")
                    || value.starts_with("--no-summary=") =>
            {
                stash_show_usage();
                return Err(GitError::Exit(129));
            }
            "--raw" => {
                if !matches!(mode, StashShowMode::NameOnly | StashShowMode::NameStatus) {
                    mode = StashShowMode::Stat;
                    show_raw = true;
                }
            }
            value if value.starts_with("--raw=") => stash_option_takes_no_value_error("raw")?,
            "--numstat" => {
                if !matches!(mode, StashShowMode::NameOnly | StashShowMode::NameStatus) {
                    mode = StashShowMode::Stat;
                    show_numstat = true;
                }
            }
            value if value.starts_with("--numstat=") => {
                stash_option_takes_no_value_error("numstat")?
            }
            "--shortstat" => {
                if !matches!(mode, StashShowMode::NameOnly | StashShowMode::NameStatus) {
                    mode = StashShowMode::Stat;
                    show_shortstat = true;
                }
            }
            value if value.starts_with("--shortstat=") => {
                stash_option_takes_no_value_error("shortstat")?
            }
            "--summary" => {
                if !matches!(mode, StashShowMode::NameOnly | StashShowMode::NameStatus) {
                    mode = StashShowMode::Stat;
                    show_summary = true;
                }
            }
            value if value.starts_with("--summary=") => {
                stash_option_takes_no_value_error("summary")?
            }
            "--compact-summary" => {
                if !matches!(mode, StashShowMode::NameOnly | StashShowMode::NameStatus) {
                    mode = StashShowMode::Stat;
                    compact_summary = true;
                }
            }
            "--no-compact-summary" => {
                compact_summary = false;
                if stash_show_should_enable_default_patch(
                    mode,
                    [
                        show_raw,
                        show_stat,
                        show_numstat,
                        show_shortstat,
                        show_summary,
                        compact_summary,
                        show_patch,
                    ],
                ) {
                    show_patch = true;
                }
            }
            value if value.starts_with("--compact-summary=") => {
                stash_option_takes_no_value_error("compact-summary")?
            }
            value if value.starts_with("--no-compact-summary=") => {
                stash_option_takes_no_value_error("no-compact-summary")?
            }
            "--patch-with-raw" => {
                if !matches!(mode, StashShowMode::NameOnly | StashShowMode::NameStatus) {
                    mode = StashShowMode::Stat;
                    show_raw = true;
                    show_patch = true;
                }
            }
            "--patch-with-stat" => {
                if !matches!(mode, StashShowMode::NameOnly | StashShowMode::NameStatus) {
                    mode = StashShowMode::Stat;
                    show_stat = true;
                    show_patch = true;
                }
            }
            value if value.starts_with("--patch-with-raw=") => {
                stash_option_takes_no_value_error("patch-with-raw")?
            }
            value if value.starts_with("--patch-with-stat=") => {
                stash_option_takes_no_value_error("patch-with-stat")?
            }
            "--abbrev" => {
                raw_abbrev = Some(Some(7));
                patch_abbrev = Some(7);
            }
            "--no-abbrev" => {
                raw_abbrev = Some(None);
            }
            "--full-index" => patch_full_index = true,
            "--no-full-index" => {
                patch_full_index = false;
                if stash_show_should_enable_default_patch(
                    mode,
                    [
                        show_raw,
                        show_stat,
                        show_numstat,
                        show_shortstat,
                        show_summary,
                        compact_summary,
                        show_patch,
                    ],
                ) {
                    show_patch = true;
                }
            }
            value if value.starts_with("--full-index=") => {
                stash_option_takes_no_value_error("full-index")?
            }
            value if value.starts_with("--no-full-index=") => {
                stash_option_takes_no_value_error("no-full-index")?
            }
            "--name-only" => {
                if matches!(mode, StashShowMode::NameStatus | StashShowMode::NoPatch) {
                    eprintln!(
                        "fatal: options '--name-only', '--name-status', '--check', and '-s' cannot be used together"
                    );
                    return Err(GitError::Exit(128));
                }
                mode = StashShowMode::NameOnly;
            }
            value if value.starts_with("--name-only=") => {
                stash_option_takes_no_value_error("name-only")?
            }
            "--name-status" => {
                if matches!(mode, StashShowMode::NameOnly | StashShowMode::NoPatch) {
                    eprintln!(
                        "fatal: options '--name-only', '--name-status', '--check', and '-s' cannot be used together"
                    );
                    return Err(GitError::Exit(128));
                }
                mode = StashShowMode::NameStatus;
            }
            value if value.starts_with("--name-status=") => {
                stash_option_takes_no_value_error("name-status")?
            }
            "-p" | "--patch" | "--oneline" => {
                if !matches!(mode, StashShowMode::NameOnly | StashShowMode::NameStatus) {
                    mode = StashShowMode::Stat;
                    show_patch = true;
                }
            }
            value if value.starts_with("--patch=") => stash_option_takes_no_value_error("patch")?,
            "-s" | "--no-patch" => {
                show_stat = false;
                show_raw = false;
                show_numstat = false;
                show_shortstat = false;
                show_summary = false;
                compact_summary = false;
                show_patch = false;
                mode = StashShowMode::NoPatch;
            }
            value if value.starts_with("--no-patch=") => {
                stash_option_takes_no_value_error("no-patch")?
            }
            "--quiet" => quiet = true,
            value if value.starts_with("--quiet=") => stash_option_takes_no_value_error("quiet")?,
            "--no-quiet" => {
                quiet = false;
                if stash_show_should_enable_default_patch(
                    mode,
                    [
                        show_raw,
                        show_stat,
                        show_numstat,
                        show_shortstat,
                        show_summary,
                        compact_summary,
                        show_patch,
                    ],
                ) {
                    show_patch = true;
                }
            }
            value if value.starts_with("--no-quiet=") => {
                stash_option_takes_no_value_error("no-quiet")?
            }
            "--exit-code" => {
                exit_code = true;
                if stash_show_should_enable_default_patch(
                    mode,
                    [
                        show_raw,
                        show_stat,
                        show_numstat,
                        show_shortstat,
                        show_summary,
                        compact_summary,
                        show_patch,
                    ],
                ) {
                    show_patch = true;
                }
            }
            value if value.starts_with("--exit-code=") => {
                stash_option_takes_no_value_error("exit-code")?
            }
            "--no-exit-code" => {
                exit_code = false;
                if stash_show_should_enable_default_patch(
                    mode,
                    [
                        show_raw,
                        show_stat,
                        show_numstat,
                        show_shortstat,
                        show_summary,
                        compact_summary,
                        show_patch,
                    ],
                ) {
                    show_patch = true;
                }
            }
            value if value.starts_with("--no-exit-code=") => {
                stash_option_takes_no_value_error("no-exit-code")?
            }
            "--ext-diff" | "--no-ext-diff" | "--textconv" | "--no-textconv" => {
                if stash_show_should_enable_default_patch(
                    mode,
                    [
                        show_raw,
                        show_stat,
                        show_numstat,
                        show_shortstat,
                        show_summary,
                        compact_summary,
                        show_patch,
                    ],
                ) {
                    show_patch = true;
                }
            }
            "--patience" | "--histogram" | "--minimal" => {
                if stash_show_should_enable_default_patch(
                    mode,
                    [
                        show_raw,
                        show_stat,
                        show_numstat,
                        show_shortstat,
                        show_summary,
                        compact_summary,
                        show_patch,
                    ],
                ) {
                    show_patch = true;
                }
            }
            value if value.starts_with("--ext-diff=") => {
                stash_option_takes_no_value_error("ext-diff")?
            }
            value if value.starts_with("--no-ext-diff=") => {
                stash_option_takes_no_value_error("no-ext-diff")?
            }
            value if value.starts_with("--textconv=") => {
                stash_option_takes_no_value_error("textconv")?
            }
            value if value.starts_with("--no-textconv=") => {
                stash_option_takes_no_value_error("no-textconv")?
            }
            "-u" | "--include-untracked" => {
                untracked_option_seen = true;
                include_untracked = true;
                only_untracked = false;
            }
            value if value.starts_with("--include-untracked=") => {
                stash_option_takes_no_value_error("include-untracked")?
            }
            "--no-include-untracked" => {
                untracked_option_seen = true;
                include_untracked = false;
                only_untracked = false;
            }
            value if value.starts_with("--no-include-untracked=") => {
                stash_option_takes_no_value_error("no-include-untracked")?
            }
            "--only-untracked" => {
                untracked_option_seen = true;
                include_untracked = true;
                only_untracked = true;
            }
            value if value.starts_with("--only-untracked=") => {
                stash_option_takes_no_value_error("only-untracked")?
            }
            "--no-only-untracked" => {
                stash_show_usage();
                return Err(GitError::Exit(129));
            }
            value if value.starts_with("--no-only-untracked=") => {
                stash_show_usage();
                return Err(GitError::Exit(129));
            }
            "--diff-filter" => {
                if idx + 1 == args.len() {
                    eprintln!("error: option `diff-filter' requires a value");
                    return Err(GitError::Exit(129));
                }
            }
            value if value.starts_with("--abbrev=") => {
                let value = value
                    .strip_prefix("--abbrev=")
                    .ok_or_else(|| GitError::Command("--abbrev requires a value".into()))?;
                let abbrev = parse_abbrev(value)?.max(4);
                raw_abbrev = Some(Some(abbrev));
                patch_abbrev = Some(abbrev);
            }
            value if value.starts_with("--diff-filter=") => {
                let value = value
                    .strip_prefix("--diff-filter=")
                    .ok_or_else(|| GitError::Command("--diff-filter requires a value".into()))?;
                diff_filter = parse_diff_filter(value)?;
                diff_filter_seen = true;
            }
            value if value.starts_with('-') => {
                eprintln!("error: unknown option `{}`", value.trim_start_matches('-'));
                stash_show_usage();
                return Err(GitError::Exit(129));
            }
            value => specs.push(value.to_string()),
        }
        idx += 1;
    }
    if specs.len() > 1 {
        eprintln!(
            "Too many revisions specified: '{}' '{}'",
            specs[0], specs[1]
        );
        return Err(GitError::Exit(1));
    }
    if matches!(mode, StashShowMode::Stat)
        && diff_filter_seen
        && !(show_raw
            || show_stat
            || show_numstat
            || show_shortstat
            || show_summary
            || compact_summary
            || show_patch)
    {
        show_patch = true;
    }
    let git_dir = cli_session.git_dir()?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let format = repository_object_format(&common_git_dir)?;
    let config = read_repo_config(&common_git_dir)?;
    if !untracked_option_seen {
        include_untracked = stash_show_include_untracked_config_default(
            cli_session.cwd(),
            &git_dir,
            &common_git_dir,
        )?;
    }
    let store = FileRefStore::new(&common_git_dir, format);
    let db = FileObjectDatabase::from_git_dir(&common_git_dir, format);
    // git's `stash show` accepts any stash-like argument (a `stash@{n}` ref, a
    // bare index, or a raw commit-ish such as `git stash create`'s output), not
    // just a reflog selector.
    let stash_oid = resolve_stash_argument(
        &common_git_dir,
        format,
        &store,
        &db,
        specs.first().map(String::as_str),
        cli_session.replace_objects(),
    )?;
    let object = db.read_object(&stash_oid)?;
    if object.object_type != ObjectType::Commit {
        return Err(GitError::InvalidObject(format!(
            "stash {stash_oid} is not a commit"
        )));
    }
    let stash_commit = Commit::parse(format, &object.body)?;
    let base_oid = stash_commit
        .parents
        .first()
        .ok_or_else(|| GitError::InvalidObject(format!("stash {stash_oid} has no parent")))?;
    let base_object = db.read_object(base_oid)?;
    if base_object.object_type != ObjectType::Commit {
        return Err(GitError::InvalidObject(format!(
            "stash parent {base_oid} is not a commit"
        )));
    }
    let base_commit = Commit::parse(format, &base_object.body)?;
    let diff_options = sley_diff_merge::DiffNameStatusOptions::default();
    let mut entries = if only_untracked {
        Vec::new()
    } else {
        sley_diff_merge::diff_name_status_trees_with_options(
            &db,
            format,
            &base_commit.tree,
            &stash_commit.tree,
            diff_options,
        )?
    };
    if include_untracked && let Some(untracked_oid) = stash_commit.parents.get(2) {
        let untracked_object = db.read_object(untracked_oid)?;
        if untracked_object.object_type != ObjectType::Commit {
            return Err(GitError::InvalidObject(format!(
                "stash untracked parent {untracked_oid} is not a commit"
            )));
        }
        let untracked_commit = Commit::parse(format, &untracked_object.body)?;
        if !only_untracked {
            let worktree_tree_entries =
                sley_diff_merge::flatten_tree(&db, format, &stash_commit.tree)?;
            let untracked_tree_entries =
                sley_diff_merge::flatten_tree(&db, format, &untracked_commit.tree)?;
            if let Some(path) = untracked_tree_entries
                .keys()
                .find(|path| worktree_tree_entries.contains_key(*path))
            {
                eprintln!(
                    "error: worktree and untracked commit have duplicate entries: {}",
                    String::from_utf8_lossy(path)
                );
                eprintln!("fatal: failed to unpack trees");
                return Err(GitError::Exit(128));
            }
        }
        entries.extend(sley_diff_merge::diff_name_status_empty_tree_with_options(
            &db,
            format,
            &untracked_commit.tree,
            diff_options,
        )?);
        entries.sort_by(|left, right| left.path.cmp(&right.path));
    }
    let entries: Vec<_> = if diff_filter.all_or_none {
        if !diff_filter.includes.is_empty()
            && entries
                .iter()
                .any(|entry| diff_filter.matches_status(entry.status.code()))
        {
            entries
        } else {
            Vec::new()
        }
    } else {
        entries
            .into_iter()
            .filter(|entry| diff_filter.matches_status(entry.status.code()))
            .collect()
    };
    let mut stdout = io::stdout();
    let repository_abbrev = repository_abbrev(&common_git_dir, format)?;
    let raw_abbrev = match raw_abbrev {
        Some(abbrev) => abbrev.map(|width| width.min(format.hex_len())),
        None => repository_abbrev,
    };
    let patch_abbrev = if patch_full_index {
        format.hex_len()
    } else {
        patch_abbrev
            .or(repository_abbrev)
            .unwrap_or(7)
            .min(format.hex_len())
    };
    let has_entries = !entries.is_empty();
    if quiet {
        if has_entries {
            return Err(GitError::Exit(1));
        }
        return Ok(());
    }
    match mode {
        StashShowMode::Stat => {
            let has_visual_mode = show_raw
                || show_stat
                || show_numstat
                || show_shortstat
                || show_summary
                || compact_summary
                || show_patch;
            if !has_visual_mode {
                show_stat = true;
            }
            let stat_entries = if show_numstat || show_stat || compact_summary || show_shortstat {
                collect_diff_stat_entries(&entries, &db, None, false, cli_session.lazy_fetch())?
            } else {
                Vec::new()
            };
            let mut wrote_prefix_output = false;
            if show_raw {
                for entry in &entries {
                    write_diff_raw_entry(&mut stdout, entry, false, false, raw_abbrev, format)?;
                }
                wrote_prefix_output = !entries.is_empty();
            }
            if show_numstat {
                for entry in &stat_entries {
                    write_diff_numstat_materialized_entry(
                        &mut stdout,
                        entry.entry,
                        entry.stats,
                        false,
                    )?;
                }
                wrote_prefix_output = !entries.is_empty();
            }
            if show_stat || compact_summary {
                write_diff_stat_materialized(
                    &mut stdout,
                    &stat_entries,
                    DiffStatOptions {
                        compact_summary,
                        stat_count: None,
                        color: false,
                        quote_path_fully: true,
                    },
                    Some(&config),
                )?;
                wrote_prefix_output |= !entries.is_empty();
            }
            if show_shortstat {
                write_diff_shortstat_materialized(&mut stdout, &stat_entries)?;
                wrote_prefix_output |= !entries.is_empty();
            }
            if show_summary {
                for entry in &entries {
                    write_diff_summary_entry(&mut stdout, entry)?;
                }
                wrote_prefix_output |= entries.iter().any(stash_show_summary_outputs_entry);
            }
            if show_patch {
                if wrote_prefix_output {
                    writeln!(stdout)?;
                }
                for entry in &entries {
                    let options = DiffRenderOptions {
                        line_indicators: sley_diff_merge::render::LineIndicators::default(),
                        suppress_blank_empty: false,
                        binary: false,
                        anchors: &[],
                        allow_textconv: false,
                        db: &db,
                        lazy_fetch: cli_session.lazy_fetch(),
                        worktree_root: None,
                        use_worktree_new: false,
                        format,
                        abbrev: patch_abbrev,
                        src_prefix: "a/",
                        dst_prefix: "b/",
                        context: 3,
                        userdiff: None,
                        funcname: None,
                        colors: None,
                        word_diff: None,
                        no_index_contents: None,
                        submodule_format: sley_rev::diff_options::SubmoduleDiffFormat::Short,
                        submodule_dirt: None,
                        ws_error: None,
                        color_moved: None,
                        interhunk: 0,
                        ws_ignore: sley_diff_merge::WsIgnore::default(),
                        diff_algorithm: sley_diff_merge::DiffAlgorithm::Myers,
                        ignore_blank_lines: false,
                        ignore_regexes: &[],
                        line_ranges: None,
                        indent_heuristic: true,
                    };
                    write_diff_patch_entry(&mut stdout, entry, options)?;
                }
            }
        }
        StashShowMode::NameOnly => {
            for entry in entries {
                writeln!(stdout, "{}", status_quote_path(&entry.path, false))?;
            }
        }
        StashShowMode::NameStatus => {
            for entry in entries {
                write!(stdout, "{}", entry.status.label())?;
                if let Some(old_path) = &entry.old_path {
                    let old_path = status_quote_path(old_path, false);
                    write!(stdout, "\t{old_path}")?;
                }
                let path = status_quote_path(&entry.path, false);
                writeln!(stdout, "\t{path}")?;
            }
        }
        StashShowMode::NoPatch => {}
    }
    if exit_code && has_entries {
        return Err(GitError::Exit(1));
    }
    Ok(())
}

fn stash_show_summary_outputs_entry(entry: &sley_diff_merge::NameStatusEntry) -> bool {
    match entry.status {
        sley_diff_merge::NameStatus::Added
        | sley_diff_merge::NameStatus::Deleted
        | sley_diff_merge::NameStatus::Renamed(_)
        | sley_diff_merge::NameStatus::Copied(_) => true,
        sley_diff_merge::NameStatus::Modified | sley_diff_merge::NameStatus::TypeChanged => {
            entry.old_mode != entry.new_mode
        }
        sley_diff_merge::NameStatus::Unmerged => false,
    }
}

fn stash_show_should_enable_default_patch(mode: StashShowMode, visual_modes: [bool; 7]) -> bool {
    matches!(mode, StashShowMode::Stat) && !visual_modes.into_iter().any(|enabled| enabled)
}

fn stash_show_usage() {
    eprintln!(
        "usage: git stash show [-u | --include-untracked | --only-untracked] [<diff-options>] [<stash>]"
    );
    eprintln!();
    eprintln!("    -u, --[no-]include-untracked");
    eprintln!("                          include untracked files in the stash");
    eprintln!("    --only-untracked      only show untracked files in the stash");
    eprintln!();
}

fn cmd_stash_list(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
    let options = setup_stash_list_options(args)?;
    let git_dir = cli_session.git_dir()?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let format = repository_object_format(&common_git_dir)?;
    let store = FileRefStore::new(&common_git_dir, format);
    stash_list_warn_invalid_note_refs(&store, &options.note_refs);
    let db = FileObjectDatabase::new(repository_objects_dir(&common_git_dir), format);
    let mut entries = store.read_reflog("refs/stash")?;
    entries.reverse();
    let mut entries = entries.into_iter().enumerate().collect::<Vec<_>>();
    entries.retain(|(_, entry)| stash_list_grep_filters_match(entry, &options));
    let mut parent_filtered_entries = Vec::new();
    for (stash_index, entry) in entries {
        if stash_list_commit_filters_match(&db, format, &entry, &options)? {
            parent_filtered_entries.push((stash_index, entry));
        }
    }
    let entries = parent_filtered_entries;
    let skipped = options.skip_count.min(entries.len());
    let selected = options
        .max_count
        .map_or(entries.len() - skipped, |max_count| {
            max_count.min(entries.len() - skipped)
        });
    for (position, (stash_index, entry)) in entries.iter().skip(skipped).take(selected).enumerate()
    {
        let mut listed_commit = None;
        match &options.format {
            StashListFormat::Default => {
                println!(
                    "stash@{{{stash_index}}}: {}",
                    String::from_utf8_lossy(&entry.message)
                );
            }
            StashListFormat::Oneline => {
                println!(
                    "{} refs/stash@{{{stash_index}}}: {}",
                    format_log_oid(&entry.new_oid, options.abbrev_len),
                    String::from_utf8_lossy(&entry.message)
                );
            }
            StashListFormat::Custom {
                compiled,
                final_newline,
            } => {
                let object = db.read_object(&entry.new_oid)?;
                let commit = Commit::parse(format, &object.body)?;
                listed_commit = Some(commit.clone());
                print_stash_compiled_format(
                    entry,
                    *stash_index,
                    &commit,
                    compiled,
                    options.abbrev_len,
                    &options.date_mode,
                    options.date_explicit,
                )?;
                if *final_newline || position + 1 < selected {
                    println!();
                }
            }
        }
        if options.show_patch {
            println!();
            let commit = match listed_commit {
                Some(commit) => commit,
                None => {
                    let object = db.read_object(&entry.new_oid)?;
                    Commit::parse(format, &object.body)?
                }
            };
            if options.combined_patch {
                write_stash_list_combined_patch(
                    &mut io::stdout(),
                    &db,
                    format,
                    &commit,
                    cli_session.lazy_fetch(),
                )?;
            } else {
                write_stash_list_patch(
                    &mut io::stdout(),
                    &common_git_dir,
                    &db,
                    format,
                    &commit,
                    cli_session.lazy_fetch(),
                )?;
            }
        }
    }
    Ok(())
}

fn stash_list_option_takes_no_value_error(option: &str) -> Result<()> {
    eprintln!("error: option `{option}' takes no value");
    Err(GitError::Exit(1))
}

fn stash_list_note_ref(value: &str) -> String {
    if value.starts_with("refs/notes/") {
        value.to_string()
    } else {
        format!("refs/notes/{value}")
    }
}

fn stash_list_warn_invalid_note_refs(store: &FileRefStore, note_refs: &[String]) {
    for note_ref in note_refs {
        if !matches!(store.read_ref(note_ref), Ok(Some(_))) {
            eprintln!("warning: notes ref {note_ref} is invalid");
        }
    }
}

fn stash_list_diff_option_with_value(value: &str) -> Option<&'static str> {
    const OPTIONS: &[&str] = &[
        "minimal",
        "patience",
        "histogram",
        "indent-heuristic",
        "no-indent-heuristic",
        "ignore-space-at-eol",
        "ignore-cr-at-eol",
        "ignore-space-change",
        "ignore-all-space",
        "ignore-blank-lines",
        "function-context",
        "no-prefix",
        "default-prefix",
        "ita-visible-in-index",
        "ita-invisible-in-index",
        "pickaxe-all",
        "pickaxe-regex",
    ];
    let value = value.strip_prefix("--")?;
    OPTIONS.iter().copied().find(|option| {
        value
            .strip_prefix(option)
            .is_some_and(|suffix| suffix.starts_with('='))
    })
}

fn stash_list_fatal_unrecognized_argument(value: &str) -> Result<()> {
    eprintln!("fatal: unrecognized argument: {value}");
    Err(GitError::Exit(1))
}

fn stash_list_no_walk_invalid_argument(value: &str) -> Result<()> {
    eprintln!("error: invalid argument to --no-walk");
    stash_list_fatal_unrecognized_argument(value)
}

fn stash_list_validate_non_negative_integer(value: &str) -> Result<()> {
    value.parse::<usize>().map(|_| ()).map_err(|_| {
        eprintln!("fatal: '{value}': not a non-negative integer");
        GitError::Exit(1)
    })
}

fn stash_list_validate_color(value: &str) -> Result<()> {
    log_validate_color(value).map_err(|err| match err {
        GitError::Exit(129) => GitError::Exit(1),
        err => err,
    })
}

fn stash_list_validate_color_moved(value: &str) -> Result<()> {
    log_validate_color_moved(value).map_err(|err| match err {
        GitError::Exit(129) => GitError::Exit(1),
        err => err,
    })
}

fn stash_list_validate_color_moved_ws(value: &str) -> Result<()> {
    log_validate_color_moved_ws(value).map_err(|err| match err {
        GitError::Exit(129) => GitError::Exit(1),
        err => err,
    })
}

fn stash_list_validate_diff_merges(value: &str) -> Result<()> {
    log_validate_diff_merges(value).map_err(|err| match err {
        GitError::Exit(128) => GitError::Exit(1),
        err => err,
    })
}

fn stash_list_validate_output_indicator(option: &str, value: &str) -> Result<()> {
    log_validate_output_indicator(option, value).map_err(|err| match err {
        GitError::Exit(129) => GitError::Exit(1),
        err => err,
    })
}

fn stash_list_validate_ws_error_highlight(value: &str) -> Result<()> {
    log_validate_ws_error_highlight(value).map_err(|err| match err {
        GitError::Exit(129) => GitError::Exit(1),
        err => err,
    })
}

fn stash_list_validate_submodule_format(value: &str) -> Result<()> {
    log_validate_submodule_format(value).map_err(|err| match err {
        GitError::Exit(129) => GitError::Exit(1),
        err => err,
    })
}

fn stash_list_validate_ignore_submodules(value: &str) -> Result<()> {
    log_validate_ignore_submodules(value).map_err(|err| match err {
        GitError::Exit(128) => GitError::Exit(1),
        err => err,
    })
}

fn stash_list_validate_similarity_option(value: &str, option: &str) -> Result<()> {
    log_validate_similarity_option(value, option).map_err(|err| match err {
        GitError::Exit(129) => GitError::Exit(1),
        err => err,
    })
}

fn stash_list_validate_break_rewrites_option(value: &str) -> Result<()> {
    log_validate_break_rewrites_option(value).map_err(|err| match err {
        GitError::Exit(129) => GitError::Exit(1),
        err => err,
    })
}

fn parse_stash_list_abbrev(value: &str) -> Option<usize> {
    match value.parse::<isize>() {
        Ok(width) if width < 0 => None,
        Ok(width) => Some((width as usize).max(4)),
        Err(_) => Some(4),
    }
}

fn parse_stash_list_age(value: &str) -> Result<i64> {
    log_parse_age(value).map_err(|err| match err {
        GitError::Exit(128) => GitError::Exit(1),
        err => err,
    })
}

fn parse_stash_list_min_age(value: &str) -> Result<i64> {
    let age = parse_stash_list_age(value)?;
    if age < 0 { Ok(i64::MAX) } else { Ok(age) }
}

fn stash_list_date_mode(value: &str) -> Result<DateMode> {
    log_date_mode(value).map_err(|err| match err {
        GitError::Exit(128) => GitError::Exit(1),
        err => err,
    })
}

fn parse_stash_list_date_cutoff(value: &str) -> Result<i64> {
    log_parse_date_cutoff(value).map_err(|err| match err {
        GitError::Exit(128) => GitError::Exit(1),
        err => err,
    })
}

fn parse_stash_list_filter_patterns(
    patterns: &[LogFilterPattern],
    mode: SimpleLogRegexMode,
) -> Result<Vec<SimpleLogRegex>> {
    parse_log_filter_patterns_with_diagnostic_verbosity(
        patterns,
        mode,
        sley_grep::RegexDiagnosticVerbosity::platform_default(),
    )
    .map_err(|err| match err {
        GitError::Exit(128) => GitError::Exit(1),
        err => err,
    })
}

fn parse_stash_list_format(value: &str) -> Result<StashListFormat> {
    match value {
        "oneline" => Ok(StashListFormat::Oneline),
        value if let Some(format) = value.strip_prefix("format:") => Ok(StashListFormat::Custom {
            compiled: CompiledLogFormat::compile(format, LogFormatDialect::Stash)?,
            final_newline: false,
        }),
        value => Ok(StashListFormat::Custom {
            compiled: CompiledLogFormat::compile(value, LogFormatDialect::Stash)?,
            final_newline: true,
        }),
    }
}

fn stash_list_grep_filters_match(entry: &ReflogEntry, options: &StashListOptions) -> bool {
    let message = String::from_utf8_lossy(&entry.message);
    if !options.reflog_filters.is_empty()
        && !options
            .reflog_filters
            .iter()
            .any(|filter| filter.is_match(&message, options.regexp_ignore_case))
    {
        return false;
    }
    if options.grep_filters.is_empty() {
        return true;
    }
    let grep_matched = if options.grep_all_match {
        options
            .grep_filters
            .iter()
            .all(|filter| filter.is_match(&message, options.regexp_ignore_case))
    } else {
        options
            .grep_filters
            .iter()
            .any(|filter| filter.is_match(&message, options.regexp_ignore_case))
    };
    grep_matched != options.invert_grep
}

fn stash_list_commit_filters_match(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    entry: &ReflogEntry,
    options: &StashListOptions,
) -> Result<bool> {
    if options.min_parents.is_none()
        && options.max_parents.is_none()
        && options.max_age.is_none()
        && options.min_age.is_none()
        && options.author_filters.is_empty()
        && options.committer_filters.is_empty()
    {
        return Ok(true);
    }
    let object = db.read_object(&entry.new_oid)?;
    let commit = Commit::parse(format, &object.body)?;
    Ok(stash_list_identity_filters_match(
        &commit.author,
        &options.author_filters,
        options.regexp_ignore_case,
    ) && stash_list_identity_filters_match(
        &commit.committer,
        &options.committer_filters,
        options.regexp_ignore_case,
    ) && stash_list_age_filters_match(&commit, options)?
        && options
            .min_parents
            .is_none_or(|min| commit.parents.len() >= min)
        && options
            .max_parents
            .is_none_or(|max| commit.parents.len() <= max))
}

fn stash_list_identity_filters_match(
    identity: &[u8],
    filters: &[SimpleLogRegex],
    ignore_case: bool,
) -> bool {
    if filters.is_empty() {
        return true;
    }
    let identity = String::from_utf8_lossy(identity);
    filters
        .iter()
        .any(|filter| filter.is_match(&identity, ignore_case))
}

fn stash_list_age_filters_match(commit: &Commit, options: &StashListOptions) -> Result<bool> {
    if options.max_age.is_none() && options.min_age.is_none() {
        return Ok(true);
    }
    let timestamp = commit_identity_timestamp_i64(&commit.committer)?;
    Ok(options.max_age.is_none_or(|age| timestamp >= age)
        && options.min_age.is_none_or(|age| timestamp <= age))
}

fn write_stash_list_patch(
    stdout: &mut impl Write,
    common_git_dir: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    commit: &Commit,
    lazy_fetch: bool,
) -> Result<()> {
    let Some(base_oid) = commit.parents.first() else {
        return Ok(());
    };
    let base = Commit::parse(format, &db.read_object(base_oid)?.body)?;
    let entries = sley_diff_merge::diff_name_status_trees_with_options(
        db,
        format,
        &base.tree,
        &commit.tree,
        sley_diff_merge::DiffNameStatusOptions::default(),
    )?;
    let abbrev = repository_abbrev(common_git_dir, format)?
        .unwrap_or(7)
        .min(format.hex_len());
    for entry in &entries {
        let options = DiffRenderOptions {
            line_indicators: sley_diff_merge::render::LineIndicators::default(),
            suppress_blank_empty: false,
            binary: false,
            anchors: &[],
            allow_textconv: false,
            db,
            lazy_fetch,
            worktree_root: None,
            use_worktree_new: false,
            format,
            abbrev,
            src_prefix: "a/",
            dst_prefix: "b/",
            context: 3,
            userdiff: None,
            funcname: None,
            colors: None,
            word_diff: None,
            no_index_contents: None,
            submodule_format: sley_rev::diff_options::SubmoduleDiffFormat::Short,
            submodule_dirt: None,
            ws_error: None,
            color_moved: None,
            interhunk: 0,
            ws_ignore: sley_diff_merge::WsIgnore::default(),
            diff_algorithm: sley_diff_merge::DiffAlgorithm::Myers,
            ignore_blank_lines: false,
            ignore_regexes: &[],
            line_ranges: None,
            indent_heuristic: true,
        };
        write_diff_patch_entry(stdout, entry, options)?;
    }
    Ok(())
}

fn write_stash_list_combined_patch(
    stdout: &mut impl Write,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    commit: &Commit,
    lazy_fetch: bool,
) -> Result<()> {
    if commit.parents.len() < 2 {
        return Ok(());
    }
    let base = Commit::parse(format, &db.read_object(&commit.parents[0])?.body)?;
    let index = Commit::parse(format, &db.read_object(&commit.parents[1])?.body)?;
    let base_map = sley_diff_merge::flatten_tree(db, format, &base.tree)?;
    let index_map = sley_diff_merge::flatten_tree(db, format, &index.tree)?;
    let result_map = sley_diff_merge::flatten_tree(db, format, &commit.tree)?;
    let mut paths = BTreeSet::new();
    paths.extend(base_map.keys().cloned());
    paths.extend(index_map.keys().cloned());
    paths.extend(result_map.keys().cloned());
    for path in paths {
        let Some((result_mode, result_oid)) = result_map.get(&path) else {
            continue;
        };
        let base_entry = base_map.get(&path);
        let index_entry = index_map.get(&path);
        if base_entry == Some(&(*result_mode, *result_oid))
            && index_entry == Some(&(*result_mode, *result_oid))
        {
            continue;
        }
        let Some((_, base_oid)) = base_entry else {
            continue;
        };
        let Some((_, index_oid)) = index_entry else {
            continue;
        };
        let base_content = merge_read_blob(db, base_oid, lazy_fetch)?;
        let index_content = merge_read_blob(db, index_oid, lazy_fetch)?;
        let result_content = merge_read_blob(db, result_oid, lazy_fetch)?;
        let mut body = Vec::new();
        if !sley_diff_merge::render::render_combined(
            &mut body,
            &result_content,
            &[base_content.as_slice(), index_content.as_slice()],
        ) {
            continue;
        }
        let path_display = String::from_utf8_lossy(&path);
        writeln!(stdout, "diff --cc {path_display}")?;
        writeln!(
            stdout,
            "index {},{}..{}",
            format_log_oid(base_oid, Some(7)),
            format_log_oid(index_oid, Some(7)),
            format_log_oid(result_oid, Some(7))
        )?;
        writeln!(stdout, "--- a/{path_display}")?;
        writeln!(stdout, "+++ b/{path_display}")?;
        stdout.write_all(&body)?;
    }
    Ok(())
}

fn cmd_stash_export(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
    let mut print = false;
    let mut to_ref: Option<String> = None;
    let mut specs = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        match arg.as_str() {
            "--print" => print = true,
            "--to-ref" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    eprintln!("error: option `to-ref' requires a value");
                    return Err(GitError::Exit(129));
                };
                to_ref = Some(value.clone());
            }
            value if let Some(value) = value.strip_prefix("--to-ref=") => {
                to_ref = Some(value.to_string());
            }
            value if value.starts_with('-') => {
                eprintln!("error: unknown option `{}`", value.trim_start_matches('-'));
                eprintln!("usage: git stash export (--print | --to-ref <ref>) [<stash>...]");
                return Err(GitError::Exit(129));
            }
            value => specs.push(value.to_string()),
        }
        index += 1;
    }
    if print == to_ref.is_some() {
        eprintln!("error: exactly one of --print and --to-ref is required");
        return Err(GitError::Exit(1));
    }

    let git_dir = cli_session.git_dir()?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let format = repository_object_format(&common_git_dir)?;
    let mut db = FileObjectDatabase::from_git_dir(&common_git_dir, format);
    let store = FileRefStore::new(&common_git_dir, format);
    let mut stash_oids = Vec::new();
    if specs.is_empty() {
        let entries = store.read_reflog("refs/stash")?;
        stash_oids.extend(entries.iter().map(|entry| entry.new_oid));
    } else {
        for spec in &specs {
            let oid = match resolve_stash_argument(
                &common_git_dir,
                format,
                &store,
                &db,
                Some(spec.as_str()),
                cli_session.replace_objects(),
            ) {
                Ok(oid) => oid,
                Err(_) => {
                    eprintln!("error: unable to find stash entry {spec}");
                    return Err(GitError::Exit(1));
                }
            };
            stash_oids.push(oid);
        }
        stash_oids.reverse();
    }
    for oid in &stash_oids {
        validate_stash_like_commit(&db, format, oid)?;
    }

    let exported = write_stash_export_chain(&mut db, format, &stash_oids)?;
    if let Some(refname) = to_ref {
        let old_oid = match store.read_ref(&refname)? {
            Some(RefTarget::Direct(oid)) => oid,
            Some(RefTarget::Symbolic(_)) | None => zero_oid(format)?,
        };
        let mut tx = store.transaction();
        tx.update(RefUpdate {
            name: refname,
            expected: None,
            new: RefTarget::Direct(exported),
            reflog: Some(ReflogEntry {
                old_oid,
                new_oid: exported,
                committer: stash_export_identity(),
                message: b"stash export".to_vec(),
            }),
        });
        tx.commit()?;
    } else {
        println!("{exported}");
    }
    Ok(())
}

fn write_stash_export_chain(
    db: &mut FileObjectDatabase,
    format: ObjectFormat,
    stash_oids: &[ObjectId],
) -> Result<ObjectId> {
    let empty_tree = db.write_object(EncodedObject::new(
        ObjectType::Tree,
        Tree {
            entries: Vec::new(),
        }
        .write(),
    ))?;
    let ident = stash_export_identity();
    let mut previous = sley_sequencer::create_commit(
        db,
        sley_sequencer::CommitCreate {
            tree: empty_tree,
            parents: Vec::new(),
            author: ident.clone(),
            committer: ident.clone(),
            message: Vec::new(),
            encoding: None,
            signature: None,
        },
    )?;
    for stash_oid in stash_oids {
        let object = db.read_object(stash_oid)?;
        let stash = Commit::parse(format, &object.body)?;
        let mut message = b"git stash: ".to_vec();
        message.extend_from_slice(&stash.message);
        if !message.ends_with(b"\n") {
            message.push(b'\n');
        }
        previous = sley_sequencer::create_commit(
            db,
            sley_sequencer::CommitCreate {
                tree: empty_tree,
                parents: vec![previous, *stash_oid],
                author: stash.author,
                committer: stash.committer,
                message,
                encoding: stash.encoding,
                signature: None,
            },
        )?;
    }
    Ok(previous)
}

fn cmd_stash_import(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
    if args.len() != 1 || args.first().is_some_and(|arg| arg.starts_with('-')) {
        eprintln!("usage: git stash import <commit>");
        return Err(GitError::Exit(129));
    }
    let git_dir = cli_session.git_dir()?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let format = repository_object_format(&common_git_dir)?;
    let db = FileObjectDatabase::from_git_dir(&common_git_dir, format);
    let chain = match resolve_revision(
        &common_git_dir,
        format,
        &args[0],
        cli_session.replace_objects(),
    ) {
        Ok(oid) => oid,
        Err(_) => {
            eprintln!("error: not a valid revision: {}", args[0]);
            return Err(GitError::Exit(1));
        }
    };
    let stashes = read_stash_export_chain(&db, format, &chain)?;
    for stash_oid in stashes {
        let object = db.read_object(&stash_oid)?;
        let stash = Commit::parse(format, &object.body)?;
        let message = String::from_utf8_lossy(&stash.message);
        store_stash_commit(cli_session, &stash_oid, message.trim_end_matches('\n'))?;
    }
    Ok(())
}

fn read_stash_export_chain(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    chain: &ObjectId,
) -> Result<Vec<ObjectId>> {
    let empty_tree = empty_tree_oid(db, format)?;
    let mut current = *chain;
    let mut stashes = Vec::new();
    loop {
        let object = db.read_object(&current)?;
        if object.object_type != ObjectType::Commit {
            eprintln!("error: not a commit: {current}");
            return Err(GitError::Exit(1));
        }
        let commit = Commit::parse(format, &object.body)?;
        if commit.tree != empty_tree || (commit.parents.len() != 2 && !commit.parents.is_empty()) {
            eprintln!("error: {current} is not a valid exported stash commit");
            return Err(GitError::Exit(1));
        }
        if commit.parents.is_empty() {
            if commit.author != stash_export_identity()
                || commit.committer != stash_export_identity()
            {
                eprintln!("error: found root commit {current} with invalid data");
                return Err(GitError::Exit(1));
            }
            break;
        }
        if !commit.message.starts_with(b"git stash: ") {
            eprintln!("error: found stash commit {current} without expected prefix");
            return Err(GitError::Exit(1));
        }
        let stash_oid = commit.parents[1];
        validate_stash_like_commit(db, format, &stash_oid)?;
        stashes.push(stash_oid);
        current = commit.parents[0];
    }
    stashes.reverse();
    Ok(stashes)
}

fn empty_tree_oid(db: &FileObjectDatabase, format: ObjectFormat) -> Result<ObjectId> {
    sley_core::object_id_for_bytes(format, "tree", &[]).and_then(|oid| {
        if db.read_object(&oid).is_ok() {
            Ok(oid)
        } else {
            Ok(oid)
        }
    })
}

fn stash_export_identity() -> Vec<u8> {
    b"git stash <git@stash> 1000684800 +0000".to_vec()
}

// ===== rebase autostash bridge =====

/// `git stash create autostash` for the rebase autostash machinery: returns
/// the stash commit oid without touching `refs/stash`, `None` when the tree
/// is clean.
pub(crate) fn create_stash_for_autostash(
    cli_session: &crate::session::CliSession,
) -> Result<Option<ObjectId>> {
    let git_dir = cli_session.git_dir()?;
    create_stash_for_autostash_at(&git_dir, cli_session.cwd())
}

pub(crate) fn create_stash_for_autostash_at(
    git_dir: &Path,
    cwd: &Path,
) -> Result<Option<ObjectId>> {
    Ok(create_stash_commit_at(
        git_dir,
        cwd,
        cwd,
        &["autostash".to_string()],
        false,
        false,
        StashCreateMode::Worktree,
        &[],
        false,
        sley_worktree::PathspecMatchMagic::default(),
    )?
    .map(|created| created.oid))
}

/// `git stash store -m <message> -q <oid>` equivalent.
pub(crate) fn store_stash_commit(
    cli_session: &crate::session::CliSession,
    stash_oid: &ObjectId,
    message: &str,
) -> Result<()> {
    let git_dir = cli_session.git_dir()?;
    store_stash_commit_at(&git_dir, stash_oid, message)
}

pub(crate) fn store_stash_commit_at(
    git_dir: &Path,
    stash_oid: &ObjectId,
    message: &str,
) -> Result<()> {
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let format = repository_object_format(&common_git_dir)?;
    let db = FileObjectDatabase::from_git_dir(&common_git_dir, format);
    validate_stash_like_commit(&db, format, stash_oid)?;
    let store = FileRefStore::new(&common_git_dir, format);
    commit_stash_store_plan(
        &store,
        *stash_oid,
        default_committer(),
        message.as_bytes().to_vec(),
    )?;
    Ok(())
}

/// `git stash apply <oid>` with all output suppressed; `Ok(false)` when the
/// stash cannot be applied cleanly (the caller stores it instead).
pub(crate) fn apply_stash_commit_quietly(
    cli_session: &crate::session::CliSession,
    stash_oid: &ObjectId,
) -> Result<bool> {
    let git_dir = cli_session.git_dir()?;
    let worktree_root = worktree_root_for_git_dir(cli_session, &git_dir)?;
    apply_stash_commit_quietly_at(
        &git_dir,
        &worktree_root,
        stash_oid,
        cli_session.lazy_fetch(),
    )
}

pub(crate) fn apply_stash_commit_quietly_at(
    git_dir: &Path,
    worktree_root: &Path,
    stash_oid: &ObjectId,
    lazy_fetch: bool,
) -> Result<bool> {
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let format = repository_object_format(&common_git_dir)?;
    let config = read_repo_config(&common_git_dir)?;
    let db = FileObjectDatabase::from_git_dir(&common_git_dir, format);
    let Ok(stash_object) = db.read_object(stash_oid) else {
        return Ok(false);
    };
    if stash_object.object_type != ObjectType::Commit {
        return Ok(false);
    }
    let Ok(stash_commit) = Commit::parse(format, &stash_object.body) else {
        return Ok(false);
    };
    let head_store = FileRefStore::new(&git_dir, format);
    let Some((head_oid, head_commit)) = stash_head_commit(&head_store, &db, format)? else {
        return Ok(false);
    };
    let Some(base_oid) = stash_commit.parents.first() else {
        return Ok(false);
    };
    let Some(index_oid) = stash_commit.parents.get(1) else {
        return Ok(false);
    };
    let Ok(base_object) = db.read_object(base_oid) else {
        return Ok(false);
    };
    let Ok(base_commit) = Commit::parse(format, &base_object.body) else {
        return Ok(false);
    };
    let Ok(index_object) = db.read_object(index_oid) else {
        return Ok(false);
    };
    let Ok(index_commit) = Commit::parse(format, &index_object.body) else {
        return Ok(false);
    };
    let _ = (&head_oid, &head_commit);
    let stash_state = StashApplyState {
        worktree_root: &worktree_root,
        git_dir: &git_dir,
        base_tree: &base_commit.tree,
        stash_tree: &stash_commit.tree,
        index_tree: &index_commit.tree,
    };
    // Autostash apply must be clean; on conflict (or any error) the caller keeps
    // the stash entry and lets the user recover it manually.
    let labels = StashApplyMergeLabels {
        ours: "Updated upstream",
        theirs: "Stashed changes",
        base: "Stash base",
        style: stash_apply_conflict_style(&git_dir),
    };
    match apply_stash_via_merge(
        &db,
        format,
        &stash_state,
        false,
        &labels,
        &config,
        lazy_fetch,
    ) {
        Ok(StashApplyOutcome::Clean) => {}
        Ok(StashApplyOutcome::Conflict) => return Ok(false),
        Err(_) => return Ok(false),
    }
    if let Some(untracked_oid) = stash_commit.parents.get(2) {
        let Ok(untracked_object) = db.read_object(untracked_oid) else {
            return Ok(false);
        };
        let Ok(untracked_commit) = Commit::parse(format, &untracked_object.body) else {
            return Ok(false);
        };
        restore_stash_tree_to_worktree(&worktree_root, &db, format, &untracked_commit.tree)?;
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sel(spec: &str) -> Option<usize> {
        parse_stash_drop_selector(spec).ok()
    }

    #[test]
    fn parses_stash_at_brace_forms() {
        assert_eq!(sel("stash@{0}"), Some(0));
        assert_eq!(sel("stash@{3}"), Some(3));
        assert_eq!(sel("refs/stash@{2}"), Some(2));
    }

    #[test]
    fn parses_bare_numeric_shorthand() {
        // `git stash drop 1` — a bare integer is `stash@{1}`.
        assert_eq!(sel("0"), Some(0));
        assert_eq!(sel("1"), Some(1));
        assert_eq!(sel("42"), Some(42));
    }

    #[test]
    fn rejects_non_numeric_and_malformed_selectors() {
        assert_eq!(sel("HEAD"), None);
        assert_eq!(sel("stash@{x}"), None);
        assert_eq!(sel("stash@{1"), None);
        assert_eq!(sel(""), None);
        assert_eq!(sel("-1"), None);
        assert_eq!(sel("1a"), None);
    }

    fn assumed_ok(args: &[&str]) -> bool {
        let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        stash_reject_assumed_push_token(&owned).is_ok()
    }

    #[test]
    fn assumed_push_allows_pure_option_invocations() {
        // `git stash -k`, `git stash --staged`, `git stash -q -u` — all options,
        // no bare positional, so push is assumable.
        assert!(assumed_ok(&["-k"]));
        assert!(assumed_ok(&["--staged"]));
        assert!(assumed_ok(&["-q", "-u"]));
        assert!(assumed_ok(&["--keep-index", "--message", "wip"]));
    }

    #[test]
    fn assumed_push_allows_pathspecs_after_dashdash() {
        assert!(assumed_ok(&["--", "file.txt"]));
        assert!(assumed_ok(&["-q", "--", "a", "b"]));
    }

    #[test]
    fn assumed_push_rejects_bare_positional_token() {
        // `git stash -q drop` must NOT silently stash a pathspec named `drop`.
        assert!(!assumed_ok(&["-q", "drop"]));
        assert!(!assumed_ok(&["drop"]));
        assert!(!assumed_ok(&["file.txt"]));
    }

    #[test]
    fn assumed_push_patch_forces_assume() {
        // `--patch` removes the positional ambiguity, so a following token is ok.
        assert!(assumed_ok(&["-p", "anything"]));
        assert!(assumed_ok(&["--patch", "file"]));
    }
}
