use super::*;

pub(crate) fn read_commit_tree(
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

pub(crate) fn update_merge_head_ref(
    git_dir: &Path,
    format: ObjectFormat,
    old_oid: ObjectId,
    new_oid: ObjectId,
    _branch: &str,
    reflog_message: Vec<u8>,
    committer: Vec<u8>,
) -> Result<()> {
    let store = FileRefStore::new(git_dir, format);
    let reflog = ReflogEntry {
        old_oid,
        new_oid,
        committer,
        message: reflog_message,
    };
    let mut tx = store.transaction();
    match store.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(name)) => {
            tx.update(RefUpdate {
                name: name.clone(),
                expected: None,
                new: RefTarget::Direct(new_oid),
                reflog: Some(reflog.clone()),
            });
            tx.update(RefUpdate {
                name: "HEAD".into(),
                expected: None,
                new: RefTarget::Symbolic(name),
                reflog: Some(reflog),
            });
        }
        _ => {
            tx.update(RefUpdate {
                name: "HEAD".into(),
                expected: None,
                new: RefTarget::Direct(new_oid),
                reflog: Some(reflog),
            });
        }
    }
    tx.commit()
}
fn resolve_pull_remote_and_refspecs(
    config: &GitConfig,
    store: &FileRefStore,
    remote: Option<String>,
    branches: Vec<String>,
) -> Result<(String, Vec<String>, Vec<String>)> {
    match (remote, branches.is_empty()) {
        (Some(remote), false) => Ok((remote, branches.clone(), branches)),
        (Some(remote), true) => {
            let Some(current) = store.current_branch()? else {
                print_pull_no_merge_candidates_detached(false);
                return Err(GitError::Exit(1));
            };
            let merge_srcs = if remote_exists(config, &remote) {
                if let Some(default_remote) = config.get("branch", Some(&current), "remote")
                    && default_remote != remote
                {
                    eprintln!("You asked to pull from the remote '{remote}', but did not specify");
                    eprintln!("a branch. Because this is not the default configured remote");
                    eprintln!(
                        "for your current branch, you must specify a branch on the command line."
                    );
                    return Err(GitError::Exit(1));
                }
                let merge_srcs = branch_merge_values(config, &current);
                if merge_srcs.is_empty() {
                    return Ok((remote, vec!["HEAD".to_string()], Vec::new()));
                }
                merge_srcs
            } else {
                Vec::new()
            };
            Ok((remote, Vec::new(), merge_srcs))
        }
        (None, true) => {
            let Some(current) = store.current_branch()? else {
                print_pull_no_merge_candidates_detached(false);
                return Err(GitError::Exit(1));
            };
            let remote = match config.get("branch", Some(&current), "remote") {
                Some(remote) => remote.to_string(),
                None => match pull_default_remote_without_tracking(config) {
                    Some(remote) => {
                        return Ok((remote, vec!["HEAD".to_string()], Vec::new()));
                    }
                    None => {
                        print_pull_no_tracking(&current, false);
                        return Err(GitError::Exit(1));
                    }
                },
            };
            if config.get("branch", Some(&current), "merge").is_none() {
                return Ok((remote, vec!["HEAD".to_string()], Vec::new()));
            };
            Ok((remote, Vec::new(), branch_merge_values(config, &current)))
        }
        (None, false) => Err(GitError::Command(
            "pull currently requires a remote when a branch is specified".into(),
        )),
    }
}

fn pull_default_remote_without_tracking(config: &GitConfig) -> Option<String> {
    let remotes = crate::commands::remote_cmds::remote_names(config);
    match remotes.as_slice() {
        [] => None,
        [only] => Some(only.clone()),
        _ if remotes.iter().any(|remote| remote == "origin") => Some("origin".to_string()),
        _ => None,
    }
}

fn print_pull_no_merge_candidates_for_refspecs(rebase: bool) {
    if rebase {
        eprintln!(
            "There is no candidate for rebasing against among the refs that you just fetched."
        );
    } else {
        eprintln!("There are no candidates for merging among the refs that you just fetched.");
    }
    eprintln!(
        "Generally this means that you provided a wildcard refspec which had no\nmatches on the remote end."
    );
}

fn print_pull_no_merge_candidates_detached(rebase: bool) {
    eprintln!("You are not currently on a branch.");
    if rebase {
        eprintln!("Please specify which branch you want to rebase against.");
    } else {
        eprintln!("Please specify which branch you want to merge with.");
    }
    eprintln!("See git-pull(1) for details.");
    eprintln!();
    eprintln!("    git pull <remote> <branch>");
    eprintln!();
}

fn print_pull_no_tracking(current: &str, rebase: bool) {
    eprintln!("There is no tracking information for the current branch.");
    if rebase {
        eprintln!("Please specify which branch you want to rebase against.");
    } else {
        eprintln!("Please specify which branch you want to merge with.");
    }
    eprintln!("See git-pull(1) for details.");
    eprintln!();
    eprintln!("    git pull <remote> <branch>");
    eprintln!();
    eprintln!("If you wish to set tracking information for this branch you can do so with:");
    eprintln!();
    eprintln!("    git branch --set-upstream-to=<remote>/<branch> {current}");
    eprintln!();
}

