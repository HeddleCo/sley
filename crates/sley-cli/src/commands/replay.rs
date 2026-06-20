//! `git cherry-pick` / `git revert` — porcelains over the sequencer state
//! machine in [`sley_sequencer::replay`].
//!
//! The state machine (todo/opts/head/abort-safety files, CHERRY_PICK_HEAD /
//! REVERT_HEAD lifecycle) lives in the library crate; this module owns the
//! drive loop: option parsing, the rev walk that populates the instruction
//! sheet, the per-commit 3-way replay, and `--continue` / `--abort` /
//! `--skip` / `--quit`.

use crate::commands::merge_rebase::{
    MergePathResult, MergeTreeMap, commit_tree_oid, head_commit_oid, three_way_merge_trees_styled,
};
use crate::*;
use sley_sequencer::replay::{self, ReplayAction, ReplayOpts, TodoItem};

const CHERRY_PICK_USAGE: &str = "\
usage: git cherry-pick [--edit] [-n] [-m <parent-number>] [-s] [-x] [--ff]
                       [-S[<keyid>]] <commit>...
   or: git cherry-pick (--continue | --skip | --abort | --quit)

    --quit                end revert or cherry-pick sequence
    --continue            resume revert or cherry-pick sequence
    --abort               cancel revert or cherry-pick sequence
    --skip                skip current commit and continue
    --[no-]cleanup <mode> how to strip spaces and #comments from message
    -n, --no-commit       don't automatically commit
    --commit              opposite of --no-commit
    -e, --[no-]edit       edit the commit message
    -s, --[no-]signoff    add a Signed-off-by trailer
    -m, --[no-]mainline <parent-number>
                          select mainline parent
    --[no-]rerere-autoupdate
                          update the index with reused conflict resolution if possible
    --[no-]strategy <strategy>
                          merge strategy
    -X, --[no-]strategy-option <option>
                          option for merge strategy
    -S, --[no-]gpg-sign[=<key-id>]
                          GPG sign commit
    -x                    append commit name
    --[no-]ff             allow fast-forward
    --[no-]allow-empty    preserve initially empty commits
    --[no-]allow-empty-message
                          allow commits with empty messages
    --[no-]keep-redundant-commits
                          deprecated: use --empty=keep instead
    --empty (stop|drop|keep)
                          how to handle commits that become empty
";

const REVERT_USAGE: &str = "\
usage: git revert [--[no-]edit] [-n] [-m <parent-number>] [-s] [-S[<keyid>]] <commit>...
   or: git revert (--continue | --skip | --abort | --quit)

    --quit                end revert or cherry-pick sequence
    --continue            resume revert or cherry-pick sequence
    --abort               cancel revert or cherry-pick sequence
    --skip                skip current commit and continue
    --[no-]cleanup <mode> how to strip spaces and #comments from message
    -n, --no-commit       don't automatically commit
    --commit              opposite of --no-commit
    -e, --[no-]edit       edit the commit message
    -s, --[no-]signoff    add a Signed-off-by trailer
    -m, --[no-]mainline <parent-number>
                          select mainline parent
    --[no-]rerere-autoupdate
                          update the index with reused conflict resolution if possible
    --[no-]strategy <strategy>
                          merge strategy
    -X, --[no-]strategy-option <option>
                          option for merge strategy
    -S, --[no-]gpg-sign[=<key-id>]
                          GPG sign commit
    --[no-]reference      use the 'reference' format to refer to commits
";

fn usage_text(action: ReplayAction) -> &'static str {
    match action {
        ReplayAction::Pick => CHERRY_PICK_USAGE,
        ReplayAction::Revert => REVERT_USAGE,
    }
}

pub(crate) fn cmd_cherry_pick(args: &[String]) -> Result<()> {
    run_replay(ReplayAction::Pick, args)
}

pub(crate) fn cmd_revert(args: &[String]) -> Result<()> {
    run_replay(ReplayAction::Revert, args)
}

pub(crate) fn cmd_replay(args: &[String]) -> Result<()> {
    run_git_replay(args)
}

const REPLAY_USAGE: &str = "\
usage: (EXPERIMENTAL!) git replay ([--contained] --onto=<newbase> | --advance=<branch> | --revert=<branch>)
       [--ref=<ref>] [--ref-action=<mode>] <revision-range>

    --[no-]contained      update all branches that point at commits in <revision-range>
    --onto <revision>     replay onto given commit
    --advance <branch>    make replay advance given branch
    --revert <branch>     revert commits onto given branch
    --ref <branch>        reference to update with result
    --ref-action <mode>   control ref update behavior (update|print)
