//! `git cherry-pick` / `git revert` — porcelains over the sequencer state
//! machine in [`sley_sequencer::replay`].
//!
//! The state machine (todo/opts/head/abort-safety files, CHERRY_PICK_HEAD /
//! REVERT_HEAD lifecycle) lives in the library crate; this module owns the
//! drive loop: option parsing, the rev walk that populates the instruction
//! sheet, the per-commit 3-way replay, and `--continue` / `--abort` /
//! `--skip` / `--quit`.
#![allow(clippy::expect_used)]

use crate::commands::merge_rebase::{MergeTreeMap, commit_tree_oid};
use crate::*;
use sley_sequencer::replay::{self, ReplayAction, ReplayOpts};

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

pub(crate) fn cmd_cherry_pick(
    cli_session: &crate::session::CliSession,
    args: &[String],
) -> Result<()> {
    run_replay(cli_session, ReplayAction::Pick, args)
}

pub(crate) fn cmd_revert(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
    run_replay(cli_session, ReplayAction::Revert, args)
}

pub(crate) fn cmd_replay(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
    run_git_replay(cli_session, args)
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

fn run_git_replay(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
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

    let cwd = cli_session.cwd().to_path_buf();
    let git_dir = cli_session.git_dir()?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let format = repository_object_format(&common_git_dir)?;
    let worktree_root =
        sley_worktree::worktree_root_for_git_dir(&git_dir)?.unwrap_or_else(|| cwd.clone());
    let config = read_repo_config(&git_dir)?;
    let config = crate::commands::merge_rebase::effective_config_with_overrides(&config);
    let db =
        crate::repository::open_object_database(&git_dir, format, cli_session.replace_objects())?;
    let ctx = sley_sequencer::pick::PickContext {
        action: if parsed.revert.is_some() {
            ReplayAction::Revert
        } else {
            ReplayAction::Pick
        },
        git_dir,
        worktree_root,
        format,
        config,
        replace_objects: cli_session.replace_objects(),
        db,
    };
    let hosts = replay_hosts(cli_session.lazy_fetch());
    let plan = build_git_replay_plan(&ctx, parsed)?;
    let new_oid = replay_commits_to_base(&ctx, &hosts, &plan)?;
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

fn build_git_replay_plan(
    ctx: &sley_sequencer::pick::PickContext,
    parsed: GitReplayArgs,
) -> Result<GitReplayPlan> {
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
        None => match config_value(&ctx.config, "replay", "refAction").as_deref() {
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
            let base = resolve_revision(&ctx.git_dir, ctx.format, onto, ctx.replace_objects)
                .map_err(|_| {
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
    ctx: &sley_sequencer::pick::PickContext,
    action: ReplayAction,
    rev_args: &[String],
    base: &ObjectId,
) -> Result<Vec<ObjectId>> {
    if action == ReplayAction::Pick
        && rev_args.len() == 1
        && !rev_args[0].contains("..")
        && !rev_args[0].starts_with('^')
    {
        let tip = resolve_revision(&ctx.git_dir, ctx.format, &rev_args[0], ctx.replace_objects)?;
        let db = ctx.db.clone();
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
    let hosts = replay_hosts(false);
    sley_sequencer::pick::select_commits(ctx, &hosts, action, rev_args)
}

fn replay_commits_to_base(
    ctx: &sley_sequencer::pick::PickContext,
    hosts: &sley_sequencer::pick::PickHosts<'_>,
    plan: &GitReplayPlan,
) -> Result<ObjectId> {
    if plan.commits.is_empty() {
        return Ok(plan.base);
    }
    let mut head = plan.base;
    for oid in &plan.commits {
        head = replay_one_commit_to(ctx, hosts, plan.action, &head, oid)?;
    }
    Ok(head)
}

fn replay_one_commit_to(
    ctx: &sley_sequencer::pick::PickContext,
    hosts: &sley_sequencer::pick::PickHosts<'_>,
    action: ReplayAction,
    head: &ObjectId,
    oid: &ObjectId,
) -> Result<ObjectId> {
    let db = ctx.db.clone();
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
                Some(parent) => tree_map_of_commit_or_halt(ctx, &db, &parent)?,
                None => MergeTreeMap::new(),
            };
            let theirs = sley_diff_merge::flatten_tree(&db, ctx.format, &commit.tree)?;
            (base, theirs)
        }
        ReplayAction::Revert => {
            let base = sley_diff_merge::flatten_tree(&db, ctx.format, &commit.tree)?;
            let theirs = match parent {
                Some(parent) => tree_map_of_commit_or_halt(ctx, &db, &parent)?,
                None => MergeTreeMap::new(),
            };
            (base, theirs)
        }
    };
    let head_tree = commit_tree_oid(&db, ctx.format, head)?;
    let ours_map = sley_diff_merge::flatten_tree(&db, ctx.format, &head_tree)?;
    let (results, conflicts) =
        sley_sequencer::apply::three_way_merge_trees_styled_with_strategy_options(
            &db,
            &ctx.config,
            ctx.format,
            &base_map,
            &ours_map,
            &theirs_map,
            "HEAD",
            &format_log_abbrev_oid(oid),
            "parent",
            sley_diff_merge::ConflictStyle::Merge,
            &[],
            hosts.promisor_fetch,
        )
        .map_err(|err| {
            eprintln!("error: {err}");
            GitError::Exit(128)
        })?;
    if !conflicts.is_empty() {
        return Err(GitError::Exit(1));
    }
    let tree_map = sley_sequencer::pick::merge_results_to_tree_map(&results);
    let new_tree = write_tree_map_object(&db, ctx.format, &tree_map)?;
    if new_tree == head_tree {
        return Ok(*head);
    }
    let author = match action {
        ReplayAction::Pick => commit.author.clone(),
        ReplayAction::Revert => commit_identity_from_env("AUTHOR", &ctx.config)?,
    };
    let message = match action {
        ReplayAction::Pick => commit.message.clone(),
        ReplayAction::Revert => sley_sequencer::pick::format_revert_message(
            &db,
            ctx.format,
            &ctx.git_dir,
            &sley_sequencer::pick::make_todo_item(&db, ctx.format, ReplayAction::Revert, oid)?,
            &commit,
            &commit_subject(&commit.message),
            parent.as_ref(),
            &ReplayOpts::default(),
        )?,
    };
    let mut writer = ctx.db.clone();
    sley_sequencer::create_commit(
        &mut writer,
        sley_sequencer::CommitCreate {
            tree: new_tree,
            parents: vec![*head],
            author,
            committer: commit_identity_from_env("COMMITTER", &ctx.config)?,
            message,
            encoding: commit.encoding,
            signature: None,
        },
    )
}

/// Local tree-map helper for the `git replay` plumbing command; errors print
/// and halt with exit 128 exactly as the engine's `print_fatal_error` path.
fn tree_map_of_commit_or_halt(
    ctx: &sley_sequencer::pick::PickContext,
    db: &FileObjectDatabase,
    oid: &ObjectId,
) -> Result<MergeTreeMap> {
    let tree = commit_tree_oid(db, ctx.format, oid).map_err(|err| {
        eprintln!("error: {err}");
        GitError::Exit(128)
    })?;
    sley_diff_merge::flatten_tree(db, ctx.format, &tree).map_err(|err| {
        eprintln!("error: {err}");
        GitError::Exit(128)
    })
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
    ctx: &sley_sequencer::pick::PickContext,
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
            committer: commit_identity_from_env("COMMITTER", &ctx.config)?,
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
    let mut iter = args.iter();
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

// The `git replay` plumbing command runs on `sley_sequencer::pick::PickContext`
// too; its extra lazy-fetch flag travels through the host bundle instead.

/// Partial-clone hydration adapter handed to the sequencer engine.
struct CliPromisorPrefetch;

impl sley_sequencer::apply::PromisorObjectFetch for CliPromisorPrefetch {
    fn read_object_maybe_prefetch(
        &self,
        db: &FileObjectDatabase,
        oid: &ObjectId,
    ) -> Result<std::sync::Arc<EncodedObject>> {
        read_object_maybe_prefetch_promisor(db, oid, true)
    }
}

fn run_replay(
    cli_session: &crate::session::CliSession,
    action: ReplayAction,
    args: &[String],
) -> Result<()> {
    let parsed = parse_replay_args(action, args)?;
    let git_dir = cli_session.git_dir()?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let format = repository_object_format(&common_git_dir)?;
    let worktree_root = worktree_root_for_git_dir(cli_session, &git_dir)?;
    let config = read_repo_config(&git_dir)?;
    let config = crate::commands::merge_rebase::effective_config_with_overrides(&config);
    let db =
        crate::repository::open_object_database(&git_dir, format, cli_session.replace_objects())?;
    let ctx = sley_sequencer::pick::PickContext {
        action,
        git_dir,
        worktree_root,
        format,
        config,
        replace_objects: cli_session.replace_objects(),
        db,
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
            CmdMode::Continue => {
                let mut hosts = replay_hosts(cli_session.lazy_fetch());
                sley_sequencer::pick::continue_sequence(&ctx, &mut hosts)
            }
            CmdMode::Abort => {
                let hosts = replay_hosts(cli_session.lazy_fetch());
                sley_sequencer::pick::rollback(&ctx, &hosts)
            }
            CmdMode::Skip => {
                let mut hosts = replay_hosts(cli_session.lazy_fetch());
                sley_sequencer::pick::skip_sequence(&ctx, &mut hosts)
            }
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
        && config_bool(&ctx.config, "revert", "reference").unwrap_or(false)
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
    let mut hosts = replay_hosts(cli_session.lazy_fetch());
    sley_sequencer::pick::pick_revisions(&ctx, &mut hosts, &opts, &parsed.rev_args)
}

/// Assemble the host services (editor/hook runs, trailer recognition,
/// partial-clone hydration) for one invocation. All seams are plain static
/// functions, so the bundle carries no borrows.
fn replay_hosts(lazy_fetch: bool) -> sley_sequencer::pick::PickHosts<'static> {
    static PREFETCH: CliPromisorPrefetch = CliPromisorPrefetch;
    sley_sequencer::pick::PickHosts {
        prepare_commit_message: &|git_dir, message, source_merge, edit| {
            prepare_replay_host_message(git_dir, message, source_merge, edit)
        },
        append_signoff: &|message, signoff| append_signoff_before_comments(message, signoff),
        has_conforming_trailer_block: &|config, text| {
            commands::interpret_trailers::message_has_conforming_trailer_block(config, text)
        },
        promisor_fetch: if lazy_fetch { Some(&PREFETCH) } else { None },
        usage_error: &|action| usage_error(action),
    }
}

/// Host implementation of the pick engine's message-preparation seam:
/// `.git/COMMIT_EDITMSG` + prepare-commit-msg hook (+ editor + commit-msg
/// hook when editing), returning the cleaned message.
fn prepare_replay_host_message(
    git_dir: &Path,
    message: Vec<u8>,
    source_merge: bool,
    edit: bool,
) -> Result<Vec<u8>> {
    let source = if source_merge {
        commands::commit::PrepareCommitMsgSource::Merge
    } else {
        commands::commit::PrepareCommitMsgSource::Message
    };
    prepare_commit_message_at(git_dir, &message, source, edit, !edit)
}
/// Run prepare-commit-msg over `.git/COMMIT_EDITMSG`, optionally launch the
/// editor, and return the cleaned message.
fn prepare_commit_message_at(
    git_dir: &Path,
    message: &[u8],
    source: commands::commit::PrepareCommitMsgSource<'_>,
    edit: bool,
    set_no_editor_env: bool,
) -> Result<Vec<u8>> {
    let path = git_dir.join("COMMIT_EDITMSG");
    if edit {
        let comment = comment_char(git_dir);
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
    } else {
        fs::write(&path, message)?;
    }
    commands::commit::run_prepare_commit_msg_hook(
        git_dir,
        &path,
        source,
        Vec::new(),
        set_no_editor_env,
    )?;
    if edit {
        launch_editor(git_dir, &path)?;
        let path_arg = path.to_string_lossy().into_owned();
        commands::hooks::run_hook_l_at(git_dir, "commit-msg", &[path_arg.as_str()])?;
    }
    let edited = fs::read(&path)?;
    Ok(strip_comment_lines(&edited, comment_char(git_dir)))
}

fn config_bool(config: &GitConfig, section: &str, key: &str) -> Option<bool> {
    let value = config_value(config, section, key)?;
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" | "" => Some(true),
        "false" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

/// Effective-config lookup (repo + global/system + `-c`/env injection).
fn config_value(config: &GitConfig, section: &str, key: &str) -> Option<String> {
    config.get(section, None, key).map(str::to_string)
}

pub(crate) fn launch_editor(git_dir: &Path, path: &Path) -> Result<()> {
    let editor = env::var("GIT_EDITOR")
        .ok()
        .or_else(|| {
            let config = read_repo_config(git_dir).ok()?;
            crate::commands::merge_rebase::effective_config_with_overrides(&config)
                .get("core", None, "editor")
                .map(str::to_string)
        })
        .or_else(|| {
            env::var("VISUAL")
                .ok()
                .filter(|value| !value.is_empty())
                .filter(|_| env::var("TERM").is_ok_and(|term| term != "dumb"))
        })
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
    sley_sequencer::pick::comment_char(git_dir)
}
/// Historical CLI signature; the canonical implementation (and its
/// partial-clone hydration seam) lives in the sequencer.
pub(crate) fn reset_merge_in(
    git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
    target: Option<&ObjectId>,
    config: &GitConfig,
    lazy_fetch: bool,
) -> Result<()> {
    static PREFETCH: CliPromisorPrefetch = CliPromisorPrefetch;
    sley_sequencer::pick::reset_merge_in(
        git_dir,
        worktree_root,
        format,
        target,
        config,
        lazy_fetch.then_some(&PREFETCH as &'static dyn sley_sequencer::apply::PromisorObjectFetch),
    )
}

pub(crate) fn strip_comment_lines(message: &[u8], comment: u8) -> Vec<u8> {
    strip_comment_string_lines(message, &[comment])
}

pub(crate) fn strip_comment_string_lines(message: &[u8], comment: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(message.len());
    let mut blank_pending = false;
    for line in message.split_inclusive(|&b| b == b'\n') {
        if !comment.is_empty() && line.starts_with(comment) {
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

#[cfg(test)]
mod comment_cleanup_tests {
    use super::strip_comment_string_lines;

    #[test]
    fn strips_the_complete_multibyte_comment_prefix() {
        let message = b"A3\n\nCOMMENT generated help\nCOMMENT status\n\nedited\n";
        assert_eq!(
            strip_comment_string_lines(message, b"COMMENT"),
            b"A3\n\nedited\n"
        );
    }
}

/// `has_conforming_footer` (approximation): the last paragraph consists of
/// `Key: value` trailer lines.
/// Append a `Signed-off-by:` trailer ahead of any trailing comment block
/// (`append_signoff` + `ignored_log_message_bytes`).
///
/// `config` is consulted so `trailer.<name>.*` recognition matches git's
/// `has_conforming_footer` (configured tokens can make a mixed final paragraph
/// count as a trailer block, suppressing the blank line before the SOB).
pub(crate) fn append_signoff_before_comments(message: Vec<u8>, signoff: &[u8]) -> Vec<u8> {
    append_signoff_before_comments_with_config(message, signoff, None)
}

/// Like [`append_signoff_before_comments`] but with an explicit repo config so
/// trailer recognition honours `trailer.<name>.*`.
pub(crate) fn append_signoff_before_comments_with_config(
    message: Vec<u8>,
    signoff: &[u8],
    config: Option<&GitConfig>,
) -> Vec<u8> {
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
    let body_text = String::from_utf8_lossy(body);
    let has_footer =
        commands::interpret_trailers::message_has_conforming_trailer_block(config, &body_text);
    if !has_footer {
        let len = out.len();
        if len == 0 {
            out.extend_from_slice(b"\n\n");
        } else if len == 1 || out[len - 2] != b'\n' {
            out.push(b'\n');
        }
        // else: already ends with two newlines — nothing to add.
    }
    out.extend_from_slice(&signoff_line);
    out.extend_from_slice(tail);
    out
}