fn print_pull_no_such_ref_fetched(merge_srcs: &[String]) {
    let src = merge_srcs.first().map(String::as_str).unwrap_or("HEAD");
    eprintln!(
        "Your configuration specifies to merge with the ref '{src}'\nfrom the remote, but no such ref was fetched."
    );
}

/// All `branch.<name>.merge` values configured for `branch`, in config order
/// (more than one is an octopus merge config).
fn branch_merge_values(config: &GitConfig, branch: &str) -> Vec<String> {
    config
        .get_all("branch", Some(branch), "merge")
        .into_iter()
        .flatten()
        .map(str::to_string)
        .collect()
}

pub(crate) fn fetch_head_merge_record(
    git_dir: &Path,
    format: ObjectFormat,
) -> Result<FetchHeadRecord> {
    fetch_head_merge_records(git_dir, format)?
        .into_iter()
        .next()
        .ok_or_else(|| GitError::reference_not_found("FETCH_HEAD"))
}

fn fetch_head_merge_records(git_dir: &Path, format: ObjectFormat) -> Result<Vec<FetchHeadRecord>> {
    let path = git_dir.join("FETCH_HEAD");
    let mut input =
        fs::File::open(path).map_err(|_| GitError::reference_not_found("FETCH_HEAD"))?;
    let records = read_fetch_head(format, &mut input)?;
    Ok(records
        .into_iter()
        .filter(|record| !record.not_for_merge)
        .collect())
}

pub(crate) fn resolve_fetch_head_revision(
    git_dir: &Path,
    format: ObjectFormat,
) -> Result<ObjectId> {
    Ok(fetch_head_merge_record(git_dir, format)?.oid)
}

fn ensure_pull_not_in_merge(git_dir: &Path, format: ObjectFormat) -> Result<()> {
    if let Ok(index) = read_worktree_index(git_dir, format)
        && !index_unmerged_paths(&index).is_empty()
    {
        eprintln!("error: Pulling is not possible because you have unmerged files.");
        eprintln!("hint: Fix them up in the work tree, and then use 'git add/rm <file>'");
        eprintln!("hint: as appropriate to mark resolution and make a commit.");
        eprintln!("fatal: Exiting because of an unresolved conflict.");
        return Err(GitError::Exit(128));
    }
    if git_dir.join("MERGE_HEAD").is_file() {
        eprintln!("fatal: You have not concluded your merge (MERGE_HEAD exists).");
        eprintln!("Please, commit your changes before merging.");
        return Err(GitError::Exit(128));
    }
    Ok(())
}

fn update_worktree_after_fetch_moved_head(
    git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    orig_head: Option<ObjectId>,
    curr_head: Option<ObjectId>,
) -> Result<()> {
    let (Some(orig_head), Some(curr_head)) = (orig_head, curr_head) else {
        return Ok(());
    };
    if orig_head == curr_head {
        return Ok(());
    }
    eprintln!(
        "warning: fetch updated the current branch head.\nfast-forwarding your working tree from\ncommit {orig_head}."
    );
    let orig_tree = commit_tree_oid(db, format, &orig_head)?;
    let curr_tree = commit_tree_oid(db, format, &curr_head)?;
    if fetch_moved_head_would_clobber_worktree(worktree_root, db, format, &orig_tree, &curr_tree)? {
        eprintln!(
            "fatal: Cannot fast-forward your working tree.\nAfter making sure that you saved anything precious from\n$ git diff {orig_head}\noutput, run\n$ git reset --hard\nto recover."
        );
        return Err(GitError::Exit(128));
    }
    verify_fast_forward_untracked_safe(worktree_root, git_dir, db, format, &orig_tree, &curr_tree)?;
    sley_worktree::reset_index_and_worktree_to_commit(worktree_root, git_dir, format, &curr_head)?;
    Ok(())
}

fn fetch_moved_head_would_clobber_worktree(
    worktree_root: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    orig_tree: &ObjectId,
    curr_tree: &ObjectId,
) -> Result<bool> {
    let orig_map = stash_tree_entry_map(db, format, orig_tree)?;
    let curr_map = stash_tree_entry_map(db, format, curr_tree)?;
    let changed = orig_map
        .keys()
        .chain(curr_map.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    for path in changed {
        if orig_map.get(&path) == curr_map.get(&path) {
            continue;
        }
        let Some((old_mode, old_oid)) = orig_map.get(&path) else {
            continue;
        };
        let rel = std::str::from_utf8(&path)
            .map_err(|_| GitError::InvalidFormat("non-utf8 worktree path".into()))?;
        if worktree_blob_identity(format, &worktree_root.join(rel))? != Some((*old_mode, *old_oid))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn worktree_blob_identity(format: ObjectFormat, path: &Path) -> Result<Option<(u32, ObjectId)>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    if metadata.is_dir() {
        return Ok(None);
    }
    if metadata.file_type().is_symlink() {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let target = fs::read_link(path)?;
            let body = target.as_os_str().as_bytes().to_vec();
            return Ok(Some((
                0o120000,
                sley_core::object_id_for_bytes(format, "blob", &body)?,
            )));
        }
        #[cfg(not(unix))]
        return Ok(None);
    }
    let body = fs::read(path)?;
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
    Ok(Some((
        mode,
        sley_core::object_id_for_bytes(format, "blob", &body)?,
    )))
}

fn ensure_pull_can_merge() -> Result<()> {
    let color_advice = effective_config_with_overrides()
        .and_then(|config| config.get("color", None, "advice").map(str::to_string))
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("always"));
    let print_hint = |line: &str| {
        if color_advice {
            eprintln!("\x1b[33m{line}\x1b[m");
        } else {
            eprintln!("{line}");
        }
    };
    print_hint("hint: You have divergent branches and need to specify how to reconcile them.");
    print_hint("hint: You can do so by running one of the following commands sometime before");
    print_hint("hint: your next pull:");
    print_hint("hint:");
    print_hint("hint:   git config pull.rebase false  # merge");
    print_hint("hint:   git config pull.rebase true   # rebase");
    print_hint("hint:   git config pull.ff only       # fast-forward only");
    print_hint("hint:");
    print_hint(
        "hint: You can replace \"git config\" with \"git config --global\" to set a default",
    );
    print_hint("hint: preference for all repositories. You can also pass --rebase, --no-rebase,");
    print_hint("hint: or --ff-only on the command line to override the configured default per");
    print_hint("hint: invocation.");
    eprintln!("fatal: Need to specify how to reconcile divergent branches.");
    Err(GitError::Exit(128))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PullFastForward {
    Allow,
    No,
    Only,
}

impl PullFastForward {
    fn as_merge_arg(self) -> &'static str {
        match self {
            PullFastForward::Allow => "--ff",
            PullFastForward::No => "--no-ff",
            PullFastForward::Only => "--ff-only",
        }
    }
}