";

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReplayRefAction {
    Update,
    Print,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReplayModeKind {
    Onto,
    Advance,
    Revert,
}

struct GitReplayArgs {
    onto: Option<String>,
    advance: Option<String>,
    revert: Option<String>,
    contained: bool,
    ref_name: Option<String>,
    ref_action: Option<ReplayRefAction>,
    rev_args: Vec<String>,
}

struct GitReplayPlan {
    action: ReplayAction,
    base: ObjectId,
    target_ref: String,
    old_oid: Option<ObjectId>,
    commits: Vec<ObjectId>,
    ref_action: ReplayRefAction,
    reflog_message: Vec<u8>,
}

fn run_git_replay(args: &[String]) -> Result<()> {
    let parsed = parse_git_replay_args(args)?;
    if parsed.onto.is_some() && parsed.advance.is_some() {
        eprintln!("fatal: options '--onto' and '--advance' cannot be used together");
        return Err(GitError::Exit(128));
    }
    if parsed.revert.is_some() && parsed.onto.is_some() {
        eprintln!("fatal: options '--revert' and '--onto' cannot be used together");
        return Err(GitError::Exit(128));
    }
    if parsed.revert.is_some() && parsed.advance.is_some() {
        eprintln!("fatal: options '--revert' and '--advance' cannot be used together");
        return Err(GitError::Exit(128));
    }
    let modes = usize::from(parsed.onto.is_some())
        + usize::from(parsed.advance.is_some())
        + usize::from(parsed.revert.is_some());
    if modes != 1 {
        eprintln!("error: exactly one of --onto, --advance, or --revert is required");
        eprint!("{REPLAY_USAGE}");
        return Err(GitError::Exit(129));
    }
    if parsed.advance.is_some() && parsed.contained {
        eprintln!("fatal: options '--advance' and '--contained' cannot be used together");
        return Err(GitError::Exit(128));
    }
    if parsed.revert.is_some() && parsed.contained {
        eprintln!("fatal: options '--revert' and '--contained' cannot be used together");
        return Err(GitError::Exit(128));
    }
    if parsed.ref_name.is_some() && parsed.contained {
        eprintln!("fatal: options '--ref' and '--contained' cannot be used together");
        return Err(GitError::Exit(128));
    }
    if parsed.rev_args.is_empty() {
        eprintln!("error: empty commit set passed");
        return Err(GitError::Exit(128));
    }

    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let format = repository_object_format(&common_git_dir)?;
    let worktree_root =
        sley_worktree::worktree_root_for_git_dir(&git_dir)?.unwrap_or_else(|| cwd.clone());
    let ctx = ReplayCtx {
        action: if parsed.revert.is_some() {
            ReplayAction::Revert
        } else {
            ReplayAction::Pick
        },
        git_dir,
        common_git_dir,
        worktree_root,
        format,
    };
    let plan = build_git_replay_plan(&ctx, parsed)?;
    let new_oid = replay_commits_to_base(&ctx, &plan)?;
    emit_or_update_replay_ref(&ctx, &plan, &new_oid)
}

fn parse_git_replay_args(args: &[String]) -> Result<GitReplayArgs> {
    let mut parsed = GitReplayArgs {
        onto: None,
        advance: None,
        revert: None,
        contained: false,
        ref_name: None,
        ref_action: None,
        rev_args: Vec::new(),
    };
    let mut iter = args.iter();
    let mut positional_only = false;
    while let Some(arg) = iter.next() {
        if positional_only {
            parsed.rev_args.push(arg.clone());
            continue;
        }
        match arg.as_str() {
            "--" => positional_only = true,
            "-h" | "--help" => {
                print!("{REPLAY_USAGE}");
                return Err(GitError::Exit(129));
            }
            "--contained" => parsed.contained = true,
            "--no-contained" => parsed.contained = false,
            "--onto" => {
                parsed.onto = Some(
                    iter.next()
                        .ok_or_else(|| option_error("switch `onto' requires a value"))?
                        .clone(),
                );
            }
            value if value.starts_with("--onto=") => {
                parsed.onto = Some(value["--onto=".len()..].to_string());
            }
            "--advance" => {
                parsed.advance = Some(
                    iter.next()
                        .ok_or_else(|| option_error("switch `advance' requires a value"))?
                        .clone(),
                );
            }
            value if value.starts_with("--advance=") => {
                parsed.advance = Some(value["--advance=".len()..].to_string());
            }
            "--revert" => {
                parsed.revert = Some(
                    iter.next()
                        .ok_or_else(|| option_error("switch `revert' requires a value"))?
                        .clone(),
                );
            }
            value if value.starts_with("--revert=") => {
                parsed.revert = Some(value["--revert=".len()..].to_string());
            }
            "--ref" => {
                parsed.ref_name = Some(
                    iter.next()
                        .ok_or_else(|| option_error("switch `ref' requires a value"))?
                        .clone(),
                );
            }
            value if value.starts_with("--ref=") => {
                parsed.ref_name = Some(value["--ref=".len()..].to_string());
            }
            "--ref-action" => {
                let value = iter
                    .next()
                    .ok_or_else(|| option_error("switch `ref-action' requires a value"))?;
                parsed.ref_action = Some(parse_replay_ref_action(value)?);
            }
            value if value.starts_with("--ref-action=") => {
                parsed.ref_action = Some(parse_replay_ref_action(&value["--ref-action=".len()..])?);
            }
            value => parsed.rev_args.push(value.to_string()),
        }
    }
    Ok(parsed)
}

fn parse_replay_ref_action(value: &str) -> Result<ReplayRefAction> {
    match value {
        "update" => Ok(ReplayRefAction::Update),
        "print" => Ok(ReplayRefAction::Print),
        _ => {
            eprintln!("fatal: invalid value for --ref-action: {value}");
            Err(GitError::Exit(128))
        }
    }
}

fn build_git_replay_plan(ctx: &ReplayCtx, parsed: GitReplayArgs) -> Result<GitReplayPlan> {
    let refs = ctx.refs();
    let mode = if parsed.onto.is_some() {
        ReplayModeKind::Onto
    } else if parsed.advance.is_some() {
        ReplayModeKind::Advance
    } else {
        ReplayModeKind::Revert
    };
    if matches!(mode, ReplayModeKind::Advance | ReplayModeKind::Revert)
        && parsed.rev_args.len() != 1
    {
        let option = if mode == ReplayModeKind::Advance {
            "--advance"
        } else {
            "--revert"
        };
        eprintln!(
            "fatal: '{option}' cannot be used with multiple revision ranges because the ordering would be ill-defined"
        );
        return Err(GitError::Exit(128));
    }
    if parsed.ref_name.is_some() && parsed.rev_args.len() != 1 {
        eprintln!("fatal: --ref cannot be used with multiple revision ranges");
        return Err(GitError::Exit(128));
    }
    let ref_action = match parsed.ref_action {
        Some(action) => action,
        None => match config_value(&ctx.git_dir, "replay", "refAction").as_deref() {
            Some("print") => ReplayRefAction::Print,
            Some("update") | None => ReplayRefAction::Update,
            Some(value) => {
                eprintln!("fatal: invalid replay.refAction value: {value}");
                return Err(GitError::Exit(128));
            }
        },
    };
    let (action, base, default_target, old_oid, reflog_message) = match mode {
        ReplayModeKind::Onto => {
            let onto = parsed.onto.as_ref().expect("--onto mode has value");
            let base = resolve_revision(&ctx.git_dir, ctx.format, onto).map_err(|_| {
                eprintln!("fatal: '{onto}' is not a valid commit-ish for --onto");
                GitError::Exit(128)
            })?;
            let target = replay_target_from_revision(&refs, &parsed.rev_args)?;
            let old_oid = read_direct_ref(&refs, ctx.format, &target)?;
            (
                ReplayAction::Pick,
                base,
                target,
                old_oid,
                format!("replay --onto {base}").into_bytes(),
            )
        }
        ReplayModeKind::Advance => {
            let advance = parsed.advance.as_ref().expect("--advance mode has value");
            let target = replay_existing_ref(&refs, advance, "--advance")?;
            let old_oid = read_direct_ref(&refs, ctx.format, &target)?;
            let Some(base) = old_oid else {
                eprintln!("fatal: argument to --advance must be a reference");
                return Err(GitError::Exit(128));
            };
            (
                ReplayAction::Pick,
                base,
                target,
                old_oid,
                format!("replay --advance {advance}").into_bytes(),
            )
        }
        ReplayModeKind::Revert => {
            let revert = parsed.revert.as_ref().expect("--revert mode has value");
            let target = replay_existing_ref(&refs, revert, "--revert")?;
            let old_oid = read_direct_ref(&refs, ctx.format, &target)?;
            let Some(base) = old_oid else {
                eprintln!("fatal: argument to --revert must be a reference");
                return Err(GitError::Exit(128));
            };
            (
                ReplayAction::Revert,
                base,
                target,
                old_oid,
                format!("replay --revert {revert}").into_bytes(),
            )
        }
    };
    let explicit_ref = parsed.ref_name.is_some();
    let target_ref = match parsed.ref_name {
        Some(name) => validate_replay_ref(&name)?,
        None => default_target,
    };
    let old_oid = if explicit_ref {
        read_direct_ref(&refs, ctx.format, &target_ref)?
    } else {
        old_oid
    };
    let commits = select_git_replay_commits(ctx, action, &parsed.rev_args, &base)?;
    Ok(GitReplayPlan {
        action,
        base,
        target_ref,
        old_oid,
        commits,
        ref_action,
        reflog_message,
    })
}

fn replay_existing_ref(store: &FileRefStore, name: &str, option: &str) -> Result<String> {
    let candidates = if name == "HEAD" || name.starts_with("refs/") {
        vec![name.to_string()]
    } else {
        vec![branch_ref_name(name)?, name.to_string()]
    };
    for candidate in candidates {
        if store.read_ref(&candidate).ok().flatten().is_some() {
            return Ok(candidate);
        }
    }
    eprintln!("fatal: argument to {option} must be a reference");
    Err(GitError::Exit(128))
}

fn replay_target_from_revision(store: &FileRefStore, rev_args: &[String]) -> Result<String> {
    if rev_args.len() == 1 {
        let arg = &rev_args[0];
        let candidate = arg
            .rsplit_once("..")
            .map(|(_, right)| right)
            .filter(|right| !right.is_empty())
            .unwrap_or(arg);
        if let Ok(name) = replay_existing_ref(store, candidate, "--onto") {
            return Ok(name);
        }
    }
    eprintln!("fatal: could not determine ref to update");
    Err(GitError::Exit(128))
}

fn validate_replay_ref(name: &str) -> Result<String> {
    if !(name == "HEAD" || name.starts_with("refs/")) || validate_ref_name(name).is_err() {
        eprintln!("fatal: '{name}' is not a valid refname");
        return Err(GitError::Exit(128));
    }
    Ok(name.to_string())
}

fn read_direct_ref(
    store: &FileRefStore,
    format: ObjectFormat,
    name: &str,
) -> Result<Option<ObjectId>> {
    Ok(match store.read_ref(name)? {
        Some(RefTarget::Direct(oid)) => Some(oid),
        Some(RefTarget::Symbolic(target)) => match store.read_ref(&target)? {
            Some(RefTarget::Direct(oid)) => Some(oid),
            _ => None,
        },
        None => {
            let _ = format;
            None
        }
    })
}

fn select_git_replay_commits(
    ctx: &ReplayCtx,
    action: ReplayAction,
    rev_args: &[String],
    base: &ObjectId,
) -> Result<Vec<ObjectId>> {
    if action == ReplayAction::Pick
        && rev_args.len() == 1
        && !rev_args[0].contains("..")
        && !rev_args[0].starts_with('^')
    {
        let tip = resolve_revision(&ctx.git_dir, ctx.format, &rev_args[0])?;
        let db = ctx.db();
        let excluded = rev_list_walk_commits(&db, ctx.format, [*base], false)?
            .into_iter()
            .map(|record| record.oid)
            .collect::<HashSet<_>>();
        let mut commits = rev_list_walk_commits(&db, ctx.format, [tip], false)?
            .into_iter()
            .filter_map(|record| (!excluded.contains(&record.oid)).then_some(record.oid))
            .collect::<Vec<_>>();
        commits.reverse();
        return Ok(commits);
    }
    let selection = select_revisions(ctx, action, rev_args)?;
    Ok(selection.commits)
}

fn replay_commits_to_base(ctx: &ReplayCtx, plan: &GitReplayPlan) -> Result<ObjectId> {
    if plan.commits.is_empty() {
        return Ok(plan.base);
    }
    let mut head = plan.base;
    for oid in &plan.commits {
        head = replay_one_commit_to(ctx, plan.action, &head, oid)?;
    }
    Ok(head)
}

fn replay_one_commit_to(
    ctx: &ReplayCtx,
    action: ReplayAction,
    head: &ObjectId,
    oid: &ObjectId,
) -> Result<ObjectId> {
    let db = ctx.db();
    let object = db.read_object(oid)?;
    if object.object_type != ObjectType::Commit {
        return Err(GitError::InvalidObject(format!(
            "expected commit {oid}, found {}",
            object.object_type.as_str()
        )));
    }
    let commit = Commit::parse(ctx.format, &object.body)?;
    if commit.parents.len() > 1 {
        eprintln!("fatal: replaying merge commits is not supported yet!");
        return Err(GitError::Exit(128));
    }
    let parent = commit.parents.first().copied();
    let (base_map, theirs_map) = match action {
        ReplayAction::Pick => {
            let base = match parent {
                Some(parent) => {
                    tree_map_of_commit(ctx, &db, &parent).map_err(finish_replay_halt)?
                }
                None => MergeTreeMap::new(),
            };
            let theirs = stash_tree_entry_map(&db, ctx.format, &commit.tree)?;
            (base, theirs)
        }
        ReplayAction::Revert => {
            let base = stash_tree_entry_map(&db, ctx.format, &commit.tree)?;
            let theirs = match parent {
                Some(parent) => {
                    tree_map_of_commit(ctx, &db, &parent).map_err(finish_replay_halt)?
                }
                None => MergeTreeMap::new(),
            };
            (base, theirs)
        }
    };
    let head_tree = commit_tree_oid(&db, ctx.format, head)?;
    let ours_map = stash_tree_entry_map(&db, ctx.format, &head_tree)?;
    let (results, conflicts) = three_way_merge_trees_styled(
        &db,
        ctx.format,
        &base_map,
        &ours_map,
        &theirs_map,
        "HEAD",
        &format_log_abbrev_oid(oid),
        "parent",
        sley_diff_merge::ConflictStyle::Merge,
    )
    .map_err(|err| {
        eprintln!("error: {err}");
        GitError::Exit(128)
    })?;
    if !conflicts.is_empty() {
        return Err(GitError::Exit(1));
    }
    let tree_map = merge_results_to_tree_map(&results);
    let new_tree = write_tree_map_object(&db, ctx.format, &tree_map)?;
    if new_tree == head_tree {
        return Ok(*head);
    }
    let author = match action {
        ReplayAction::Pick => commit.author.clone(),
        ReplayAction::Revert => commit_identity_from_env("AUTHOR")?,
    };
    let message = match action {
        ReplayAction::Pick => commit.message.clone(),
        ReplayAction::Revert => format_revert_message(
            ctx,
            &db,
            &make_todo_item(ctx, ReplayAction::Revert, oid)?,
            &commit,
            &commit_subject(&commit.message),
            parent.as_ref(),
            &ReplayOpts::default(),
        )?,
    };
    let mut writer = ctx.db();
    sley_sequencer::create_commit(
        &mut writer,
        sley_sequencer::CommitCreate {
            tree: new_tree,
            parents: vec![*head],
            author,
            committer: commit_identity_from_env("COMMITTER")?,
            message,
            encoding: commit.encoding.clone(),
        },
    )
}

fn finish_replay_halt(halt: ReplayHalt) -> GitError {
    match halt {
        ReplayHalt::Fatal => GitError::Exit(128),
        ReplayHalt::Code(code) => GitError::Exit(code),
    }
}

fn merge_results_to_tree_map(results: &BTreeMap<Vec<u8>, MergePathResult>) -> MergeTreeMap {
    let mut out = MergeTreeMap::new();
    for (path, result) in results {
        if let MergePathResult::Resolved(Some((mode, oid))) = result {
            out.insert(path.clone(), (*mode, *oid));
        }
    }
    out
}

fn write_tree_map_object(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    entries: &MergeTreeMap,
) -> Result<ObjectId> {
    let _ = format;
    write_tree_map_level(db, entries, &[])
}

fn write_tree_map_level(
    db: &FileObjectDatabase,
    entries: &MergeTreeMap,
    prefix: &[u8],
) -> Result<ObjectId> {
    let mut tree_entries: Vec<TreeEntry> = Vec::new();
    let mut subdirs: BTreeSet<Vec<u8>> = BTreeSet::new();
    let prefix_len = if prefix.is_empty() {
        0
    } else {
        prefix.len() + 1
    };
    for (path, (mode, oid)) in entries {
        if !prefix.is_empty()
            && (!path.starts_with(prefix) || path.get(prefix.len()) != Some(&b'/'))
        {
            continue;
        }
        let rel = &path[prefix_len..];
        if let Some(slash) = rel.iter().position(|byte| *byte == b'/') {
            subdirs.insert(rel[..slash].to_vec());
        } else {
            tree_entries.push(TreeEntry {
                mode: *mode,
                name: BString::from(rel.to_vec()),
                oid: *oid,
            });
        }
    }
    for dir in subdirs {
        let mut sub_prefix = prefix.to_vec();
        if !sub_prefix.is_empty() {
            sub_prefix.push(b'/');
        }
        sub_prefix.extend_from_slice(&dir);
        let oid = write_tree_map_level(db, entries, &sub_prefix)?;
        tree_entries.push(TreeEntry {
            mode: 0o040000,
            name: BString::from(dir),
            oid,
        });
    }
    tree_entries.sort_by_key(|entry| {
        let mut key = entry.name.clone().into_bytes();
        if entry.mode == 0o040000 {
            key.push(b'/');
        }
        key
    });
    db.write_object(EncodedObject::new(
        ObjectType::Tree,
        Tree {
            entries: tree_entries,
        }
        .write(),
    ))
}

fn emit_or_update_replay_ref(
    ctx: &ReplayCtx,
    plan: &GitReplayPlan,
    new_oid: &ObjectId,
) -> Result<()> {
    let old_oid = plan.old_oid.unwrap_or_else(|| ObjectId::null(ctx.format));
    if plan.ref_action == ReplayRefAction::Print {
        println!("update {} {} {}", plan.target_ref, new_oid, old_oid);
        return Ok(());
    }
    let refs = ctx.refs();
    let mut tx = refs.transaction();
    tx.update(RefUpdate {
        name: plan.target_ref.clone(),
        expected: plan.old_oid.map(RefTarget::Direct),
        new: RefTarget::Direct(*new_oid),
        reflog: Some(ReflogEntry {
            old_oid,
            new_oid: *new_oid,
            committer: commit_identity_from_env("COMMITTER")?,
            message: plan.reflog_message.clone(),
        }),
    });
    tx.commit()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CmdMode {
    Quit,
    Continue,
    Abort,
    Skip,
}

impl CmdMode {
    fn option(self) -> &'static str {
        match self {
            CmdMode::Quit => "--quit",
            CmdMode::Continue => "--continue",
            CmdMode::Abort => "--abort",
            CmdMode::Skip => "--skip",
        }
    }
}

/// `--empty=` choice (cherry-pick only).
#[derive(Clone, Copy, PartialEq, Eq)]
enum EmptyOpt {
    Unspecified,
    Stop,
    Drop,
    Keep,
}

struct ParsedReplay {
    cmd: Option<CmdMode>,
    opts: ReplayOpts,
    empty_opt: EmptyOpt,
    /// Positional revision args plus pass-through rev-walk options.
    rev_args: Vec<String>,
}

fn usage_error(action: ReplayAction) -> GitError {
    eprint!("{}", usage_text(action));
    GitError::Exit(129)
}

fn option_error(message: &str) -> GitError {
    eprintln!("error: {message}");
    GitError::Exit(129)
}

/// `die()`-style failure: the porcelain prints `fatal: <action> failed` after
/// the already-printed error lines.
fn fatal_failed(action: ReplayAction) -> GitError {
    eprintln!("fatal: {} failed", action.name());
    GitError::Exit(128)
}

fn parse_replay_args(action: ReplayAction, args: &[String]) -> Result<ParsedReplay> {
    let mut cmd: Option<CmdMode> = None;
    let mut opts = ReplayOpts::default();
    let mut empty_opt = EmptyOpt::Unspecified;
    let mut rev_args: Vec<String> = Vec::new();
    let set_cmd = |current: &mut Option<CmdMode>, mode: CmdMode| -> Result<()> {
        if let Some(prev) = *current
            && prev != mode
        {
            eprintln!(
                "error: options '{}' and '{}' cannot be used together",
                mode.option(),
                prev.option()
            );
            return Err(GitError::Exit(129));
        }
        *current = Some(mode);
        Ok(())
    };
    let mut iter = args.iter().peekable();
    let mut seen_dashdash = false;
    while let Some(arg) = iter.next() {
        if seen_dashdash {
            rev_args.push(arg.clone());
            continue;
        }
        match arg.as_str() {
            "--" => {
                seen_dashdash = true;
                rev_args.push(arg.clone());
            }
            "--quit" => set_cmd(&mut cmd, CmdMode::Quit)?,
            "--continue" => set_cmd(&mut cmd, CmdMode::Continue)?,
            "--abort" => set_cmd(&mut cmd, CmdMode::Abort)?,
            "--skip" => set_cmd(&mut cmd, CmdMode::Skip)?,
            "-n" | "--no-commit" => opts.no_commit = true,
            "--commit" => opts.no_commit = false,
            "-e" | "--edit" => opts.edit = Some(true),
            "--no-edit" => opts.edit = Some(false),
            "-r" => {}
            "-s" | "--signoff" => opts.signoff = true,
            "--no-signoff" => opts.signoff = false,
            "-m" | "--mainline" => {
                let Some(value) = iter.next() else {
                    return Err(option_error("switch `m' requires a value"));
                };
                opts.mainline = parse_mainline(value)?;
            }
            value if value.starts_with("-m") && value.len() > 2 => {
                opts.mainline = parse_mainline(&value[2..])?;
            }
            value if value.starts_with("--mainline=") => {
                opts.mainline = parse_mainline(&value["--mainline=".len()..])?;
            }
            "--rerere-autoupdate" => opts.allow_rerere_auto = Some(true),
            "--no-rerere-autoupdate" => opts.allow_rerere_auto = Some(false),
            "--strategy" => {
                let Some(value) = iter.next() else {
                    return Err(option_error("switch `strategy' requires a value"));
                };
                opts.strategy = Some(value.clone());
            }
            value if value.starts_with("--strategy=") => {
                opts.strategy = Some(value["--strategy=".len()..].to_string());
            }
            "-X" | "--strategy-option" => {
                let Some(value) = iter.next() else {
                    return Err(option_error("switch `X' requires a value"));
                };
                opts.strategy_options.push(value.clone());
            }
            value if value.starts_with("-X") && value.len() > 2 => {
                opts.strategy_options.push(value[2..].to_string());
            }
            value if value.starts_with("--strategy-option=") => {
                opts.strategy_options
                    .push(value["--strategy-option=".len()..].to_string());
            }
            "-S" | "--gpg-sign" => opts.gpg_sign = Some(String::new()),
            value if value.starts_with("-S") && value.len() > 2 => {
                opts.gpg_sign = Some(value[2..].to_string());
            }
            value if value.starts_with("--gpg-sign=") => {
                opts.gpg_sign = Some(value["--gpg-sign=".len()..].to_string());
            }
            "--no-gpg-sign" => opts.gpg_sign = None,
            "--cleanup" => {
                let Some(value) = iter.next() else {
                    return Err(option_error("switch `cleanup' requires a value"));
                };
                opts.default_msg_cleanup = Some(value.clone());
            }
            value if value.starts_with("--cleanup=") => {
                opts.default_msg_cleanup = Some(value["--cleanup=".len()..].to_string());
            }
            "-x" if action == ReplayAction::Pick => opts.record_origin = true,
            "--ff" if action == ReplayAction::Pick => opts.allow_ff = true,
            "--no-ff" if action == ReplayAction::Pick => opts.allow_ff = false,
            "--allow-empty" if action == ReplayAction::Pick => opts.allow_empty = true,
            "--no-allow-empty" if action == ReplayAction::Pick => opts.allow_empty = false,
            "--allow-empty-message" if action == ReplayAction::Pick => {
                opts.allow_empty_message = true;
            }
            "--no-allow-empty-message" if action == ReplayAction::Pick => {
                opts.allow_empty_message = false;
            }
            "--keep-redundant-commits" if action == ReplayAction::Pick => {
                opts.keep_redundant_commits = true;
            }
            "--no-keep-redundant-commits" if action == ReplayAction::Pick => {
                opts.keep_redundant_commits = false;
            }
            value if action == ReplayAction::Pick && value.starts_with("--empty=") => {
                empty_opt = match &value["--empty=".len()..] {
                    "stop" => EmptyOpt::Stop,
                    "drop" => EmptyOpt::Drop,
                    "keep" => EmptyOpt::Keep,
                    other => {
                        return Err(option_error(&format!(
                            "invalid value for '--empty': '{other}'"
                        )));
                    }
                };
            }
            "--reference" if action == ReplayAction::Revert => {
                opts.commit_use_reference = true;
            }
            "--no-reference" if action == ReplayAction::Revert => {
                opts.commit_use_reference = false;
            }
            // Everything else (including unknown options) passes through to
            // the rev-walk argument parser, mirroring
            // PARSE_OPT_KEEP_UNKNOWN_OPT + setup_revisions.
            value => rev_args.push(value.to_string()),
        }
    }
    Ok(ParsedReplay {
        cmd,
        opts,
        empty_opt,
        rev_args,
    })
}

fn parse_mainline(value: &str) -> Result<u32> {
    match value.parse::<i64>() {
        Ok(n) if n > 0 => Ok(n as u32),
        _ => Err(option_error(
            "option `mainline' expects a number greater than zero",
        )),
    }
}

/// Repository handles shared by every replay operation.
struct ReplayCtx {
    action: ReplayAction,
    git_dir: PathBuf,
    common_git_dir: PathBuf,
    worktree_root: PathBuf,
    format: ObjectFormat,
}

impl ReplayCtx {
    fn db(&self) -> FileObjectDatabase {
        FileObjectDatabase::from_git_dir(&self.common_git_dir, self.format)
    }

    fn refs(&self) -> FileRefStore {
        FileRefStore::new(&self.git_dir, self.format)
    }

    fn head_oid(&self) -> Option<ObjectId> {
        head_commit_oid(&self.refs()).ok().flatten()
    }
}

fn run_replay(action: ReplayAction, args: &[String]) -> Result<()> {
    let parsed = parse_replay_args(action, args)?;
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let format = repository_object_format(&common_git_dir)?;
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    let ctx = ReplayCtx {
        action,
        git_dir,
        common_git_dir,
        worktree_root,
        format,
    };

    let me = action.name();
    if let Some(cmd) = parsed.cmd {
        // verify_opt_compatible: a cmdmode rejects the pick options.
        let incompatible: &[(&str, bool)] = &[
            ("--no-commit", parsed.opts.no_commit),
            ("--signoff", parsed.opts.signoff),
            ("--mainline", parsed.opts.mainline > 0),
            ("--strategy", parsed.opts.strategy.is_some()),
            (
                "--strategy-option",
                !parsed.opts.strategy_options.is_empty(),
            ),
            ("-x", parsed.opts.record_origin),
            ("--ff", parsed.opts.allow_ff),
            (
                "--rerere-autoupdate",
                parsed.opts.allow_rerere_auto == Some(true),
            ),
            (
                "--no-rerere-autoupdate",
                parsed.opts.allow_rerere_auto == Some(false),
            ),
            (
                "--keep-redundant-commits",
                parsed.opts.keep_redundant_commits,
            ),
            ("--empty", parsed.empty_opt != EmptyOpt::Unspecified),
        ];
        for (name, set) in incompatible {
            if *set {
                eprintln!("fatal: {me}: {name} cannot be used with {}", cmd.option());
                return Err(GitError::Exit(128));
            }
        }
        return match cmd {
            CmdMode::Quit => {
                replay::remove_state(&ctx.git_dir);
                replay::remove_branch_state(&ctx.git_dir);
                Ok(())
            }
            CmdMode::Continue => sequencer_continue(&ctx),
            CmdMode::Abort => sequencer_rollback(&ctx),
            CmdMode::Skip => sequencer_skip(&ctx),
        };
    }

    let mut opts = parsed.opts;
    if action == ReplayAction::Pick {
        opts.drop_redundant_commits = parsed.empty_opt == EmptyOpt::Drop;
        opts.keep_redundant_commits =
            opts.keep_redundant_commits || parsed.empty_opt == EmptyOpt::Keep;
    }
    if opts.keep_redundant_commits {
        opts.allow_empty = true;
    }
    if action == ReplayAction::Revert && opts.commit_use_reference {
        // git also flips this from the revert.reference config.
    } else if action == ReplayAction::Revert
        && config_bool(&ctx.git_dir, "revert", "reference").unwrap_or(false)
    {
        opts.commit_use_reference = true;
    }
    if opts.allow_ff {
        for (name, set) in [
            ("--signoff", opts.signoff),
            ("--no-commit", opts.no_commit),
            ("-x", opts.record_origin),
            ("--edit", opts.edit == Some(true)),
        ] {
            if set {
                eprintln!("fatal: {me}: --ff cannot be used with {name}");
                return Err(GitError::Exit(128));
            }
        }
    }
    if parsed.rev_args.is_empty() {
        return Err(usage_error(action));
    }
    sequencer_pick_revisions(&ctx, &opts, &parsed.rev_args)
}

fn config_bool(git_dir: &Path, section: &str, key: &str) -> Option<bool> {
    let value = config_value(git_dir, section, key)?;
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" | "" => Some(true),
        "false" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

/// Effective-config lookup (repo + global/system + `-c`/env injection).
fn config_value(git_dir: &Path, section: &str, key: &str) -> Option<String> {
    if let Some(config) = crate::commands::merge_rebase::effective_config_with_overrides()
        && let Some(value) = config.get(section, None, key)
    {
        return Some(value.to_string());
    }
    let config = read_repo_config(git_dir).ok()?;
    config.get(section, None, key).map(str::to_string)
}

// ---------------------------------------------------------------------------
// Revision selection
// ---------------------------------------------------------------------------

struct RevSelection {
    /// Resolved commits in pick order.
    commits: Vec<ObjectId>,
    /// True when the args were a single plain revision (no ranges, walk
    /// options, or extra revs) — git's single-pick fast path that skips the
    /// sequencer directory.
    single: bool,
}

/// Resolve the rev arguments into the ordered commit list (mirrors
/// `setup_revisions` + `walk_revs_populate_todo`.
fn select_revisions(
    ctx: &ReplayCtx,
    action: ReplayAction,
    rev_args: &[String],
) -> Result<RevSelection> {
    let db = ctx.db();
    let config = read_repo_config(&ctx.git_dir)?;
    let cwd = env::current_dir()?;
    let setup_args = rev_args
        .iter()
        .map(|arg| {
            if arg == "-" {
                "@{-1}".to_string()
            } else {
                arg.clone()
            }
        })
        .collect::<Vec<_>>();
    let setup = sley_rev::setup_revisions(
        &setup_args,
        &sley_rev::RevisionSetupContext {
            git_dir: &ctx.git_dir,
            worktree_root: Some(&ctx.worktree_root),
            cwd: &cwd,
            format: ctx.format,
            reader: &db,
            config: Some(&config),
        },
    )?;
    if !setup.leftovers.is_empty() || !setup.pathspecs.is_empty() {
        return Err(usage_error(action));
    }
    let mut options = setup.options;
    if !options.has_revisions() && options.max_count.is_some() {
        let oid = resolve_revision(&ctx.git_dir, ctx.format, "HEAD").map_err(|_| {
            eprintln!("fatal: bad revision 'HEAD'");
            GitError::Exit(128)
        })?;
        options.positives.push(sley_rev::RevisionTip {
            oid,
            rev: "HEAD".to_string(),
            source_name: Some("HEAD".to_string()),
            from_ref_selector: false,
        });
    }
    if !options.has_revisions() {
        return Ok(RevSelection {
            commits: Vec::new(),
            single: false,
        });
    }

    let has_walk_spec = !options.negatives.is_empty()
        || !options.symmetric_ranges.is_empty()
        || options.max_count.is_some()
        || options.skip > 0
        || options.date_window.min_time.is_some()
        || options.date_window.max_time.is_some()
        || !options.author_patterns.is_empty();
    let mut commits: Vec<ObjectId> = Vec::new();
    if has_walk_spec {
        let starts = options
            .positives
            .iter()
            .map(|tip| {
                sley_rev::peel_to_commit(&db, ctx.format, &tip.oid).map_err(|_| {
                    eprintln!("error: {}: can't cherry-pick that object", tip.rev);
                    fatal_failed(action)
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let mut excluded = HashSet::new();
        for oid in &options.negatives {
            for record in rev_list_walk_commits(&db, ctx.format, [*oid], options.first_parent)? {
                excluded.insert(record.oid);
            }
        }
        let order = match options.order {
            sley_rev::RevisionOrder::Default | sley_rev::RevisionOrder::Date => {
                sley_rev::RevWalkOrder::CommitDate
            }
            sley_rev::RevisionOrder::Topo => sley_rev::RevWalkOrder::Topo,
            sley_rev::RevisionOrder::AuthorDate => sley_rev::RevWalkOrder::AuthorDate,
        };
        let mut walk = sley_rev::RevWalk::new(&ctx.git_dir, ctx.format, &db, starts)
            .order(order)
            .first_parent(options.first_parent)
            .date_window(options.date_window);
        while let Some(record) = walk.try_next()? {
            if excluded.contains(&record.oid) {
                continue;
            }
            commits.push(record.oid);
        }
    } else {
        for tip in &options.positives {
            let commit_oid = sley_rev::peel_to_commit(&db, ctx.format, &tip.oid).map_err(|_| {
                eprintln!("error: {}: can't cherry-pick that object", tip.rev);
                fatal_failed(action)
            })?;
            commits.push(commit_oid);
        }
    }

    if !options.author_patterns.is_empty() {
        commits.retain(|oid| {
            options.author_patterns.iter().all(|pattern| {
                commit_author_matches(&db, ctx.format, oid, pattern).unwrap_or(false)
            })
        });
    }
    if options.skip > 0 {
        commits = commits.into_iter().skip(options.skip).collect();
    }
    if let Some(limit) = options.max_count {
        commits.truncate(limit);
    }
    // Picking ranges (a real walk) happens oldest-first; reverting keeps the
    // newest-first walk order. Plain rev lists keep argument order either way.
    if action == ReplayAction::Pick && has_walk_spec {
        commits.reverse();
    }
    let single = commits.len() == 1
        && options.positives.len() == 1
        && options.negatives.is_empty()
        && options.symmetric_ranges.is_empty()
        && !has_walk_spec
        && options.author_patterns.is_empty();
    Ok(RevSelection { commits, single })
}

fn commit_author_matches(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    oid: &ObjectId,
    pattern: &str,
) -> Result<bool> {
    let object = db.read_object(oid)?;
    let commit = Commit::parse(format, &object.body)?;
    let author = String::from_utf8_lossy(&commit.author).to_string();
    // git matches --author as a (basic) regex; the corpus only needs
    // substring-with-dot-wildcard semantics, so approximate `.` as any-char.
    Ok(regexish_contains(&author, pattern))
}

/// Tiny "regex" matcher: `.` matches any character, everything else literal.
fn regexish_contains(haystack: &str, pattern: &str) -> bool {
    let h: Vec<char> = haystack.chars().collect();
    let p: Vec<char> = pattern.chars().collect();
    if p.is_empty() {
        return true;
    }
    if h.len() < p.len() {
        return false;
    }
    for start in 0..=(h.len() - p.len()) {
        if p.iter()
            .enumerate()
            .all(|(i, pc)| *pc == '.' || h[start + i] == *pc)
        {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Top-level flows
// ---------------------------------------------------------------------------

fn sequencer_pick_revisions(ctx: &ReplayCtx, opts: &ReplayOpts, rev_args: &[String]) -> Result<()> {
    let selection = select_revisions(ctx, ctx.action, rev_args)?;
    if selection.commits.is_empty() {
        eprintln!("error: empty commit set passed");
        return Err(fatal_failed(ctx.action));
    }
    if selection.single {
        // Single plain rev: replay it without touching the sequencer dir.
        let item = make_todo_item(ctx, ctx.action, &selection.commits[0])?;
        return match do_pick_commit(ctx, opts, &item, true) {
            Ok(PickFlow::Done | PickFlow::Dropped) => Ok(()),
            Ok(PickFlow::Conflict) => Err(GitError::Exit(1)),
            Ok(PickFlow::HaltEmpty) => Err(GitError::Exit(1)),
            Err(halt) => Err(finish_halt(ctx, halt)),
        };
    }

    let mut items = Vec::with_capacity(selection.commits.len());
    for oid in &selection.commits {
        items.push(make_todo_item(ctx, ctx.action, oid)?);
    }
    let advise_skip =
        ctx.git_dir.join("CHERRY_PICK_HEAD").exists() || ctx.git_dir.join("REVERT_HEAD").exists();
    if let Some(in_progress) = replay::in_progress_error(&ctx.git_dir, advise_skip) {
        eprintln!("error: {}", in_progress.error);
        if config_bool(&ctx.git_dir, "advice", "sequencerInUse").unwrap_or(true) {
            for line in in_progress.hint.lines() {
                eprintln!("hint: {line}");
            }
        }
        return Err(fatal_failed(ctx.action));
    }
    replay::create_seq_dir(&ctx.git_dir).map_err(|err| {
        eprintln!("error: {err}");
        fatal_failed(ctx.action)
    })?;
    let head = ctx.head_oid();
    if head.is_none() && ctx.action == ReplayAction::Revert {
        eprintln!("error: can't revert as initial commit");
        return Err(fatal_failed(ctx.action));
    }
    let head_text = match &head {
        Some(oid) => oid.to_hex(),
        None => ObjectId::null(ctx.format).to_hex(),
    };
    replay::save_head(&ctx.git_dir, &head_text)?;
    replay::save_opts(&ctx.git_dir, opts)?;
    replay::update_abort_safety(&ctx.git_dir, head.as_ref());
    pick_commits(ctx, opts, &items)
}

/// The unmerged-index guard inside `do_pick_commit`'s dirty-index check
/// (`error_resolve_conflict`).
fn check_no_unmerged(ctx: &ReplayCtx) -> std::result::Result<(), ReplayHalt> {
    let index_path = sley_worktree::repository_index_path(&ctx.git_dir);
    let Ok(bytes) = fs::read(&index_path) else {
        return Ok(());
    };
    let index = Index::parse(&bytes, ctx.format).map_err(print_fatal_error)?;
    if index
        .entries
        .iter()
        .any(|entry| index_entry_stage(entry) > 0)
    {
        let verb = match ctx.action {
            ReplayAction::Pick => "Cherry-picking",
            ReplayAction::Revert => "Reverting",
        };
        eprintln!("error: {verb} is not possible because you have unmerged files.");
        eprintln!("hint: Fix them up in the work tree, and then use 'git add/rm <file>'");
        eprintln!("hint: as appropriate to mark resolution and make a commit.");
        return Err(ReplayHalt::Fatal);
    }
    Ok(())
}

fn make_todo_item(ctx: &ReplayCtx, action: ReplayAction, oid: &ObjectId) -> Result<TodoItem> {
    let db = ctx.db();
    let object = db.read_object(oid)?;
    let commit = Commit::parse(ctx.format, &object.body)?;
    let subject = commit_subject(&commit.message);
    let short = format_log_abbrev_oid(oid);
    let display = if subject.is_empty() {
        short
    } else {
        format!("{short} {subject}")
    };
    Ok(TodoItem {
        action,
        oid: *oid,
        display,
    })
}

/// The pick loop (`pick_commits`): replay every remaining todo item, saving
/// the sheet before each step, and tear the state down on success.
fn pick_commits(ctx: &ReplayCtx, opts: &ReplayOpts, items: &[TodoItem]) -> Result<()> {
    for (index, item) in items.iter().enumerate() {
        replay::save_todo(&ctx.git_dir, &items[index..])?;
        match do_pick_commit(ctx, opts, item, false) {
            Ok(PickFlow::Done | PickFlow::Dropped) => {}
            Ok(PickFlow::Conflict) => return Err(GitError::Exit(1)),
            Ok(PickFlow::HaltEmpty) => return Err(GitError::Exit(1)),
            Err(halt) => return Err(finish_halt(ctx, halt)),
        }
    }
    replay::remove_state(&ctx.git_dir);
    Ok(())
}

/// Failure routing for the pick engine: `Fatal` is the `res < 0` path (the
/// porcelain appends `fatal: <action> failed`, exit 128); `Code` propagates a
/// child-status exit (no fatal line).
enum ReplayHalt {
    Fatal,
    Code(i32),
}

fn finish_halt(ctx: &ReplayCtx, halt: ReplayHalt) -> GitError {
    match halt {
        ReplayHalt::Fatal => fatal_failed(ctx.action),
        ReplayHalt::Code(code) => GitError::Exit(code),
    }
}

enum PickFlow {
    /// Commit created (or fast-forwarded / no-commit staged).
    Done,
    /// Redundant commit dropped (`--empty=drop`).
    Dropped,
    /// Stopped with conflicts (exit 1).
    Conflict,
    /// Halted on a commit that became empty (exit 1).
    HaltEmpty,
}

#[allow(clippy::too_many_lines)]
fn do_pick_commit(
    ctx: &ReplayCtx,
    opts: &ReplayOpts,
    item: &TodoItem,
    _is_single: bool,
) -> std::result::Result<PickFlow, ReplayHalt> {
    let action = item.action;
    let db = ctx.db();
    let commit_object = read_object_or_fatal(ctx, &db, &item.oid)?;
    let commit = Commit::parse(ctx.format, &commit_object.body).map_err(|err| {
        eprintln!("error: {err}");
        ReplayHalt::Fatal
    })?;

    // HEAD / index-dirtiness checks.
    check_no_unmerged(ctx)?;
    let head = ctx.head_oid();
    let unborn = head.is_none();
    let head_tree = match &head {
        Some(oid) => commit_tree_oid(&db, ctx.format, oid).map_err(print_fatal_error)?,
        None => ObjectId::empty_tree(ctx.format),
    };
    let index_tree = index_tree_oid(ctx).map_err(print_fatal_error)?;
    if !opts.no_commit && index_tree != head_tree {
        eprintln!(
            "error: your local changes would be overwritten by {}.",
            action.name()
        );
        if config_bool(&ctx.git_dir, "advice", "commitBeforeMerge").unwrap_or(true) {
            eprintln!("hint: commit your changes or stash them to proceed.");
        }
        return Err(ReplayHalt::Fatal);
    }

    // Mainline / parent selection.
    let parent: Option<ObjectId> = if commit.parents.is_empty() {
        None
    } else if commit.parents.len() > 1 {
        if opts.mainline == 0 {
            eprintln!(
                "error: commit {} is a merge but no -m option was given.",
                item.oid
            );
            return Err(ReplayHalt::Fatal);
        }
        match commit.parents.get(opts.mainline as usize - 1) {
            Some(parent) => Some(*parent),
            None => {
                eprintln!(
                    "error: commit {} does not have parent {}",
                    item.oid, opts.mainline
                );
                return Err(ReplayHalt::Fatal);
            }
        }
    } else if opts.mainline > 1 {
        eprintln!(
            "error: commit {} does not have parent {}",
            item.oid, opts.mainline
        );
        return Err(ReplayHalt::Fatal);
    } else {
        Some(commit.parents[0])
    };

    let subject = commit_subject(&commit.message);
    let short = format_log_abbrev_oid(&item.oid);
    let label = format!("{short} ({subject})");
    let parent_label = format!("parent of {label}");

    // Fast-forward (`--ff`).
    if opts.allow_ff
        && ((parent.is_some() && parent.as_ref() == head.as_ref()) || (parent.is_none() && unborn))
    {
        fast_forward_to(ctx, &item.oid, head.as_ref()).map_err(print_fatal_error)?;
        return Ok(PickFlow::Done);
    }

    // Replay message.
    let mut message: Vec<u8> = match action {
        ReplayAction::Pick => {
            let mut msg = commit.message.clone();
            if opts.record_origin {
                if !msg.ends_with(b"\n") {
                    msg.push(b'\n');
                }
                if !has_conforming_footer(&msg) {
                    msg.push(b'\n');
                }
                msg.extend_from_slice(
                    format!("(cherry picked from commit {})\n", item.oid).as_bytes(),
                );
            }
            msg
        }
        ReplayAction::Revert => {
            format_revert_message(ctx, &db, item, &commit, &subject, parent.as_ref(), opts)
                .map_err(print_fatal_error)?
        }
    };
    if opts.signoff {
        let signoff = commit_signoff_from_env().map_err(print_fatal_error)?;
        message = append_signoff_before_comments(message, &signoff);
    }

    // Tree maps for the 3-way replay.
    let (base_map, theirs_map, theirs_label, ancestor_label) = match action {
        ReplayAction::Pick => {
            let base = match &parent {
                Some(parent) => tree_map_of_commit(ctx, &db, parent)?,
                None => MergeTreeMap::new(),
            };
            let theirs =
                stash_tree_entry_map(&db, ctx.format, &commit.tree).map_err(print_fatal_error)?;
            (base, theirs, label.clone(), parent_label.clone())
        }
        ReplayAction::Revert => {
            let base =
                stash_tree_entry_map(&db, ctx.format, &commit.tree).map_err(print_fatal_error)?;
            let theirs = match &parent {
                Some(parent) => tree_map_of_commit(ctx, &db, parent)?,
                None => MergeTreeMap::new(),
            };
            (base, theirs, parent_label.clone(), label.clone())
        }
    };
    let ours_map = stash_tree_entry_map(&db, ctx.format, &index_tree).map_err(print_fatal_error)?;

    let style = match config_value(&ctx.git_dir, "merge", "conflictstyle").as_deref() {
        Some("diff3") | Some("zdiff3") => sley_diff_merge::ConflictStyle::Diff3,
        _ => sley_diff_merge::ConflictStyle::Merge,
    };
    let (results, conflicts) = three_way_merge_trees_styled(
        &db,
        ctx.format,
        &base_map,
        &ours_map,
        &theirs_map,
        "HEAD",
        &theirs_label,
        &ancestor_label,
        style,
    )
    .map_err(print_fatal_error)?;

    // Pre-flight worktree clobber checks (unpack_trees' verify steps).
    verify_worktree_safe(ctx, &ours_map, &results)?;

    let cleanup_mode = opts
        .default_msg_cleanup
        .clone()
        .or_else(|| config_value(&ctx.git_dir, "commit", "cleanup"));

    if !conflicts.is_empty() {
        apply_merge_results_to_index_and_worktree(ctx, &db, &ours_map, &results)
            .map_err(print_fatal_error)?;
        // State files for the resolution flow.
        let help_msg = env::var("GIT_CHERRY_PICK_HELP").ok();
        let suppress_pick_head = help_msg.is_some();
        if action == ReplayAction::Pick && !opts.no_commit && !suppress_pick_head {
            fs::write(
                ctx.git_dir.join("CHERRY_PICK_HEAD"),
                format!("{}\n", item.oid),
            )
            .map_err(|err| print_fatal_error(GitError::from(err)))?;
        }
        if action == ReplayAction::Revert {
            fs::write(ctx.git_dir.join("REVERT_HEAD"), format!("{}\n", item.oid))
                .map_err(|err| print_fatal_error(GitError::from(err)))?;
        }
        let mut merge_msg = message.clone();
        append_conflicts_hint(&mut merge_msg, &conflicts, cleanup_mode.as_deref());
        fs::write(ctx.git_dir.join("MERGE_MSG"), merge_msg)
            .map_err(|err| print_fatal_error(GitError::from(err)))?;

        // stdout: per-path merge notices; stderr: the error + advice.
        for path in &conflicts {
            let display = String::from_utf8_lossy(path);
            if let Some(MergePathResult::Conflict {
                base, ours, theirs, ..
            }) = results.get(path)
                && base.is_some()
                && ours.is_some()
                && theirs.is_some()
            {
                println!("Auto-merging {display}");
            }
            println!("CONFLICT (content): Merge conflict in {display}");
        }
        let verb = match action {
            ReplayAction::Pick => "apply",
            ReplayAction::Revert => "revert",
        };
        eprintln!("error: could not {verb} {short}... {subject}");
        print_conflict_advice(ctx, opts, help_msg.as_deref());
        replay::update_abort_safety(&ctx.git_dir, head.as_ref());
        return Ok(PickFlow::Conflict);
    }

    // Clean merge: stage the result.
    apply_merge_results_to_index_and_worktree(ctx, &db, &ours_map, &results)
        .map_err(print_fatal_error)?;
    let new_tree = sley_worktree::write_tree_from_index(&ctx.git_dir, ctx.format)
        .map_err(print_fatal_error)?;

    if opts.no_commit {
        if action == ReplayAction::Revert {
            fs::write(ctx.git_dir.join("REVERT_HEAD"), format!("{}\n", item.oid))
                .map_err(|err| print_fatal_error(GitError::from(err)))?;
        }
        fs::write(ctx.git_dir.join("MERGE_MSG"), &message)
            .map_err(|err| print_fatal_error(GitError::from(err)))?;
        replay::update_abort_safety(&ctx.git_dir, head.as_ref());
        return Ok(PickFlow::Done);
    }

    // CHERRY_PICK_HEAD / REVERT_HEAD ahead of the commit attempt.
    if action == ReplayAction::Pick {
        fs::write(
            ctx.git_dir.join("CHERRY_PICK_HEAD"),
            format!("{}\n", item.oid),
        )
        .map_err(|err| print_fatal_error(GitError::from(err)))?;
    }
    fs::write(ctx.git_dir.join("MERGE_MSG"), &message)
        .map_err(|err| print_fatal_error(GitError::from(err)))?;

    // Empty-commit handling.
    if new_tree == head_tree {
        let originally_empty = original_commit_empty(ctx, &db, &commit)?;
        let allow = if originally_empty {
            u8::from(opts.allow_empty)
        } else if opts.keep_redundant_commits {
            1
        } else if opts.drop_redundant_commits {
            2
        } else {
            0
        };
        match allow {
            0 => {
                // Halt: the spawned `git commit` would refuse and print the
                // empty-cherry-pick advice; replicate its stderr and exit 1.
                print_empty_halt_advice(ctx);
                replay::update_abort_safety(&ctx.git_dir, head.as_ref());
                return Ok(PickFlow::HaltEmpty);
            }
            2 => {
                let _ = fs::remove_file(ctx.git_dir.join("CHERRY_PICK_HEAD"));
                let _ = fs::remove_file(ctx.git_dir.join("MERGE_MSG"));
                eprintln!(
                    "dropping {} {} -- patch contents already upstream",
                    item.oid, subject
                );
                replay::update_abort_safety(&ctx.git_dir, head.as_ref());
                return Ok(PickFlow::Dropped);
            }
            _ => {}
        }
    }

    // Optional message edit.
    if should_edit(ctx.action, opts) {
        message = edit_message(ctx, &message).map_err(|err| {
            // Editor failure cancels the commit but keeps the state files.
            eprintln!("error: {err}");
            ReplayHalt::Code(1)
        })?;
    }

    // Create the commit and advance HEAD.
    let author = match action {
        ReplayAction::Pick => commit.author.clone(),
        ReplayAction::Revert => commit_identity_from_env("AUTHOR").map_err(print_fatal_error)?,
    };
    let committer = commit_identity_from_env("COMMITTER").map_err(print_fatal_error)?;
    let reflog_message = format!("{}: {subject}", action.name()).into_bytes();
    let new_oid = commit_and_advance_head(
        ctx,
        &new_tree,
        head.as_ref(),
        author,
        committer,
        &message,
        reflog_message,
    )
    .map_err(print_fatal_error)?;
    let _ = fs::remove_file(ctx.git_dir.join("CHERRY_PICK_HEAD"));
    let _ = fs::remove_file(ctx.git_dir.join("MERGE_MSG"));
    print_commit_summary_line(ctx, &new_oid, &message);
    replay::update_abort_safety(&ctx.git_dir, Some(&new_oid));
    Ok(PickFlow::Done)
}

fn read_object_or_fatal(
    ctx: &ReplayCtx,
    db: &FileObjectDatabase,
    oid: &ObjectId,
) -> std::result::Result<std::sync::Arc<EncodedObject>, ReplayHalt> {
    let _ = ctx;
    match db.read_object(oid) {
        Ok(object) if object.object_type == ObjectType::Commit => Ok(object),
        Ok(object) => {
            eprintln!(
                "error: expected commit {oid}, found {}",
                object.object_type.as_str()
            );
            Err(ReplayHalt::Fatal)
        }
        Err(err) => {
            eprintln!("error: {err}");
            Err(ReplayHalt::Fatal)
        }
    }
}

fn print_fatal_error(err: GitError) -> ReplayHalt {
    eprintln!("error: {err}");
    ReplayHalt::Fatal
}

fn tree_map_of_commit(
    ctx: &ReplayCtx,
    db: &FileObjectDatabase,
    oid: &ObjectId,
) -> std::result::Result<MergeTreeMap, ReplayHalt> {
    let tree = commit_tree_oid(db, ctx.format, oid).map_err(print_fatal_error)?;
    stash_tree_entry_map(db, ctx.format, &tree).map_err(print_fatal_error)
}

/// Tree oid of the current index (the "are there staged changes" probe).
fn index_tree_oid(ctx: &ReplayCtx) -> Result<ObjectId> {
    let index_path = sley_worktree::repository_index_path(&ctx.git_dir);
    if !index_path.exists() {
        return Ok(ObjectId::empty_tree(ctx.format));
    }
    sley_worktree::write_tree_from_index(&ctx.git_dir, ctx.format)
}

fn original_commit_empty(
    ctx: &ReplayCtx,
    db: &FileObjectDatabase,
    commit: &Commit,
) -> std::result::Result<bool, ReplayHalt> {
    let parent_tree = match commit.parents.first() {
        Some(parent) => commit_tree_oid(db, ctx.format, parent).map_err(print_fatal_error)?,
        None => ObjectId::empty_tree(ctx.format),
    };
    Ok(parent_tree == commit.tree)
}

fn should_edit(action: ReplayAction, opts: &ReplayOpts) -> bool {
    use std::io::IsTerminal as _;
    match opts.edit {
        Some(edit) => edit,
        // Unspecified: revert edits when stdin is a tty; cherry-pick doesn't.
        None => action == ReplayAction::Revert && std::io::stdin().is_terminal(),
    }
}

/// Launch the configured editor over `.git/COMMIT_EDITMSG` seeded with
/// `message`; the edited result is cleaned of comment lines.
fn edit_message(ctx: &ReplayCtx, message: &[u8]) -> Result<Vec<u8>> {
    let path = ctx.git_dir.join("COMMIT_EDITMSG");
    let comment = comment_char(&ctx.git_dir);
    let mut template = message.to_vec();
    if !template.ends_with(b"\n") {
        template.push(b'\n');
    }
    // The editor template carries the commented help block git appends.
    template.push(b'\n');
    let c = comment as char;
    template.extend_from_slice(
        format!(
            "{c} Please enter the commit message for your changes. Lines starting\n{c} with '{c}' will be ignored, and an empty message aborts the commit.\n"
        )
        .as_bytes(),
    );
    fs::write(&path, template)?;
    launch_editor(&ctx.git_dir, &path)?;
    let edited = fs::read(&path)?;
    Ok(strip_comment_lines(&edited, comment))
}

pub(crate) fn launch_editor(git_dir: &Path, path: &Path) -> Result<()> {
    let editor = env::var("GIT_EDITOR")
        .ok()
        .or_else(|| config_value(git_dir, "core", "editor"))
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
        .status()?;
    if !status.success() {
        return Err(GitError::Command(format!(
            "there was a problem with the editor '{editor}'"
        )));
    }
    Ok(())
}

pub(crate) fn comment_char(git_dir: &Path) -> u8 {
    match config_value(git_dir, "core", "commentChar").as_deref() {
        Some(value) if value.eq_ignore_ascii_case("auto") => b'#',
        Some(value) => value.bytes().next().unwrap_or(b'#'),
        None => b'#',
    }
}

pub(crate) fn strip_comment_lines(message: &[u8], comment: u8) -> Vec<u8> {
    let mut out = Vec::with_capacity(message.len());
    let mut blank_pending = false;
    for line in message.split_inclusive(|&b| b == b'\n') {
        if line.first() == Some(&comment) {
            continue;
        }
        let body = line.strip_suffix(b"\n").unwrap_or(line);
        if body.iter().all(|b| b.is_ascii_whitespace()) {
            blank_pending = !out.is_empty();
            continue;
        }
        if blank_pending {
            out.push(b'\n');
            blank_pending = false;
        }
        out.extend_from_slice(line);
    }
    // stripspace: trim leading/trailing blank lines and collapse internal
    // blank runs to a single empty line.
    let text = out;
    let mut start = 0;
    while start < text.len() && text[start] == b'\n' {
        start += 1;
    }
    let mut end = text.len();
    while end > start && text[end - 1] == b'\n' {
        end -= 1;
    }
    let mut cleaned = text[start..end].to_vec();
    if !cleaned.is_empty() {
        cleaned.push(b'\n');
    }
    cleaned
}

/// `has_conforming_footer` (approximation): the last paragraph consists of
/// `Key: value` trailer lines.
fn has_conforming_footer(message: &[u8]) -> bool {
    let text = String::from_utf8_lossy(message);
    let trimmed = text.trim_end_matches('\n');
    let last_para = match trimmed.rfind("\n\n") {
        Some(pos) => &trimmed[pos + 2..],
        None => return false,
    };
    if last_para.is_empty() {
        return false;
    }
    last_para.lines().all(|line| {
        line.split_once(':').is_some_and(|(key, value)| {
            !key.is_empty() && !key.contains(char::is_whitespace) && value.starts_with(' ')
        }) || line.starts_with("(cherry picked from commit ")
    })
}

/// Append a `Signed-off-by:` trailer ahead of any trailing comment block
/// (`append_signoff` + `ignored_log_message_bytes`).
pub(crate) fn append_signoff_before_comments(message: Vec<u8>, signoff: &[u8]) -> Vec<u8> {
    // `signoff` is the full "Signed-off-by: name <email>" line (no newline).
    let signoff_line = {
        let mut line = signoff.to_vec();
        line.push(b'\n');
        line
    };
    // Find the earliest suffix made only of ignorable lines (comments,
    // blanks, and the old-style "Conflicts:" block — git's
    // `ignore_non_trailer`); the trailer is inserted ahead of it.
    let mut body_end = message.len();
    {
        let mut candidate = None;
        let mut offset = 0;
        let mut in_conflicts = false;
        for line in message.split_inclusive(|&b| b == b'\n') {
            let stripped: &[u8] = if line.ends_with(b"\n") {
                &line[..line.len() - 1]
            } else {
                line
            };
            let is_commentish = line.first() == Some(&b'#') || stripped.is_empty();
            let is_conflicts_head = stripped == b"Conflicts:";
            let is_conflicts_entry = in_conflicts && line.first() == Some(&b'\t');
            if is_commentish || is_conflicts_head || is_conflicts_entry {
                if candidate.is_none() {
                    candidate = Some(offset);
                }
                if is_conflicts_head {
                    in_conflicts = true;
                } else if !is_conflicts_entry {
                    in_conflicts = false;
                }
            } else {
                candidate = None;
                in_conflicts = false;
            }
            offset += line.len();
        }
        if let Some(pos) = candidate {
            body_end = pos;
        }
    }
    let (body, tail) = message.split_at(body_end);
    if body
        .windows(signoff_line.len())
        .any(|window| window == signoff_line.as_slice())
    {
        return message;
    }
    let mut out = body.to_vec();
    // git's `append_signoff` newline handling (sequencer.c): complete the line,
    // then pad so the sign-off sits below a blank line. The empty-buffer case
    // gets two newlines so the editor template leaves room for a title + body —
    // this is what makes `commit -s --allow-empty-message` produce "\n\nS-o-b\n".
    if !out.is_empty() && !out.ends_with(b"\n") {
        out.push(b'\n');
    }
    if !has_conforming_footer(body) {
        let len = out.len();
        if len == 0 {
            out.extend_from_slice(b"\n\n");
        } else if len == 1 {
            out.push(b'\n');
        } else if out[len - 2] != b'\n' {
            out.push(b'\n');
        }
        // else: already ends with two newlines — nothing to add.
    }
    out.extend_from_slice(&signoff_line);
    out.extend_from_slice(tail);
    out
}

/// Format the revert commit message (`sequencer_format_revert_message`).
fn format_revert_message(
    ctx: &ReplayCtx,
    db: &FileObjectDatabase,
    item: &TodoItem,
    commit: &Commit,
    subject: &str,
    parent: Option<&ObjectId>,
    opts: &ReplayOpts,
) -> Result<Vec<u8>> {
    let mut message = String::new();
    if opts.commit_use_reference {
        let comment = comment_char(&ctx.git_dir) as char;
        message.push_str(&format!(
            "{comment} *** SAY WHY WE ARE REVERTING ON THE TITLE LINE ***\n"
        ));
    } else if let Some(orig) = subject.strip_prefix("Revert \"")
        && !orig.starts_with("Revert \"")
    {
        message.push_str("Reapply \"");
        message.push_str(orig);
        message.push('\n');
    } else {
        message.push_str(&format!("Revert \"{subject}\"\n"));
    }
    message.push_str("\nThis reverts commit ");
    message.push_str(&refer_to_commit(ctx, db, &item.oid, opts)?);
    if commit.parents.len() > 1
        && let Some(parent) = parent
    {
        message.push_str(", reversing\nchanges made to ");
        message.push_str(&refer_to_commit(ctx, db, parent, opts)?);
    }
    message.push_str(".\n");
    Ok(message.into_bytes())
}

fn refer_to_commit(
    ctx: &ReplayCtx,
    db: &FileObjectDatabase,
    oid: &ObjectId,
    opts: &ReplayOpts,
) -> Result<String> {
    if !opts.commit_use_reference {
        return Ok(oid.to_hex());
    }
    let object = db.read_object(oid)?;
    let commit = Commit::parse(ctx.format, &object.body)?;
    let subject = commit_subject(&commit.message);
    let date = commit_identity_date(&commit.author, &DateMode::Short);
    Ok(format!(
        "{} ({subject}, {date})",
        format_log_abbrev_oid(oid)
    ))
}

/// Refuse the replay when applying the result would clobber local
/// modifications or untracked files (unpack_trees' verify steps).
fn verify_worktree_safe(
    ctx: &ReplayCtx,
    ours_map: &MergeTreeMap,
    results: &BTreeMap<Vec<u8>, MergePathResult>,
) -> std::result::Result<(), ReplayHalt> {
    let mut local_changes: Vec<Vec<u8>> = Vec::new();
    let mut untracked: Vec<Vec<u8>> = Vec::new();
    for (path, result) in results {
        let target: Option<(u32, ObjectId)> = match result {
            MergePathResult::Resolved(entry) => *entry,
            MergePathResult::Conflict { .. } => None,
        };
        let changes = match result {
            MergePathResult::Resolved(entry) => ours_map.get(path) != entry.as_ref(),
            MergePathResult::Conflict { .. } => true,
        };
        if !changes {
            continue;
        }
        let Ok(rel) = std::str::from_utf8(path) else {
            continue;
        };
        let full = ctx.worktree_root.join(rel);
        match ours_map.get(path) {
            Some((_, ours_oid)) => {
                // Tracked: the on-disk content must match ours' blob.
                let Ok(bytes) = fs::read(&full) else {
                    continue;
                };
                let on_disk = sley_core::object_id_for_bytes(ctx.format, "blob", &bytes)
                    .map_err(print_fatal_error)?;
                if &on_disk != ours_oid {
                    local_changes.push(path.clone());
                }
            }
            None => {
                // A gitlink target (mode 160000) is a submodule directory, not an
                // untracked file to preserve. git's unpack-trees `verify_absent_1`
                // routes a gitlink/submodule entry to `check_submodule_move_head`
                // instead of `check_ok_to_remove`, so a directory in the way of a
                // *new* gitlink never trips the untracked-overwrite refusal (the
                // `revert "Replace sub1 with directory"` case: the on-disk `sub1`
                // dir holds tracked files being removed by this same apply, and the
                // gitlink is materialized in their place). Skip the untracked check
                // for gitlink targets exactly as git skips `check_ok_to_remove`.
                if matches!(target, Some((mode, _)) if sley_index::is_gitlink(mode)) {
                    continue;
                }
                // Untracked file in the way of a new path.
                let would_write =
                    target.is_some() || matches!(result, MergePathResult::Conflict { .. });
                if would_write && full.exists() {
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
        return Err(ReplayHalt::Fatal);
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
        return Err(ReplayHalt::Fatal);
    }
    Ok(())
}

/// Apply merge results: update the index (preserving cached stat data for
/// unchanged entries) and the worktree files that changed.
fn apply_merge_results_to_index_and_worktree(
    ctx: &ReplayCtx,
    db: &FileObjectDatabase,
    ours_map: &MergeTreeMap,
    results: &BTreeMap<Vec<u8>, MergePathResult>,
) -> Result<()> {
    let index_path = sley_worktree::repository_index_path(&ctx.git_dir);
    let old_index = if index_path.exists() {
        Index::parse(&fs::read(&index_path)?, ctx.format)?
    } else {
        Index {
            version: 2,
            entries: Vec::new(),
            extensions: Vec::new(),
            checksum: None,
        }
    };
    let mut old_entries: BTreeMap<Vec<u8>, IndexEntry> = BTreeMap::new();
    for entry in &old_index.entries {
        if index_entry_stage(entry) == 0 {
            old_entries.insert(entry.path.clone().into_bytes(), entry.clone());
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
                    entries.push(crate::commands::merge_rebase::merge_index_entry(
                        path, *mode, *oid, 0,
                    ));
                }
            }
            MergePathResult::Resolved(None) => {}
            MergePathResult::Conflict {
                base, ours, theirs, ..
            } => {
                if let Some((mode, oid)) = base {
                    entries.push(crate::commands::merge_rebase::merge_index_entry(
                        path, *mode, *oid, 1,
                    ));
                }
                if let Some((mode, oid)) = ours {
                    entries.push(crate::commands::merge_rebase::merge_index_entry(
                        path, *mode, *oid, 2,
                    ));
                }
                if let Some((mode, oid)) = theirs {
                    entries.push(crate::commands::merge_rebase::merge_index_entry(
                        path, *mode, *oid, 3,
                    ));
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
        &index_path,
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
                    // A gitlink (submodule) entry's oid is a *commit* recorded in
                    // the submodule, NOT a blob in the superproject ODB — reading
                    // it as a blob fails ("not found"/"expected blob"). git's
                    // entry.c gitlink arm never reads object content: it only
                    // `mkdir`s the submodule dir. `merge_write_worktree_file`
                    // ignores `content` for a gitlink mode (it materializes the
                    // directory), so pass empty content and skip the ODB read.
                    let content = if sley_index::is_gitlink(*mode) {
                        Vec::new()
                    } else {
                        crate::commands::merge_rebase::merge_read_blob(db, oid)?
                    };
                    crate::commands::merge_rebase::merge_write_worktree_file(
                        &ctx.worktree_root,
                        path,
                        &content,
                        *mode,
                    )?;
                }
            }
            MergePathResult::Resolved(None) => {
                if ours_map.contains_key(path) {
                    crate::commands::merge_rebase::merge_remove_worktree_file(
                        &ctx.worktree_root,
                        path,
                    )?;
                }
            }
            MergePathResult::Conflict { worktree, .. } => match worktree {
                Some((mode, content)) => crate::commands::merge_rebase::merge_write_worktree_file(
                    &ctx.worktree_root,
                    path,
                    content,
                    *mode,
                )?,
                None => crate::commands::merge_rebase::merge_remove_worktree_file(
                    &ctx.worktree_root,
                    path,
                )?,
            },
        }
    }
    Ok(())
}

/// `append_conflicts_hint`: the commented `Conflicts:` block (with the
/// scissors cut line first under `--cleanup=scissors`).
fn append_conflicts_hint(message: &mut Vec<u8>, conflicts: &[Vec<u8>], cleanup: Option<&str>) {
    if !message.ends_with(b"\n") {
        message.push(b'\n');
    }
    if cleanup == Some("scissors") {
        message.push(b'\n');
        message.extend_from_slice(b"# ------------------------ >8 ------------------------\n");
        message.extend_from_slice(b"# Do not modify or remove the line above.\n");
        message.extend_from_slice(b"# Everything below it will be ignored.\n");
        message.extend_from_slice(b"#\n");
        message.extend_from_slice(b"# Conflicts:\n");
    } else {
        message.push(b'\n');
        message.extend_from_slice(b"# Conflicts:\n");
    }
    for path in conflicts {
        message.extend_from_slice(b"#\t");
        message.extend_from_slice(path);
        message.push(b'\n');
    }
}

fn print_conflict_advice(ctx: &ReplayCtx, opts: &ReplayOpts, help_msg: Option<&str>) {
    if !config_bool(&ctx.git_dir, "advice", "mergeConflict").unwrap_or(true) {
        return;
    }
    let lines: Vec<String> = if let Some(msg) = help_msg {
        msg.lines().map(str::to_string).collect()
    } else if opts.no_commit {
        vec![
            "after resolving the conflicts, mark the corrected paths".to_string(),
            "with 'git add <paths>' or 'git rm <paths>'".to_string(),
        ]
    } else {
        let me = ctx.action.name();
        vec![
            "After resolving the conflicts, mark them with".to_string(),
            "\"git add/rm <pathspec>\", then run".to_string(),
            format!("\"git {me} --continue\"."),
            format!("You can instead skip this commit with \"git {me} --skip\"."),
            format!("To abort and get back to the state before \"git {me}\","),
            format!("run \"git {me} --abort\"."),
        ]
    };
    for line in lines {
        eprintln!("hint: {line}");
    }
    eprintln!("hint: Disable this message with \"git config set advice.mergeConflict false\"");
}

/// stderr block `git commit` prints when a pick resolves to nil (the
/// `empty_cherry_pick_advice` + single-pick variant; the whence probe in git
/// always reports the single variant for non-rebase picks).
fn print_empty_halt_advice(ctx: &ReplayCtx) {
    eprintln!("The previous cherry-pick is now empty, possibly due to conflict resolution.");
    eprintln!("If you wish to commit it anyway, use:");
    eprintln!();
    eprintln!("    git commit --allow-empty");
    eprintln!();
    let me = ctx.action.name();
    eprintln!("Otherwise, please use 'git {me} --skip'");
}

fn fast_forward_to(ctx: &ReplayCtx, target: &ObjectId, head: Option<&ObjectId>) -> Result<()> {
    let refs = ctx.refs();
    let target_ref = match refs.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(branch)) => branch,
        _ => "HEAD".to_string(),
    };
    let committer = commit_identity_from_env("COMMITTER")?;
    let old_oid = head.copied().unwrap_or_else(|| ObjectId::null(ctx.format));
    let mut tx = refs.transaction();
    tx.update(RefUpdate {
        name: target_ref,
        expected: head.map(|oid| RefTarget::Direct(*oid)),
        new: RefTarget::Direct(*target),
        reflog: Some(ReflogEntry {
            old_oid,
            new_oid: *target,
            committer,
            message: format!("{}: fast-forward", ctx.action.name()).into_bytes(),
        }),
    });
    tx.commit()?;
    sley_worktree::reset_index_and_worktree_to_commit(
        &ctx.worktree_root,
        &ctx.git_dir,
        ctx.format,
        target,
    )?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn commit_and_advance_head(
    ctx: &ReplayCtx,
    tree: &ObjectId,
    head: Option<&ObjectId>,
    author: Vec<u8>,
    committer: Vec<u8>,
    message: &[u8],
    reflog_message: Vec<u8>,
) -> Result<ObjectId> {
    let mut db = ctx.db();
    let new_oid = sley_sequencer::create_commit(
        &mut db,
        sley_sequencer::CommitCreate {
            tree: *tree,
            parents: head.iter().map(|oid| **oid).collect(),
            author,
            committer: committer.clone(),
            message: message.to_vec(),
            encoding: None,
        },
    )?;
    let refs = ctx.refs();
    let target_ref = match refs.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(branch)) => branch,
        _ => "HEAD".to_string(),
    };
    let old_oid = head.copied().unwrap_or_else(|| ObjectId::null(ctx.format));
    let mut tx = refs.transaction();
    tx.update(RefUpdate {
        name: target_ref,
        expected: head.map(|oid| RefTarget::Direct(*oid)),
        new: RefTarget::Direct(new_oid),
        reflog: Some(ReflogEntry {
            old_oid,
            new_oid,
            committer,
            message: reflog_message,
        }),
    });
    tx.commit()?;
    Ok(new_oid)
}

fn print_commit_summary_line(ctx: &ReplayCtx, oid: &ObjectId, message: &[u8]) {
    let refs = ctx.refs();
    let location = match refs.read_ref("HEAD") {
        Ok(Some(RefTarget::Symbolic(branch))) => branch
            .strip_prefix("refs/heads/")
            .unwrap_or(&branch)
            .to_string(),
        _ => "detached HEAD".to_string(),
    };
    let subject = commit_subject(message);
    println!("[{location} {}] {subject}", format_log_abbrev_oid(oid));
}

// ---------------------------------------------------------------------------
// --continue / --skip / --abort / --quit
// ---------------------------------------------------------------------------

fn sequencer_continue(ctx: &ReplayCtx) -> Result<()> {
    let opts = replay::read_opts(&ctx.git_dir).map_err(|err| {
        eprintln!("error: {err}");
        fatal_failed(ctx.action)
    })?;
    if !replay::todo_path(&ctx.git_dir).exists() {
        return continue_single_pick(ctx, &opts).map(|_| ());
    }
    let items = read_populate_todo(ctx)?;
    // Conflict resolution pending: commit it first.
    if ctx.git_dir.join("CHERRY_PICK_HEAD").exists() || ctx.git_dir.join("REVERT_HEAD").exists() {
        continue_single_pick(ctx, &opts)?;
    }
    // The stopped item is concluded; replay the rest.
    pick_commits(ctx, &opts, &items[1.min(items.len())..])
}

fn read_populate_todo(ctx: &ReplayCtx) -> Result<Vec<TodoItem>> {
    let todo_path = replay::todo_path(&ctx.git_dir);
    let text = fs::read_to_string(&todo_path).map_err(|err| {
        eprintln!("error: could not read '{}': {err}", todo_path.display());
        fatal_failed(ctx.action)
    })?;
    let unusable = |line_errors: &[String]| {
        for error in line_errors {
            eprintln!("error: {error}");
        }
        eprintln!(
            "error: unusable instruction sheet: '{}'",
            todo_path.display()
        );
        fatal_failed(ctx.action)
    };
    let parsed = match replay::parse_todo(&text) {
        Ok(parsed) => parsed,
        Err(err) => return Err(unusable(&err.line_errors)),
    };
    if parsed.is_empty() {
        eprintln!("error: no commits parsed.");
        return Err(fatal_failed(ctx.action));
    }
    let mut items = Vec::with_capacity(parsed.len());
    for (idx, line) in parsed.iter().enumerate() {
        if line.action != ctx.action {
            let message = match ctx.action {
                ReplayAction::Pick => "cannot cherry-pick during a revert.",
                ReplayAction::Revert => "cannot revert during a cherry-pick.",
            };
            eprintln!("error: {message}");
            return Err(fatal_failed(ctx.action));
        }
        let oid = match resolve_revision(&ctx.git_dir, ctx.format, &line.object_name) {
            Ok(oid) => oid,
            Err(_) => {
                let errors = vec![
                    format!("could not parse '{}'", line.object_name),
                    format!(
                        "invalid line {}: {} {} {}",
                        idx + 1,
                        line.action.command(),
                        line.object_name,
                        line.rest
                    ),
                ];
                return Err(unusable(&errors));
            }
        };
        items.push(TodoItem {
            action: line.action,
            oid,
            display: if line.rest.is_empty() {
                line.object_name.clone()
            } else {
                format!("{} {}", line.object_name, line.rest)
            },
        });
    }
    Ok(items)
}

/// `continue_single_pick`: conclude the stopped pick by committing the staged
/// resolution (the inline equivalent of the `git commit` child).
fn continue_single_pick(ctx: &ReplayCtx, opts: &ReplayOpts) -> Result<()> {
    let cph = ctx.git_dir.join("CHERRY_PICK_HEAD");
    let rvh = ctx.git_dir.join("REVERT_HEAD");
    if !cph.exists() && !rvh.exists() {
        eprintln!("error: no cherry-pick or revert in progress");
        return Err(fatal_failed(ctx.action));
    }
    // Unmerged files leave the commit impossible (the `git commit` child's
    // error block, exit 128).
    let index_path = sley_worktree::repository_index_path(&ctx.git_dir);
    if let Ok(bytes) = fs::read(&index_path) {
        let index = Index::parse(&bytes, ctx.format)?;
        let unmerged: Vec<String> = index
            .entries
            .iter()
            .filter(|entry| index_entry_stage(entry) > 0)
            .map(|entry| entry.path.to_string())
            .collect();
        if !unmerged.is_empty() {
            for path in unmerged.iter().collect::<BTreeSet<_>>() {
                println!("U\t{path}");
            }
            eprintln!("error: Committing is not possible because you have unmerged files.");
            eprintln!("hint: Fix them up in the work tree, and then use 'git add/rm <file>'");
            eprintln!("hint: as appropriate to mark resolution and make a commit.");
            eprintln!("fatal: Exiting because of an unresolved conflict.");
            return Err(GitError::Exit(128));
        }
    }
    let head = ctx.head_oid();
    let db = ctx.db();
    let head_tree = match &head {
        Some(oid) => commit_tree_oid(&db, ctx.format, oid)?,
        None => ObjectId::empty_tree(ctx.format),
    };
    let index_tree = index_tree_oid(ctx)?;
    if index_tree == head_tree {
        // Resolved to nil: print the empty advice and stop (exit 1).
        print_empty_halt_advice(ctx);
        return Err(GitError::Exit(1));
    }
    // Message from MERGE_MSG with comments stripped (--cleanup=strip).
    let raw = fs::read(ctx.git_dir.join("MERGE_MSG")).unwrap_or_default();
    let mut message = strip_comment_lines(&raw, comment_char(&ctx.git_dir));
    let edit = opts.edit == Some(true);
    if edit {
        let path = ctx.git_dir.join("COMMIT_EDITMSG");
        fs::write(&path, &message)?;
        launch_editor(&ctx.git_dir, &path)?;
        message = strip_comment_lines(&fs::read(&path)?, comment_char(&ctx.git_dir));
    }
    // Author: the picked commit's author for cherry-picks; env for reverts.
    let author = if cph.exists() {
        let text = fs::read_to_string(&cph)?;
        let oid = ObjectId::from_hex(ctx.format, text.trim())?;
        let object = db.read_object(&oid)?;
        Commit::parse(ctx.format, &object.body)?.author
    } else {
        commit_identity_from_env("AUTHOR")?
    };
    let committer = commit_identity_from_env("COMMITTER")?;
    let subject = commit_subject(&message);
    let new_oid = commit_and_advance_head(
        ctx,
        &index_tree,
        head.as_ref(),
        author,
        committer,
        &message,
        format!("commit: {subject}").into_bytes(),
    )?;
    let _ = fs::remove_file(&cph);
    let _ = fs::remove_file(&rvh);
    let _ = fs::remove_file(ctx.git_dir.join("MERGE_MSG"));
    print_commit_summary_line(ctx, &new_oid, &message);
    Ok(())
}

fn sequencer_skip(ctx: &ReplayCtx) -> Result<()> {
    let last = replay::last_command(&ctx.git_dir);
    let state_file = ctx.git_dir.join(ctx.action.head_file());
    if !state_file.exists() {
        if last != Some(ctx.action) {
            eprintln!("error: no {} in progress", ctx.action.name());
            return Err(fatal_failed(ctx.action));
        }
        let head = ctx.head_oid();
        if !replay::rollback_is_safe(&ctx.git_dir, head.as_ref()) {
            eprintln!("error: there is nothing to skip");
            if config_bool(&ctx.git_dir, "advice", "resolveConflict").unwrap_or(true) {
                eprintln!("hint: have you committed already?");
                eprintln!("hint: try \"git {} --continue\"", ctx.action.name());
            }
            return Err(fatal_failed(ctx.action));
        }
    }
    // `git reset --merge HEAD` (an unborn HEAD resets to the empty tree).
    let head = ctx.head_oid();
    reset_merge_target(ctx, head.as_ref()).map_err(|_| {
        eprintln!("error: failed to skip the commit");
        fatal_failed(ctx.action)
    })?;
    if !replay::seq_dir(&ctx.git_dir).is_dir() {
        return Ok(());
    }
    sequencer_continue_after_skip(ctx)
}

/// Continue after a skip: like `sequencer_continue` but the stopped item is
/// dropped without committing.
fn sequencer_continue_after_skip(ctx: &ReplayCtx) -> Result<()> {
    let opts = replay::read_opts(&ctx.git_dir).map_err(|err| {
        eprintln!("error: {err}");
        fatal_failed(ctx.action)
    })?;
    let items = read_populate_todo(ctx)?;
    pick_commits(ctx, &opts, &items[1.min(items.len())..])
}

fn sequencer_rollback(ctx: &ReplayCtx) -> Result<()> {
    let head_file = replay::head_path(&ctx.git_dir);
    if !head_file.exists() {
        // Single-pick abort.
        if !ctx.git_dir.join("CHERRY_PICK_HEAD").exists()
            && !ctx.git_dir.join("REVERT_HEAD").exists()
        {
            eprintln!("error: no cherry-pick or revert in progress");
            return Err(fatal_failed(ctx.action));
        }
        let Some(head) = ctx.head_oid() else {
            eprintln!("error: cannot abort from a branch yet to be born");
            return Err(fatal_failed(ctx.action));
        };
        return reset_merge(ctx, &head).map_err(|err| {
            eprintln!("error: {err}");
            fatal_failed(ctx.action)
        });
    }
    let text = fs::read_to_string(&head_file).map_err(|err| {
        eprintln!("error: cannot open '{}': {err}", head_file.display());
        fatal_failed(ctx.action)
    })?;
    let stored = text.trim();
    let oid = ObjectId::from_hex(ctx.format, stored).map_err(|_| {
        eprintln!(
            "error: stored pre-cherry-pick HEAD file '{}' is corrupt",
            head_file.display()
        );
        fatal_failed(ctx.action)
    })?;
    if oid == ObjectId::null(ctx.format) {
        eprintln!("error: cannot abort from a branch yet to be born");
        return Err(fatal_failed(ctx.action));
    }
    let head = ctx.head_oid();
    if !replay::rollback_is_safe(&ctx.git_dir, head.as_ref()) {
        eprintln!("warning: You seem to have moved HEAD. Not rewinding, check your HEAD!");
    } else {
        reset_merge(ctx, &oid).map_err(|err| {
            eprintln!("error: {err}");
            fatal_failed(ctx.action)
        })?;
    }
    replay::remove_state(&ctx.git_dir);
    Ok(())
}

/// `git reset --merge <oid>`: reset the index to the target tree and update
/// worktree files whose index entry changes, refusing to clobber paths whose
/// on-disk content diverges from the (stage-0) index. Conflicted paths are
/// reset outright. Clears the in-progress branch state on success.
fn reset_merge(ctx: &ReplayCtx, target: &ObjectId) -> Result<()> {
    reset_merge_in(&ctx.git_dir, &ctx.worktree_root, ctx.format, Some(target))
}

fn reset_merge_target(ctx: &ReplayCtx, target: Option<&ObjectId>) -> Result<()> {
    reset_merge_in(&ctx.git_dir, &ctx.worktree_root, ctx.format, target)
}

pub(crate) fn reset_merge_in(
    git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
    target: Option<&ObjectId>,
) -> Result<()> {
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let target_map = match target {
        Some(target) => {
            let target_tree = commit_tree_oid(&db, format, target)?;
            stash_tree_entry_map(&db, format, &target_tree)?
        }
        None => MergeTreeMap::new(),
    };
    let index_path = sley_worktree::repository_index_path(git_dir);
    let old_index = if index_path.exists() {
        Index::parse(&fs::read(&index_path)?, format)?
    } else {
        Index {
            version: 2,
            entries: Vec::new(),
            extensions: Vec::new(),
            checksum: None,
        }
    };
    let mut stage0: BTreeMap<Vec<u8>, IndexEntry> = BTreeMap::new();
    let mut conflicted: BTreeSet<Vec<u8>> = BTreeSet::new();
    for entry in &old_index.entries {
        let path = entry.path.clone().into_bytes();
        if index_entry_stage(entry) == 0 {
            stage0.insert(path, entry.clone());
        } else {
            conflicted.insert(path);
        }
    }

    // Plan: per path, decide keep / update / delete + verify safety.
    let mut errors: Vec<Vec<u8>> = Vec::new();
    let mut updates: Vec<(Vec<u8>, (u32, ObjectId))> = Vec::new();
    let mut deletions: Vec<Vec<u8>> = Vec::new();
    let all_paths: BTreeSet<Vec<u8>> = stage0
        .keys()
        .cloned()
        .chain(conflicted.iter().cloned())
        .chain(target_map.keys().cloned())
        .collect();
    for path in &all_paths {
        let target_entry = target_map.get(path);
        let conflict = conflicted.contains(path);
        let current = stage0.get(path);
        if conflict {
            // Conflicted entries are reset to the target unconditionally.
            match target_entry {
                Some(entry) => updates.push((path.clone(), *entry)),
                None => deletions.push(path.clone()),
            }
            continue;
        }
        let current_entry = current.map(|entry| (entry.mode, entry.oid));
        if current_entry.as_ref() == target_entry {
            continue;
        }
        if current.is_some_and(|entry| sley_index::is_gitlink(entry.mode))
            && target_entry.is_none()
            && target_map
                .keys()
                .any(|candidate| {
                    candidate.starts_with(path) && candidate.get(path.len()) == Some(&b'/')
                })
        {
            errors.push(path.clone());
            continue;
        }
        // The index entry changes: the worktree must match the index.
        if let Some(entry) = current {
            let rel = entry.path.to_string();
            let full = worktree_root.join(&rel);
            if let Ok(bytes) = fs::read(&full) {
                let on_disk = sley_core::object_id_for_bytes(format, "blob", &bytes)?;
                if on_disk != entry.oid {
                    errors.push(path.clone());
                    continue;
                }
            }
        } else if matches!(target_entry, Some((mode, _)) if sley_index::is_gitlink(*mode)) {
            let Some(rel) = std::str::from_utf8(path).ok() else {
                continue;
            };
            let full = worktree_root.join(rel);
            if fs::symlink_metadata(&full).is_ok_and(|metadata| !metadata.is_dir()) {
                errors.push(path.clone());
                continue;
            }
        }
        match target_entry {
            Some(entry) => updates.push((path.clone(), *entry)),
            None => deletions.push(path.clone()),
        }
    }
    if !errors.is_empty() {
        for path in &errors {
            eprintln!(
                "error: Entry '{}' not uptodate. Cannot merge.",
                String::from_utf8_lossy(path)
            );
        }
        let target_text = target
            .map(|oid| oid.to_hex())
            .unwrap_or_else(|| "HEAD".to_string());
        return Err(GitError::Command(format!(
            "Could not reset index file to revision '{target_text}'."
        )));
    }

    // Apply: rebuild the index from the target tree (preserving stat data
    // for unchanged entries), then touch only the planned worktree paths.
    let mut entries: Vec<IndexEntry> = Vec::new();
    for (path, (mode, oid)) in &target_map {
        if let Some(old) = stage0.get(path)
            && old.mode == *mode
            && old.oid == *oid
        {
            entries.push(old.clone());
        } else {
            entries.push(crate::commands::merge_rebase::merge_index_entry(
                path, *mode, *oid, 0,
            ));
        }
    }
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    let mut index = Index {
        version: 2,
        entries,
        extensions: Vec::new(),
        checksum: None,
    };
    index.upgrade_version_for_flags();
    fs::write(&index_path, index.write(format)?)?;
    for (path, (mode, oid)) in &updates {
        let content = if sley_index::is_gitlink(*mode) {
            Vec::new()
        } else {
            crate::commands::merge_rebase::merge_read_blob(&db, oid)?
        };
        crate::commands::merge_rebase::merge_write_worktree_file(
            worktree_root,
            path,
            &content,
            *mode,
        )?;
    }
    for path in &deletions {
        crate::commands::merge_rebase::merge_remove_worktree_file(worktree_root, path)?;
    }
    sley_worktree::refresh_index_paths(
        worktree_root,
        git_dir,
        format,
        &[],
        /* quiet */ true,
        /* ignore_missing */ true,
        /* really_refresh */ false,
    )?;

    // Move HEAD (branch tip or detached) when the target differs.
    let refs = FileRefStore::new(git_dir, format);
    let current_head = head_commit_oid(&refs)?;
    if let Some(target) = target
        && current_head.as_ref() != Some(target)
    {
        let target_ref = match refs.read_ref("HEAD")? {
            Some(RefTarget::Symbolic(branch)) => branch,
            _ => "HEAD".to_string(),
        };
        let committer = commit_identity_from_env("COMMITTER")?;
        let old_oid = current_head.unwrap_or_else(|| ObjectId::null(format));
        let mut tx = refs.transaction();
        tx.update(RefUpdate {
            name: target_ref,
            expected: current_head.map(RefTarget::Direct),
            new: RefTarget::Direct(*target),
            reflog: Some(ReflogEntry {
                old_oid,
                new_oid: *target,
                committer,
                message: b"reset: moving to target".to_vec(),
            }),
        });
        tx.commit()?;
    }

    sley_sequencer::replay::remove_branch_state(git_dir);
    Ok(())
}
