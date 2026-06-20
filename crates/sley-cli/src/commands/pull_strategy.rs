//! Narrow `git pull -s <strategy>` support, routed from the dispatcher before
//! the general `pull` implementation sees the arguments.
//!
//! `cmd_pull` (commands::merge_rebase) intentionally rejects `-s`; the only
//! strategy that has a t-suite dependency today is the upstream t4013 setup's
//! `git pull -s ours --no-rebase . side` — a *local* pull whose merge keeps
//! HEAD's tree verbatim. Rather than widening the general pull machinery, this
//! module implements exactly that shape: resolve the named branch in the
//! current repository, write FETCH_HEAD/ORIG_HEAD the way git does, create a
//! merge commit whose tree is HEAD's tree, and advance the current branch.
//!
//! Anything outside the supported shape (a non-`ours` strategy, a remote that
//! is not the current repository, missing branch argument) is rejected with
//! the same kind of "unsupported" error the general pull path produces, so
//! behaviour only *grows* for the `-s ours` case.
use crate::*;

/// Returns `Some(index)` when `args` carries a `-s`/`--strategy` option, with
/// `index` pointing at the option itself (its value may live in the same token
/// after `=` or in the next token).
pub(crate) fn pull_has_strategy_option(args: &[String]) -> bool {
    args.iter()
        .any(|arg| arg == "-s" || arg == "--strategy" || arg.starts_with("--strategy="))
}

/// `git pull -s ours [--no-rebase] <path-to-self> <branch>` — the only
/// strategy-pull sley models. The dispatcher calls this when
/// `pull_has_strategy_option` fired; everything else stays on `cmd_pull`.
pub(crate) fn cmd_pull_with_strategy(args: &[String]) -> Result<()> {
    let mut strategy = None::<String>;
    let mut remote = None::<String>;
    let mut branch = None::<String>;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-s" | "--strategy" => {
                let Some(value) = iter.next() else {
                    return Err(GitError::Command("--strategy requires a value".into()));
                };
                strategy = Some(value.to_string());
            }
            value if value.starts_with("--strategy=") => {
                strategy = Some(value["--strategy=".len()..].to_string());
            }
            // The general pull options that do not change the ours-merge
            // outcome: an ours pull never touches the worktree, so the
            // rebase/ff knobs and quiet flags are accepted and ignored.
            "--no-rebase" | "--rebase=false" | "--no-ff" | "--ff" | "-q" | "--quiet"
            | "--no-quiet" => {}
            value if value.starts_with('-') => {
                return Err(GitError::Command(format!(
                    "pull -s currently supports only the local `pull -s ours <path> <branch>` shape; unsupported option {value}"
                )));
            }
            value => {
                if remote.is_none() {
                    remote = Some(value.to_string());
                } else if branch.is_none() {
                    branch = Some(value.to_string());
                } else {
                    return Err(GitError::Command(
                        "pull accepts at most one remote and one branch".into(),
                    ));
                }
            }
        }
    }
    let strategy = strategy.unwrap_or_default();
    if strategy != "ours" {
        return Err(GitError::Command(format!(
            "pull strategy '{strategy}' is not supported"
        )));
    }
    let (Some(remote), Some(branch)) = (remote, branch) else {
        return Err(GitError::Command(
            "pull -s ours requires explicit <repository> and <refspec> arguments".into(),
        ));
    };

    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let format = repository_object_format(&git_dir)?;
    let store = FileRefStore::new(&git_dir, format);
    let db = FileObjectDatabase::from_git_dir(&git_dir, format);

    // Only the "current repository" remote shape is modelled: git treats a
    // path argument as a remote URL, and `.` (or the repository root itself)
    // is how the t-suite pulls a local branch.
    let remote_is_self = remote == "."
        || fs::canonicalize(&remote)
            .ok()
            .zip(fs::canonicalize(cwd.clone()).ok())
            .is_some_and(|(a, b)| a == b);
    if !remote_is_self {
        return Err(GitError::Command(format!(
            "pull -s ours currently supports only the current repository as <repository>; got {remote}"
        )));
    }

    let other_oid = sley_rev::resolve_revision_with_reader(&git_dir, format, &db, &branch)?;
    let other_oid = sley_rev::peel_to_commit(&db, format, &other_oid)?;
    let Some(head_oid) = head_branch_oid(&store)? else {
        return Err(GitError::Command(
            "pull -s ours requires a born HEAD".into(),
        ));
    };

    // FETCH_HEAD / ORIG_HEAD, byte-matching `git pull . <branch>`.
    fs::write(
        git_dir.join("FETCH_HEAD"),
        format!("{other_oid}\t\tbranch '{branch}' of {remote}\n"),
    )?;
    fs::write(git_dir.join("ORIG_HEAD"), format!("{head_oid}\n"))?;

    let tree = pull_commit_tree_oid(&db, format, &head_oid)?;
    let author = commit_identity_from_env("AUTHOR")?;
    let committer = commit_identity_from_env("COMMITTER")?;
    let mut db = FileObjectDatabase::from_git_dir(&git_dir, format);
    let merge_oid = sley_sequencer::create_commit(
        &mut db,
        sley_sequencer::CommitCreate {
            tree,
            parents: vec![head_oid, other_oid],
            author,
            committer: committer.clone(),
            message: format!("Merge branch '{branch}'\n").into_bytes(),
            encoding: None,
            signature: None,
        },
    )?;
    let target_ref = match store.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(branch_ref)) => branch_ref,
        _ => "HEAD".to_string(),
    };
    let mut tx = store.transaction();
    tx.update(RefUpdate {
        name: target_ref,
        expected: Some(RefTarget::Direct(head_oid)),
        new: RefTarget::Direct(merge_oid),
        reflog: Some(ReflogEntry {
            old_oid: head_oid,
            new_oid: merge_oid,
            committer,
            message: format!(
                "pull {}: Merge made by the 'ours' strategy.",
                args.join(" ")
            )
            .into_bytes(),
        }),
    });
    tx.commit()?;
    println!("Merge made by the 'ours' strategy.");
    Ok(())
}

/// Resolve HEAD to a commit oid (through a symbolic branch if needed).
fn head_branch_oid(store: &FileRefStore) -> Result<Option<ObjectId>> {
    match store.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(branch)) => match store.read_ref(&branch)? {
            Some(RefTarget::Direct(oid)) => Ok(Some(oid)),
            _ => Ok(None),
        },
        Some(RefTarget::Direct(oid)) => Ok(Some(oid)),
        None => Ok(None),
    }
}

/// Tree oid of a commit, for the ours-merge result tree.
fn pull_commit_tree_oid(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    commit_oid: &ObjectId,
) -> Result<ObjectId> {
    let object = db.read_object(commit_oid)?;
    if object.object_type != ObjectType::Commit {
        return Err(GitError::InvalidObject(format!(
            "expected commit {commit_oid}, found {}",
            object.object_type.as_str()
        )));
    }
    Ok(Commit::parse_ref(format, &object.body)?.tree)
}