fn parse_pull_ff_config(config: &GitConfig) -> Result<Option<PullFastForward>> {
    let Some(value) = config.get("pull", None, "ff") else {
        return Ok(None);
    };
    let trimmed = value.trim();
    if let Some(parsed) = parse_maybe_bool(trimmed) {
        return Ok(Some(if parsed {
            PullFastForward::Allow
        } else {
            PullFastForward::No
        }));
    }
    if trimmed.eq_ignore_ascii_case("only") {
        return Ok(Some(PullFastForward::Only));
    }
    eprintln!("fatal: invalid value for 'pull.ff': '{trimmed}'");
    Err(GitError::Exit(128))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PullRebase {
    False,
    True,
    Merges,
    Interactive,
}

impl PullRebase {
    fn enabled(self) -> bool {
        !matches!(self, PullRebase::False)
    }

    fn rebase_arg(self) -> Option<&'static str> {
        match self {
            PullRebase::False | PullRebase::True => None,
            PullRebase::Merges => Some("--rebase-merges"),
            PullRebase::Interactive => Some("--interactive"),
        }
    }
}

fn parse_pull_rebase_value(key: &str, value: &str) -> Result<PullRebase> {
    let trimmed = value.trim();
    if let Some(parsed) = parse_maybe_bool(trimmed) {
        return Ok(if parsed {
            PullRebase::True
        } else {
            PullRebase::False
        });
    }
    match trimmed.to_ascii_lowercase().as_str() {
        "merges" | "m" => Ok(PullRebase::Merges),
        "interactive" | "i" => Ok(PullRebase::Interactive),
        _ => {
            eprintln!("fatal: invalid value for '{key}': '{trimmed}'");
            Err(GitError::Exit(128))
        }
    }
}

fn parse_config_bool_value(value: &str) -> Option<bool> {
    parse_maybe_bool(value.trim())
}

fn pull_autostash_config(config: &GitConfig, rebase: PullRebase) -> Option<bool> {
    config
        .get("pull", None, "autostash")
        .and_then(parse_config_bool_value)
        .or_else(|| {
            if rebase.enabled() {
                config
                    .get("rebase", None, "autostash")
                    .and_then(parse_config_bool_value)
            } else {
                None
            }
        })
}

fn push_autostash_arg(args: &mut Vec<String>, autostash: Option<bool>) {
    match autostash {
        Some(true) => args.push("--autostash".to_string()),
        Some(false) => args.push("--no-autostash".to_string()),
        None => {}
    }
}

fn ensure_pull_rebase_clean_without_autostash(
    git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
) -> Result<()> {
    let status = crate::collect_short_status(worktree_root, git_dir, format)?;
    let tracked = status
        .iter()
        .filter(|entry| entry.index != b'?' && entry.index != b'!')
        .collect::<Vec<_>>();
    if tracked
        .iter()
        .all(|entry| entry.index == b' ' && entry.worktree == b' ')
    {
        return Ok(());
    }
    let has_staged = tracked.iter().any(|entry| entry.index != b' ');
    let has_unstaged = tracked.iter().any(|entry| entry.worktree != b' ');
    if has_unstaged {
        eprintln!("error: cannot pull with rebase: You have unstaged changes.");
    }
    if has_staged {
        eprintln!("error: cannot pull with rebase: Your index contains uncommitted changes.");
    }
    eprintln!("error: Please commit or stash them.");
    Err(GitError::Exit(128))
}

fn ensure_rebase_not_unborn_with_index(
    git_dir: &Path,
    format: ObjectFormat,
    orig_head: Option<ObjectId>,
) -> Result<()> {
    if orig_head.is_some() {
        return Ok(());
    }
    if let Ok(index) = read_worktree_index(git_dir, format)
        && !index.entries.is_empty()
    {
        eprintln!("fatal: Updating an unborn branch with changes added to the index.");
        return Err(GitError::Exit(128));
    }
    Ok(())
}

fn pull_rebase_fork_point(
    git_dir: &Path,
    format: ObjectFormat,
    config: &GitConfig,
    remote: &str,
    refspecs: &[String],
    merge_srcs: &[String],
    orig_head: Option<ObjectId>,
) -> Result<Option<ObjectId>> {
    let Some(orig_head) = orig_head else {
        return Ok(None);
    };
    if remote == "." {
        return Ok(None);
    }
    let Some(remote_ref) = refspecs.first().or_else(|| merge_srcs.first()) else {
        return Ok(None);
    };
    let remote_ref = if remote_ref.starts_with("refs/") {
        remote_ref.to_string()
    } else {
        format!("refs/heads/{remote_ref}")
    };
    let Some(tracking_ref) = pull_remote_tracking_ref(config, remote, &remote_ref) else {
        return Ok(None);
    };
    let common_git_dir = common_git_dir_for_git_dir(git_dir)?;
    let db = FileObjectDatabase::from_git_dir(&common_git_dir, format);
    merge_base_fork_point(git_dir, format, &db, &tracking_ref, &orig_head)
}

fn pull_remote_tracking_ref(config: &GitConfig, remote: &str, remote_ref: &str) -> Option<String> {
    for fetch in config
        .get_all("remote", Some(remote), "fetch")
        .into_iter()
        .flatten()
    {
        let refspec = parse_refspec(fetch).ok()?;
        if refspec.negative || refspec.dst.is_none() {
            continue;
        }
        if let Ok(Some(mapped)) = refspec_map_source(&refspec, remote_ref) {
            return Some(mapped);
        }
    }
    None
}

fn print_fetch_status(
    source: &str,
    updates: &[FetchRefUpdate],
    old_oids: &HashMap<String, ObjectId>,
) {
    let mut displayed = false;
    for update in updates {
        let src_short = update
            .src
            .strip_prefix("refs/heads/")
            .unwrap_or(update.src.as_str());
        let Some(dst) = update.dst.as_ref() else {
            if !displayed {
                eprintln!("From {source}");
                displayed = true;
            }
            eprintln!(" * branch            {src_short:11}-> FETCH_HEAD");
            continue;
        };
        if old_oids.get(dst) == Some(&update.oid) {
            continue;
        }
        if !displayed {
            eprintln!("From {source}");
            displayed = true;
        }
        let dst_short = dst.strip_prefix("refs/remotes/").unwrap_or(dst.as_str());
        let old_short = old_oids
            .get(dst)
            .map(format_log_abbrev_oid)
            .unwrap_or_else(|| "0000000".to_string());
        eprintln!(
            "   {}..{}  {:11} -> {}",
            old_short,
            format_log_abbrev_oid(&update.oid),
            src_short,
            dst_short
        );
    }
}

fn pull_fetch(
    git_dir: &Path,
    format: ObjectFormat,
    remote: &str,
    refspecs: &[String],
    options: FetchOptions,
) -> Result<FetchOutcome> {
    if let Ok(input) = fs::read(remote)
        && let Ok(bundle) = Bundle::parse(&input, format)
    {
        fetch_bundle(git_dir, format, remote, refspecs, &bundle, options)?;
        return Ok(FetchOutcome::default());
    }
    if fetch_source_is_ssh(remote)? {
        fetch_ssh_repository(git_dir, format, remote, refspecs, options)?;
        Ok(FetchOutcome::default())
    } else {
        let config = read_repo_config(git_dir)?;
        let remote_git_dir = ls_remote_git_dir(remote)?;
        let remote_common_git_dir = common_git_dir_for_git_dir(&remote_git_dir)?;
        let fetch_source = sley_remote::FetchSource::Local {
            git_dir: remote_git_dir,
            common_git_dir: remote_common_git_dir,
        };
        let store = FileRefStore::new(git_dir, format);
        let mut old_oids = HashMap::new();
        if !options.merge_srcs.is_empty() {
            for update_dst in store.list_refs()? {
                if let Some((oid, _)) = resolve_for_each_ref_target(&store, &update_dst)? {
                    old_oids.insert(update_dst.name, oid);
                }
            }
        }
        let quiet = options.quiet;
        let outcome = run_fetch_with_outcome(
            git_dir,
            format,
            &config,
            remote,
            &fetch_source,
            refspecs,
            options,
        )?;
        if !quiet {
            print_fetch_status(remote, &outcome.ref_updates, &old_oids);
        }
        Ok(outcome)
    }
}

fn run_fetch_with_outcome(
    git_dir: &Path,
    format: ObjectFormat,
    config: &GitConfig,
    source: &str,
    fetch_source: &sley_remote::FetchSource,
    refspecs: &[String],
    options: FetchOptions,
) -> Result<FetchOutcome> {
    let mut credentials = sley_remote::CredentialHelperProvider::new(Some(config));
    let mut progress = StdoutProgress;
    sley_remote::fetch(
        sley_remote::FetchRequest {
            git_dir,
            format,
            config,
            remote_name: source,
            source: fetch_source,
            refspecs,
            options: &options,
        },
        sley_remote::FetchServices {
            credentials: &mut credentials,
            progress: &mut progress,
            ref_hook: None,
        },
    )
}

fn pull_checkout_into_void(
    git_dir: &Path,
    worktree_root: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    commit_oid: &ObjectId,
) -> Result<()> {
    let object = db.read_object(commit_oid)?;
    let commit = Commit::parse_ref(format, &object.body)?;
    let target_map = stash_tree_entry_map(db, format, &commit.tree)?;
    let index_path = sley_worktree::repository_index_path(git_dir);
    let mut index_entries = if index_path.exists() {
        Index::parse(&fs::read(&index_path)?, format)?.entries
    } else {
        Vec::new()
    };
    let existing_paths = index_entries
        .iter()
        .filter(|entry| index_entry_stage(entry) == 0)
        .map(|entry| entry.path.clone().into_bytes())
        .collect::<HashSet<_>>();

    let mut local_changes = Vec::new();
    let mut untracked = Vec::new();
    for path in target_map.keys() {
        if existing_paths.contains(path) {
            local_changes.push(path.clone());
            continue;
        }
        let rel = std::str::from_utf8(path)
            .map_err(|_| GitError::InvalidFormat("non-utf8 worktree path".into()))?;
        if fs::symlink_metadata(worktree_root.join(rel)).is_ok() {
            untracked.push(path.clone());
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

    index_entries.retain(|entry| !target_map.contains_key(entry.path.as_ref()));
    for (path, (mode, oid)) in &target_map {
        let content = if sley_index::is_gitlink(*mode) {
            Vec::new()
        } else {
            merge_read_blob(db, oid)?
        };
        merge_write_worktree_file(worktree_root, path, &content, *mode)?;
        index_entries.push(merge_index_entry(path, *mode, *oid, 0));
    }
    index_entries.sort_by(|left, right| left.path.cmp(&right.path));
    fs::write(
        index_path,
        Index {
            version: 2,
            entries: index_entries,
            extensions: Vec::new(),
            checksum: None,
        }
        .write(format)?,
    )?;
    Ok(())
}

pub(crate) fn cmd_pull(args: &[String]) -> Result<()> {
    // git's `set_reflog_message`: record the pull invocation (`pull …`) as the
    // reflog action so a fast-forward merge writes `pull …: Fast-forward`. The
    // workspace forbids `std::env::set_var`, so the action is stashed in a
    // process-global store (mirroring the `GIT_CONFIG_PARAMETERS` pattern) and
    // read back by `merge_reflog_message`. Only set when neither the env var nor
    // an earlier override is present, matching git's `setenv(…, 0)`.
    if env::var_os("GIT_REFLOG_ACTION").is_none() {
        let mut action = String::from("pull");
        for arg in args {
            action.push(' ');
            action.push_str(arg);
        }
        set_reflog_action_override(action);
    }
    let mut opt_ff = None::<PullFastForward>;
    let mut verbosity = 0i32;
    let mut rebase_flag = None::<PullRebase>;
    let mut autostash_flag = None::<bool>;
    let mut force_rebase = false;
    let mut verify_signatures = None::<bool>;
    let mut dry_run = false;
    let mut no_write_fetch_head = false;
    let mut tags = None::<bool>;
    let mut merge_passthrough = Vec::<String>::new();
    let mut remote = None::<String>;
    let mut branches = Vec::<String>::new();
    let mut depth = None::<u32>;
    let mut expect_depth_value = false;
    let mut all = false;
    let mut set_upstream = false;
    let mut recurse_submodules_cli = FetchRecurseSubmodules::Default;
    for arg in args {
        if expect_depth_value {
            expect_depth_value = false;
            depth = Some(crate::commands::remote_cmds::parse_clone_depth(arg)?);
            continue;
        }
        match arg.as_str() {
            "--ff" => opt_ff = Some(PullFastForward::Allow),
            "--no-ff" => opt_ff = Some(PullFastForward::No),
            "--ff-only" => opt_ff = Some(PullFastForward::Only),
            "--rebase" => rebase_flag = Some(PullRebase::True),
            value if value.starts_with("--rebase=") => {
                let value = value.strip_prefix("--rebase=").unwrap_or_default();
                rebase_flag = Some(parse_pull_rebase_value("--rebase", value)?);
            }
            "--no-rebase" => rebase_flag = Some(PullRebase::False),
            "--autostash" => autostash_flag = Some(true),
            "--no-autostash" => autostash_flag = Some(false),
            "-f" | "--force" => force_rebase = true,
            "--verify-signatures" => {
                verify_signatures = Some(true);
                merge_passthrough.push(arg.clone());
            }
            "--no-verify-signatures" => {
                verify_signatures = Some(false);
                merge_passthrough.push(arg.clone());
            }
            "-q" | "--quiet" => {
                if verbosity <= 0 {
                    verbosity -= 1;
                } else {
                    verbosity = -1;
                }
            }
            "-v" | "--verbose" => {
                if verbosity >= 0 {
                    verbosity += 1;
                } else {
                    verbosity = 1;
                }
            }
            "--no-quiet" | "--no-verbose" => verbosity = 0,
            "--all" => all = true,
            "--no-all" => all = false,
            "-u" | "--set-upstream" => set_upstream = true,
            "--no-set-upstream" => set_upstream = false,
            "--dry-run" => dry_run = true,
            "--no-dry-run" => dry_run = false,
            "--no-write-fetch-head" => no_write_fetch_head = true,
            "--tags" => tags = Some(true),
            "--no-tags" => tags = Some(false),
            "-n"
            | "--no-stat"
            | "--stat"
            | "--summary"
            | "--no-summary"
            | "--compact-summary"
            | "--no-compact-summary"
            | "--log"
            | "--no-log"
            | "--commit"
            | "--no-commit"
            | "--squash"
            | "--no-squash"
            | "--allow-unrelated-histories"
            | "--no-allow-unrelated-histories"
            | "--signoff"
            | "--no-signoff"
            | "--no-verify"
            | "--verify" => merge_passthrough.push(arg.clone()),
            value if value.starts_with("--log=") || value.starts_with("--cleanup=") => {
                merge_passthrough.push(value.to_string());
            }
            "--recurse-submodules" => recurse_submodules_cli = FetchRecurseSubmodules::On,
            "--no-recurse-submodules" => recurse_submodules_cli = FetchRecurseSubmodules::Off,
            value if value.starts_with("--recurse-submodules=") => {
                let value = value.strip_prefix("--recurse-submodules=").ok_or_else(|| {
                    GitError::Command("pull --recurse-submodules requires a value".into())
                })?;
                recurse_submodules_cli = FetchRecurseSubmodules::from_arg(Some(value))?;
            }
            "--depth" => expect_depth_value = true,
            value if value.starts_with("--depth=") => {
                depth = Some(crate::commands::remote_cmds::parse_clone_depth(
                    value.strip_prefix("--depth=").unwrap_or_default(),
                )?);
            }
            value if value.starts_with('-') => {
                return Err(GitError::Command(format!(
                    "pull currently supports --ff-only, --no-ff, --rebase, --no-rebase, --autostash, --no-autostash, --quiet, --recurse-submodules, --no-recurse-submodules, and remote/branch arguments; unsupported option {value}"
                )));
            }
            value => {
                if remote.is_none() {
                    remote = Some(value.to_string());
                } else {
                    branches.push(value.to_string());
                }
            }
        }
    }
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let format = repository_object_format(&git_dir)?;
    let config = read_repo_config(&git_dir)?;
    let store = FileRefStore::new(&git_dir, format);
    if no_write_fetch_head {
        eprintln!("error: unknown option `no-write-fetch-head'");
        return Err(GitError::Exit(129));
    }
    if all && dry_run {
        return Ok(());
    }
    let (remote, refspecs, merge_srcs) =
        resolve_pull_remote_and_refspecs(&config, &store, remote, branches)?;
    ensure_pull_not_in_merge(&git_dir, format)?;
    if opt_ff.is_none() {
        opt_ff = parse_pull_ff_config(&config)?;
        if rebase_flag.is_some() && opt_ff == Some(PullFastForward::Only) {
            opt_ff = Some(PullFastForward::Allow);
        }
    }
    // Mirror git's `config_get_rebase` (builtin/pull.c): an explicit
    // `--rebase`/`--no-rebase` wins; otherwise `branch.<name>.rebase` is
    // consulted before the global `pull.rebase`. `rebase_unspecified` stays true
    // only when *none* of those sources expressed a preference — that is the sole
    // gate for the "Need to specify how to reconcile divergent branches" die, so
    // a bare `--no-rebase` must clear it (was previously keyed off `pull.rebase`
    // alone, which wrongly fired on `git pull --no-rebase`).
    let current_branch_name = match store.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(name)) => Some(
            name.strip_prefix("refs/heads/")
                .unwrap_or(&name)
                .to_string(),
        ),
        _ => None,
    };
    let branch_rebase = current_branch_name
        .as_deref()
        .and_then(|name| config.get("branch", Some(name), "rebase"));
    let config_rebase = branch_rebase.or_else(|| config.get("pull", None, "rebase"));
    let (effective_rebase, rebase_unspecified) = match rebase_flag {
        Some(value) => (value, false),
        None => match config_rebase {
            Some(value) => (parse_pull_rebase_value("pull.rebase", value)?, false),
            None => (PullRebase::False, true),
        },
    };
    if effective_rebase.enabled() && verify_signatures == Some(true) {
        eprintln!("warning: ignoring --verify-signatures for rebase");
    }
    let effective_autostash =
        autostash_flag.or_else(|| pull_autostash_config(&config, effective_rebase));
    let orig_head = head_commit_oid(&store)?;
    let rebase_fork_point = if effective_rebase.enabled() {
        ensure_rebase_not_unborn_with_index(&git_dir, format, orig_head)?;
        pull_rebase_fork_point(
            &git_dir,
            format,
            &config,
            &remote,
            &refspecs,
            &merge_srcs,
            orig_head,
        )?
    } else {
        None
    };
    if effective_rebase.enabled() && effective_autostash != Some(true) {
        let worktree_root = worktree_root_for_git_dir(&git_dir)?;
        ensure_pull_rebase_clean_without_autostash(&git_dir, &worktree_root, format)?;
    }
    let fetch_options = FetchOptions {
        quiet: verbosity < 0,
        auto_follow_tags: true,
        fetch_all_tags: tags == Some(true),
        prune: false,
        prune_tags: false,
        dry_run,
        force: false,
        append: false,
        write_fetch_head: !dry_run,
        tag_option_explicit: tags.is_some(),
        prune_option_explicit: false,
        prune_tags_option_explicit: false,
        refmap: None,
        depth,
        merge_srcs: merge_srcs.clone(),
        filter: None,
        refetch: false,
        cloning: false,
        record_promisor_refs: true,
        update_shallow: false,
        deepen_relative: false,
        update_head_ok: true,
        deepen_since: None,
        deepen_not: Vec::new(),
        ssh_options: None,
        atomic: false,
    };
    let fetch_recurse_submodules = resolve_fetch_recurse_submodules(
        &config,
        recurse_submodules_cli,
        FetchRecurseSubmodules::OnDemand,
    );
    let update_recurse_submodules = match recurse_submodules_cli {
        FetchRecurseSubmodules::On | FetchRecurseSubmodules::OnDemand => true,
        FetchRecurseSubmodules::Off => false,
        FetchRecurseSubmodules::Default => config
            .get_bool("submodule", None, "recurse")
            .unwrap_or(false),
    };
    // git captures `orig_head` (HEAD before the fetch). A refspec like
    // `main:main` can create the current branch during the fetch, but the
    // pull-into-void decision keys off the *pre-fetch* state, so capture it now.
    let orig_head_unborn = orig_head.is_none();
    let before_fetch_refs = fetch_ref_snapshot(&git_dir, format)?;
    let fetch_outcome =
        match pull_fetch(&git_dir, format, &remote, &refspecs, fetch_options.clone()) {
            Ok(outcome) => outcome,
            Err(err) => {
                if !merge_srcs.is_empty() && format!("{err}").contains("remote ref") {
                    print_pull_no_such_ref_fetched(&merge_srcs);
                    return Err(GitError::Exit(1));
                }
                return Err(err);
            }
        };
    if set_upstream {
        crate::commands::remote_cmds::fetch_set_upstream_from_outcome(
            &git_dir,
            format,
            &remote,
            &fetch_outcome,
        )?;
    }
    if dry_run {
        return Ok(());
    }
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let db = FileObjectDatabase::from_git_dir(&common_git_dir, format);
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    fetch_populated_submodules_after_superproject(FetchSubmoduleRequest {
        git_dir: &git_dir,
        format,
        worktree_root: &worktree_root,
        config: &config,
        recurse_submodules: fetch_recurse_submodules,
        default_recurse_submodules: FetchRecurseSubmodules::OnDemand,
        source: &remote,
        changed_gitlinks: changed_gitlinks_for_fetch(
            &git_dir,
            format,
            &before_fetch_refs,
            &fetch_outcome,
        )?,
        options: &fetch_options,
        submodule_prefix: "",
        jobs: None,
    })?;
    let curr_head = head_commit_oid(&store)?;
    update_worktree_after_fetch_moved_head(
        &git_dir,
        &worktree_root,
        format,
        &db,
        orig_head,
        curr_head,
    )?;
    // Pulling into an unborn branch (git's `pull_into_void`): there is no HEAD to
    // merge against, so we fast-forward to FETCH_HEAD's merge target by pointing
    // HEAD at it (unless a refspec already moved the current branch there) and
    // checking out its tree. Keyed off the *pre-fetch* state so a `main:main`
    // refspec that created the branch still triggers the void checkout.
    let merge_records = match fetch_head_merge_records(&git_dir, format) {
        Ok(records) if !records.is_empty() => records,
        Ok(_) if !refspecs.is_empty() => {
            print_pull_no_merge_candidates_for_refspecs(effective_rebase.enabled());
            return Err(GitError::Exit(1));
        }
        Ok(_) if !merge_srcs.is_empty() => {
            print_pull_no_such_ref_fetched(&merge_srcs);
            return Err(GitError::Exit(1));
        }
        Ok(_) => return Err(GitError::reference_not_found("FETCH_HEAD")),
        Err(_) if !refspecs.is_empty() => {
            print_pull_no_merge_candidates_for_refspecs(effective_rebase.enabled());
            return Err(GitError::Exit(1));
        }
        Err(_) if !merge_srcs.is_empty() => {
            print_pull_no_such_ref_fetched(&merge_srcs);
            return Err(GitError::Exit(1));
        }
        Err(err) => return Err(err),
    };
    if orig_head_unborn {
        if merge_records.len() > 1 {
            eprintln!("fatal: Cannot merge multiple branches into empty head.");
            return Err(GitError::Exit(128));
        }
        let merge_oid = merge_records[0].oid;
        pull_checkout_into_void(&git_dir, &worktree_root, &db, format, &merge_oid)?;
        let target_ref = match store.read_ref("HEAD")? {
            Some(RefTarget::Symbolic(branch)) => branch,
            _ => "HEAD".to_string(),
        };
        // The branch may already point at `merge_oid` if a refspec like
        // `main:main` updated it during the fetch; only move it when it doesn't.
        if store.read_ref(&target_ref)? != Some(RefTarget::Direct(merge_oid)) {
            let mut tx = store.transaction();
            tx.update(RefUpdate {
                name: target_ref,
                expected: None,
                new: RefTarget::Direct(merge_oid),
                reflog: Some(ReflogEntry {
                    old_oid: zero_oid(format)?,
                    new_oid: merge_oid,
                    committer: commit_identity_from_env("COMMITTER")?,
                    message: b"initial pull".to_vec(),
                }),
            });
            tx.commit()?;
        }
        return Ok(());
    }
    let ours_oid = resolve_revision(&git_dir, format, "HEAD")?;
    let merge_oids = merge_records
        .iter()
        .map(|record| sley_rev::peel_to_commit(&db, format, &record.oid))
        .collect::<Result<Vec<_>>>()?;
    if merge_oids.len() > 1 {
        if effective_rebase.enabled() {
            eprintln!("fatal: Cannot rebase onto multiple branches.");
            return Err(GitError::Exit(128));
        }
        if opt_ff == Some(PullFastForward::Only) {
            eprintln!("fatal: Cannot fast-forward to multiple branches.");
            return Err(GitError::Exit(128));
        }
    }
    let theirs_oid = merge_oids[0];
    let ours_commit = sley_rev::peel_to_commit(&db, format, &ours_oid)?;
    let already_up_to_date = merge_oids.iter().all(|theirs_commit| {
        *theirs_commit == ours_commit
            || ancestor_depths(&db, format, &ours_commit)
                .is_ok_and(|ours_depths| ours_depths.contains_key(theirs_commit))
    });
    if already_up_to_date {
        if verbosity >= 0 {
            println!("Already up to date.");
        }
        // git's pull still runs `git submodule update` after an up-to-date merge,
        // so a `--recurse-submodules` pull re-syncs a submodule worktree that an
        // earlier `--no-recurse-submodules` pull advanced the gitlink for without
        // checking out.
        pull_update_submodules_after_merge(update_recurse_submodules, verbosity)?;
        return Ok(());
    }
    let fast_forward = if merge_oids.len() == 1 {
        ancestor_depths(&db, format, &theirs_oid)?.contains_key(&ours_commit)
    } else {
        false
    };
    let mut effective_rebase = effective_rebase;
    if opt_ff == Some(PullFastForward::Only) {
        if !fast_forward {
            eprintln!("fatal: Not possible to fast-forward, aborting.");
            return Err(GitError::Exit(128));
        }
        effective_rebase = PullRebase::False;
    }
    if opt_ff.is_none() && rebase_unspecified && !fast_forward {
        ensure_pull_can_merge()?;
    }
    if fast_forward {
        let mut merge_args = Vec::new();
        if effective_rebase.enabled() {
            merge_args.push("--ff-only".to_string());
        } else if let Some(ff) = opt_ff {
            merge_args.push(ff.as_merge_arg().to_string());
        }
        if update_recurse_submodules {
            merge_args.push("--recurse-submodules".to_string());
        }
        merge_args.extend(merge_passthrough.iter().cloned());
        push_autostash_arg(&mut merge_args, effective_autostash);
        if verbosity < 0 {
            merge_args.push("--quiet".to_string());
        }
        merge_args.push("FETCH_HEAD".to_string());
        cmd_merge(&merge_args)?;
        pull_update_submodules_after_merge(update_recurse_submodules, verbosity)?;
        return Ok(());
    }
    if effective_rebase.enabled() {
        let mut rebase_args = Vec::new();
        if let Some(arg) = effective_rebase.rebase_arg() {
            rebase_args.push(arg.to_string());
        }
        push_autostash_arg(&mut rebase_args, effective_autostash);
        if verbosity < 0 {
            rebase_args.push("--quiet".to_string());
        }
        if force_rebase {
            rebase_args.push("--force-rebase".to_string());
        }
        if update_recurse_submodules {
            rebase_args.push("--recurse-submodules".to_string());
        }
        if let Some(fork_point) = rebase_fork_point {
            rebase_args.push("--onto".to_string());
            rebase_args.push("FETCH_HEAD".to_string());
            rebase_args.push(fork_point.to_hex());
        } else {
            rebase_args.push("FETCH_HEAD".to_string());
        }
        return commands::rebase::cmd_rebase(&rebase_args);
    }
    let mut merge_args = Vec::new();
    if let Some(ff) = opt_ff {
        merge_args.push(ff.as_merge_arg().to_string());
    }
    if update_recurse_submodules {
        merge_args.push("--recurse-submodules".to_string());
    }
    merge_args.extend(merge_passthrough);
    push_autostash_arg(&mut merge_args, effective_autostash);
    if verbosity < 0 {
        merge_args.push("--quiet".to_string());
    }
    if merge_oids.len() == 1 {
        merge_args.push("FETCH_HEAD".to_string());
    } else {
        merge_args.extend(merge_oids.iter().map(ToString::to_string));
    }
    cmd_merge(&merge_args)?;
    pull_update_submodules_after_merge(update_recurse_submodules, verbosity)?;
    Ok(())
}

/// git's pull `update_submodules`: after the superproject merge, check out each
/// active submodule's working tree to the recorded gitlink commit
/// (`git submodule update --recursive`). This runs even when the merge was a
/// no-op ("Already up to date"), so a submodule left stale by an earlier
/// `--no-recurse-submodules` pull is brought back in sync on the next
/// `--recurse-submodules` pull. Scoped to the merge paths; `pull --rebase`
/// keeps its own (local-commit-preserving) submodule handling.
fn pull_update_submodules_after_merge(recurse: bool, verbosity: i32) -> Result<()> {
    if !recurse {
        return Ok(());
    }
    let mut args = Vec::new();
    if verbosity < 0 {
        args.push("--quiet".to_string());
    }
    args.push("update".to_string());
    args.push("--recursive".to_string());
    commands::submodule::cmd_submodule(&args)
}
