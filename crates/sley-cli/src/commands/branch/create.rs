//! Branch creation and tracking setup.

use super::config::{AutoRebase, validate_autosetuprebase, write_branch_repo_config};
use super::delete::force_update_branch;
use super::list::{BranchListMode, print_branch_list};
use super::operand::{
    BranchOperandKind, branch_resolve_local_branch_operand, validate_branch_creation_name,
};
use super::upstream::{
    ResolvedBranchUpstream, branch_tracking_ref_candidate, branch_upstream_remote_ref,
    resolve_branch_upstream,
};
use crate::*;

pub(super) struct BranchCreateOptions {
    pub(crate) force: bool,
    pub(crate) quiet: bool,
    pub(crate) track: Option<BranchTrackMode>,
    pub(crate) recurse_submodules: bool,
    pub(crate) legacy_set_upstream: bool,
    pub(crate) edit_description: bool,
    pub(crate) create_reflog: bool,
    pub(crate) positionals: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum BranchTrackMode {
    Direct,
    Inherit,
    Never,
}
pub(super) fn run_branch_create_options(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    config: &GitConfig,
    replace_objects: bool,
    options: BranchCreateOptions,
) -> Result<()> {
    // git's `builtin/branch.c`:
    //   recurse = (explicit || submodule.recurse) && submodule.propagateBranches
    // Explicit `--recurse-submodules` without propagateBranches is fatal.
    let propagate = config
        .get_bool("submodule", None, "propagateBranches")
        .unwrap_or(false);
    let config_recurse = config
        .get_bool("submodule", None, "recurse")
        .unwrap_or(false);
    if options.recurse_submodules && !propagate {
        eprintln!(
            "fatal: branch with --recurse-submodules can only be used if submodule.propagateBranches is enabled"
        );
        return Err(GitError::Exit(128));
    }
    let recurse_submodules = (options.recurse_submodules || config_recurse) && propagate;
    if options.edit_description {
        return branch_edit_description(git_dir, format, store, &options.positionals);
    }
    if options.legacy_set_upstream && !options.positionals.is_empty() {
        eprintln!(
            "fatal: the '--set-upstream' option is no longer supported. Please use '--track' or '--set-upstream-to' instead"
        );
        return Err(GitError::Exit(128));
    }
    match options.positionals.as_slice() {
        [] => print_branch_list(store, BranchListMode::Local),
        [branch] if options.force && recurse_submodules => create_branches_recursively(
            git_dir,
            format,
            store,
            config,
            replace_objects,
            branch,
            None,
            options.track,
            options.create_reflog,
            options.quiet,
            true,
        ),
        [branch] if options.force => {
            let branch = force_update_branch(
                git_dir, format, store, config, replace_objects, branch, None,
            )?;
            branch_create_set_tracking(git_dir, store, &branch, None, options.track, options.quiet)
        }
        [branch] if recurse_submodules => create_branches_recursively(
            git_dir,
            format,
            store,
            config,
            replace_objects,
            branch,
            None,
            options.track,
            options.create_reflog,
            options.quiet,
            false,
        ),
        [branch] => {
            create_branch_from_start_with_reflog(
                git_dir,
                format,
                store,
                config,
                replace_objects,
                branch,
                None,
                options.create_reflog,
            )?;
            branch_create_set_tracking_or_rollback(
                git_dir,
                store,
                branch,
                None,
                options.track,
                options.quiet,
            )
        }
        [branch, start] if options.force && recurse_submodules => create_branches_recursively(
            git_dir,
            format,
            store,
            config,
            replace_objects,
            branch,
            Some(start),
            options.track,
            options.create_reflog,
            options.quiet,
            true,
        ),
        [branch, start] if options.force => {
            let branch = force_update_branch(
                git_dir,
                format,
                store,
                config,
                replace_objects,
                branch,
                Some(start),
            )?;
            branch_create_set_tracking(
                git_dir,
                store,
                &branch,
                Some(start),
                options.track,
                options.quiet,
            )
        }
        [branch, start] if recurse_submodules => create_branches_recursively(
            git_dir,
            format,
            store,
            config,
            replace_objects,
            branch,
            Some(start),
            options.track,
            options.create_reflog,
            options.quiet,
            false,
        ),
        [branch, start] => {
            create_branch_from_start_with_reflog(
                git_dir,
                format,
                store,
                config,
                replace_objects,
                branch,
                Some(start),
                options.create_reflog,
            )?;
            branch_create_set_tracking_or_rollback(
                git_dir,
                store,
                branch,
                Some(start),
                options.track,
                options.quiet,
            )
        }
        _ => Err(GitError::Command(
            "branch currently supports: branch [--list [<pattern>...]] [<name> [<start>]] or branch -d|-D <name>... or branch --force <name> [<start>]"
                .into(),
        )),
    }
}

/// Port of git's `create_branches_recursively` (`branch.c`): create `branch` in
/// the superproject and every *active* submodule recorded in the start-point
/// tree, dry-running the submodule side first so a single failure rolls the
/// whole operation back (no partial branch set).
///
/// Nested submodules are handled by recursing into each submodule's own tree
/// after its branch is created (matching `submodule--helper create-branch`).
fn create_branches_recursively(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    config: &GitConfig,
    replace_objects: bool,
    branch: &str,
    start: Option<&String>,
    track: Option<BranchTrackMode>,
    create_reflog: bool,
    quiet: bool,
    force: bool,
) -> Result<()> {
    create_branches_recursively_inner(
        git_dir,
        format,
        store,
        config,
        replace_objects,
        branch,
        start,
        /* tracking_name */ start.map(String::as_str),
        track,
        create_reflog,
        quiet,
        force,
        /* dry_run */ false,
    )
}

fn create_branches_recursively_inner(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    config: &GitConfig,
    replace_objects: bool,
    branch: &str,
    start: Option<&String>,
    tracking_name: Option<&str>,
    track: Option<BranchTrackMode>,
    create_reflog: bool,
    quiet: bool,
    force: bool,
    dry_run: bool,
) -> Result<()> {
    let start_rev = start.map_or("HEAD", String::as_str);
    let start_oid = resolve_branch_start(git_dir, format, store, replace_objects, start_rev)?;
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let start_commit = sley_rev::peel_to_commit(&db, format, &start_oid)?;
    let children = active_submodule_gitlinks(git_dir, &db, format, config, &start_commit)?;

    // Dry-run submodule creates first (git's first loop with dry_run=1).
    for child in &children {
        create_branch_in_submodule(
            git_dir,
            format,
            config,
            replace_objects,
            branch,
            child,
            tracking_name,
            track,
            create_reflog,
            quiet,
            force,
            /* dry_run */ true,
        )?;
    }

    if dry_run {
        // Validate this repository's own create as well.
        validate_branch_create_here(store, branch, force)?;
        // And that the start oid is a real commit in this object store.
        let _ = sley_rev::peel_to_commit(&db, format, &start_oid)?;
        return Ok(());
    }

    // Superproject / this-repo create. `start` is the user-facing start-point
    // at the top level (or None → HEAD); when recursing into a submodule it is
    // the gitlink oid hex so the new tip matches the superproject recording.
    if force {
        let _ = force_update_branch(
            git_dir,
            format,
            store,
            config,
            replace_objects,
            branch,
            start,
        )?;
    } else {
        create_branch_from_start_with_reflog(
            git_dir,
            format,
            store,
            config,
            replace_objects,
            branch,
            start,
            create_reflog,
        )?;
    }

    // Tracking uses the original start *name* (branch point), not the peeled
    // oid — so `origin/branch-a` still sets remote tracking in submodules even
    // when that ref does not exist there (t3207 remote-tracking tests).
    let track_start = tracking_name.map(|s| s.to_string());
    // Soft failures (unable to resolve upstream in a submodule) are ignored by
    // the tracking helpers themselves; hard errors still propagate.
    branch_create_set_tracking(git_dir, store, branch, track_start.as_ref(), track, quiet)?;

    // Real submodule creates (git's second loop with dry_run=0).
    for child in &children {
        create_branch_in_submodule(
            git_dir,
            format,
            config,
            replace_objects,
            branch,
            child,
            tracking_name,
            track,
            create_reflog,
            quiet,
            force,
            /* dry_run */ false,
        )?;
    }
    Ok(())
}

struct SubmoduleBranchTarget {
    path: String,
    name: String,
    oid: ObjectId,
}

fn active_submodule_gitlinks(
    git_dir: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    config: &GitConfig,
    start_commit: &ObjectId,
) -> Result<Vec<SubmoduleBranchTarget>> {
    let tree = commands::merge_rebase::commit_tree_oid(db, format, start_commit)?;
    let leaves = sley_diff_merge::flatten_tree(db, format, &tree)?;
    let submodules = submodule_config_for_commit(git_dir, db, format, start_commit);
    let mut out = Vec::new();
    for (path_bytes, (mode, oid)) in leaves {
        if !sley_index::is_gitlink(mode) {
            continue;
        }
        let Ok(path) = std::str::from_utf8(&path_bytes) else {
            continue;
        };
        let name = submodules
            .as_ref()
            .and_then(|set| set.from_path(path))
            .map(|sub| sub.name.clone())
            .unwrap_or_else(|| path.to_string());
        // git's `is_tree_submodule_active` — inactive submodules are skipped
        // entirely (t3207 "should not create branches in inactive submodules").
        if !branch_is_submodule_active(config, &name, path) {
            continue;
        }
        out.push(SubmoduleBranchTarget {
            path: path.to_string(),
            name,
            oid,
        });
    }
    Ok(out)
}

fn submodule_config_for_commit(
    git_dir: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    commit: &ObjectId,
) -> Option<sley_submodule::SubmoduleConfigSet> {
    // Prefer the worktree .gitmodules when present (usual case).
    if let Some(worktree) = sley_worktree::worktree_root_for_git_dir(git_dir)
        .ok()
        .flatten()
    {
        if let Ok(cfg) = GitConfig::read(worktree.join(".gitmodules")) {
            return Some(sley_submodule::SubmoduleConfigSet::parse(&cfg));
        }
    }
    // Fall back to the start commit's tree (needed when creating a branch
    // whose start introduces a submodule not in the current worktree).
    let tree = commands::merge_rebase::commit_tree_oid(db, format, commit).ok()?;
    let leaves = sley_diff_merge::flatten_tree(db, format, &tree).ok()?;
    let (_, gitmodules_oid) = leaves
        .into_iter()
        .find(|(path, _)| path.as_slice() == b".gitmodules")?;
    let object = db.read_object(&gitmodules_oid.1).ok()?;
    if object.object_type != ObjectType::Blob {
        return None;
    }
    let cfg = GitConfig::parse(&object.body).ok()?;
    Some(sley_submodule::SubmoduleConfigSet::parse(&cfg))
}

fn branch_is_submodule_active(repo_config: &GitConfig, name: &str, path: &str) -> bool {
    if let Some(active) = repo_config.get_bool("submodule", Some(name), "active") {
        return active;
    }
    let active_specs: Vec<&str> = repo_config
        .get_all("submodule", None, "active")
        .into_iter()
        .flatten()
        .collect();
    if !active_specs.is_empty() {
        return active_specs.iter().any(|spec| {
            let spec = spec.trim_end_matches('/');
            if spec.is_empty() || spec == "." {
                return true;
            }
            path == spec || path.starts_with(&format!("{spec}/"))
        });
    }
    repo_config.get("submodule", Some(name), "url").is_some()
}

fn branch_submodule_admin_git_dir(super_git_dir: &Path, name: &str) -> PathBuf {
    let mut path = super_git_dir.join("modules");
    for component in name.split('/') {
        if !component.is_empty() {
            path.push(component);
        }
    }
    path
}

fn validate_branch_create_here(store: &FileRefStore, branch: &str, force: bool) -> Result<()> {
    let refname = validate_branch_creation_name(branch)?;
    if !force && store.read_ref(&refname)?.is_some() {
        eprintln!("fatal: a branch named '{branch}' already exists");
        return Err(GitError::Exit(128));
    }
    Ok(())
}

fn create_branch_in_submodule(
    super_git_dir: &Path,
    super_format: ObjectFormat,
    super_config: &GitConfig,
    replace_objects: bool,
    branch: &str,
    child: &SubmoduleBranchTarget,
    tracking_name: Option<&str>,
    track: Option<BranchTrackMode>,
    create_reflog: bool,
    quiet: bool,
    force: bool,
    dry_run: bool,
) -> Result<()> {
    let sub_git_dir = branch_submodule_admin_git_dir(super_git_dir, &child.name);
    if !sub_git_dir.is_dir() {
        // Also try the worktree-populated path's .git file → admin dir.
        let alt = sley_worktree::worktree_root_for_git_dir(super_git_dir)
            .ok()
            .flatten()
            .and_then(|root| {
                let sub_root = root.join(&child.path);
                sley_diff_merge::gitlink_git_dir(&sub_root)
            });
        let Some(sub_git_dir) = alt.filter(|p| p.is_dir()) else {
            eprintln!(
                "fatal: submodule '{}': unable to find submodule",
                child.name
            );
            return Err(GitError::Exit(128));
        };
        return create_branch_in_submodule_at(
            &sub_git_dir,
            super_format,
            super_config,
            replace_objects,
            branch,
            child,
            tracking_name,
            track,
            create_reflog,
            quiet,
            force,
            dry_run,
        );
    }
    create_branch_in_submodule_at(
        &sub_git_dir,
        super_format,
        super_config,
        replace_objects,
        branch,
        child,
        tracking_name,
        track,
        create_reflog,
        quiet,
        force,
        dry_run,
    )
}

fn create_branch_in_submodule_at(
    sub_git_dir: &Path,
    super_format: ObjectFormat,
    _super_config: &GitConfig,
    replace_objects: bool,
    branch: &str,
    child: &SubmoduleBranchTarget,
    tracking_name: Option<&str>,
    track: Option<BranchTrackMode>,
    create_reflog: bool,
    quiet: bool,
    force: bool,
    dry_run: bool,
) -> Result<()> {
    let format = repository_object_format(sub_git_dir).unwrap_or(super_format);
    let store = FileRefStore::new(sub_git_dir, format);
    let config = read_repo_config(sub_git_dir).unwrap_or_default();
    // Prefix submodule errors like git's "submodule 'name': …".
    // Suppress the bare "fatal: a branch named …" from the child and re-emit
    // with git's "submodule 'X': …" prefix so t3207's grep matches.
    let already_exists = !force
        && store
            .read_ref(&format!("refs/heads/{branch}"))
            .ok()
            .flatten()
            .is_some();
    if dry_run && already_exists {
        eprintln!(
            "submodule '{}': fatal: a branch named '{branch}' already exists",
            child.name
        );
        return Err(GitError::Exit(128));
    }

    // Pass the gitlink oid as the start-point so the branch tip matches the
    // superproject's recorded commit (not the submodule's current HEAD).
    let start_oid_hex = child.oid.to_hex();
    create_branches_recursively_inner(
        sub_git_dir,
        format,
        &store,
        &config,
        replace_objects,
        branch,
        Some(&start_oid_hex),
        tracking_name,
        track,
        create_reflog,
        quiet,
        force,
        dry_run,
    )
    .map_err(|err| {
        if dry_run {
            eprintln!(
                "submodule '{}': cannot create branch '{branch}'",
                child.name
            );
        } else {
            eprintln!(
                "fatal: submodule '{}': cannot create branch '{branch}'",
                child.name
            );
        }
        err
    })
}

pub(super) fn branch_create_set_tracking_or_rollback(
    git_dir: &Path,
    store: &FileRefStore,
    branch: &str,
    start: Option<&String>,
    track: Option<BranchTrackMode>,
    quiet: bool,
) -> Result<()> {
    match branch_create_set_tracking(git_dir, store, branch, start, track, quiet) {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = store.delete_branch(branch);
            Err(err)
        }
    }
}

pub(super) fn branch_edit_description(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    positionals: &[String],
) -> Result<()> {
    let branch = match positionals {
        [] => {
            if let Some(branch) = store.current_branch()? {
                let refname = branch_ref_name(&branch)?;
                if store.read_ref(&refname)?.is_none() {
                    eprintln!("fatal: cannot give description to unborn branch '{branch}'");
                    return Err(GitError::Exit(128));
                }
                branch
            } else {
                eprintln!("fatal: cannot give description to detached HEAD");
                return Err(GitError::Exit(128));
            }
        }
        [branch] => {
            let (branch, refname) = branch_resolve_local_branch_operand(
                git_dir,
                format,
                store,
                branch,
                BranchOperandKind::Existing,
            )?;
            if store.read_ref(&refname)?.is_none() {
                eprintln!("error: no branch named '{branch}'");
                return Err(GitError::Exit(1));
            }
            branch
        }
        _ => {
            eprintln!("fatal: cannot edit description of more than one branch");
            return Err(GitError::Exit(128));
        }
    };

    // On-disk only so command-line `-c` values are not rewritten into the file.
    let mut config = read_repo_config_on_disk(git_dir)?;
    let existing = config
        .get("branch", Some(&branch), "description")
        .unwrap_or("");
    let path = git_dir.join("EDIT_DESCRIPTION");
    fs::write(&path, existing)?;
    commands::replay::launch_editor(git_dir, &path)?;
    let description = fs::read_to_string(&path)?;
    let _ = fs::remove_file(&path);
    if description.is_empty() {
        unset_branch_description(&mut config, &branch);
    } else {
        set_config_value(
            &mut config,
            "branch",
            Some(&branch),
            "description",
            &description,
        );
    }
    write_repo_config(git_dir, &config)
}

pub(super) fn unset_branch_description(config: &mut GitConfig, branch: &str) {
    for section in &mut config.sections {
        if section.name == "branch" && section.subsection.as_deref() == Some(branch) {
            section
                .entries
                .retain(|entry| !entry.key.eq_ignore_ascii_case("description"));
        }
    }
    config
        .sections
        .retain(|section| !(section.name == "branch" && section.entries.is_empty()));
}

/// The effective tracking mode, mirroring git's `enum branch_track`. When the
/// command line does not request a mode, `branch.autosetupmerge` (parsed in
/// [`config_default_track`]) selects the default — which is `Remote`, not
/// "off", so creating a branch from a remote-tracking start-point sets up
/// tracking automatically.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum EffectiveTrack {
    Never,
    Remote,
    Always,
    Explicit,
    Inherit,
    Simple,
}

/// Resolve `branch.autosetupmerge` into the default tracking mode used when the
/// command line gives no `--track`/`--no-track`. Matches git's
/// `git_default_branch_config` (environment.c).
pub(super) fn config_default_track(config: &GitConfig) -> EffectiveTrack {
    match config.get("branch", None, "autosetupmerge") {
        None => EffectiveTrack::Remote,
        Some("always") => EffectiveTrack::Always,
        Some("inherit") => EffectiveTrack::Inherit,
        Some("simple") => EffectiveTrack::Simple,
        Some(other) => {
            if config_bool_value(other) {
                EffectiveTrack::Remote
            } else {
                EffectiveTrack::Never
            }
        }
    }
}

/// git's `git_config_bool` truthiness for non-special strings.
pub(super) fn config_bool_value(value: &str) -> bool {
    match value.to_ascii_lowercase().as_str() {
        "" | "true" | "yes" | "on" => true,
        "false" | "no" | "off" | "0" => false,
        other => other.parse::<i64>().map(|n| n != 0).unwrap_or(true),
    }
}

pub(crate) fn branch_create_set_tracking(
    git_dir: &Path,
    store: &FileRefStore,
    branch: &str,
    start: Option<&String>,
    track: Option<BranchTrackMode>,
    quiet: bool,
) -> Result<()> {
    let config = read_repo_config(git_dir)?;
    let effective = match track {
        Some(BranchTrackMode::Never) => EffectiveTrack::Never,
        Some(BranchTrackMode::Direct) => EffectiveTrack::Explicit,
        Some(BranchTrackMode::Inherit) => EffectiveTrack::Inherit,
        None => config_default_track(&config),
    };
    match effective {
        EffectiveTrack::Never => Ok(()),
        EffectiveTrack::Inherit => {
            branch_create_inherit_upstream(git_dir, store, branch, start, quiet)
        }
        EffectiveTrack::Explicit => {
            // --track: track even a local start-point, and fail when it is not a branch.
            let upstream = branch_create_direct_upstream(store, start)?;
            set_branch_upstream_quiet(git_dir, store, branch, &upstream, quiet)
        }
        EffectiveTrack::Always => {
            // autosetupmerge=always tracks branch start-points, but a detached
            // HEAD or other non-branch commit-ish simply creates the branch.
            branch_create_set_tracking_if_branch(git_dir, store, branch, start, quiet)
        }
        EffectiveTrack::Remote | EffectiveTrack::Simple => {
            // Default / autosetupmerge=simple: only track when the start-point
            // is a remote-tracking branch matched by some remote's fetch
            // refspec. `simple` additionally requires the remote branch name
            // to equal the new branch name.
            let Some(start) = start else { return Ok(()) };
            let resolved = match resolve_remote_tracking_upstream(store, &config, start.as_str())? {
                Some(resolved) => resolved,
                None => return Ok(()),
            };
            if effective == EffectiveTrack::Simple {
                let tracked = resolved.merge.strip_prefix("refs/heads/");
                if tracked != Some(branch) {
                    return Ok(());
                }
            }
            install_tracking_config(git_dir, store, branch, &resolved, quiet)
        }
    }
}

pub(super) fn branch_create_set_tracking_if_branch(
    git_dir: &Path,
    store: &FileRefStore,
    branch: &str,
    start: Option<&String>,
    quiet: bool,
) -> Result<()> {
    let upstream = branch_create_direct_upstream(store, start)?;
    let config = read_repo_config(git_dir)?;
    let format = repository_object_format(git_dir)?;
    let Some(resolved) = resolve_branch_upstream(git_dir, format, store, &config, &upstream)?
    else {
        return Ok(());
    };
    if resolved.remote == "." && resolved.merge == branch_ref_name(branch)? {
        eprintln!("warning: not setting branch '{branch}' as its own upstream");
        return Ok(());
    }
    install_tracking_config(git_dir, store, branch, &resolved, quiet)
}

/// Resolve a start-point to a remote-tracking upstream, mirroring git's
/// `setup_tracking` for `BRANCH_TRACK_REMOTE`: only matches when the
/// start-point names a remote-tracking branch covered by some remote's fetch
/// refspec. Returns `None` for local branches (which the default mode must not
/// track).
///
/// The remote-tracking ref need not currently exist in this repository: git's
/// `remote_find_tracking` matches the refspec alone. That matters for
/// `branch --recurse-submodules` where a submodule often has no
/// `origin/<branch>` tip yet (t3207 remote-tracking tests).
pub(super) fn resolve_remote_tracking_upstream(
    store: &FileRefStore,
    config: &GitConfig,
    start: &str,
) -> Result<Option<ResolvedBranchUpstream>> {
    let mut matches = Vec::new();
    for remote in remote_names(config) {
        let Some((remote_ref, merge)) = branch_upstream_remote_ref(config, &remote, start) else {
            continue;
        };
        // Prefer remotes that actually have the tip, but still accept a pure
        // refspec match so recursive branch create can set tracking in a
        // submodule whose object store lacks the remote-tracking ref.
        let _ = store; // existence is optional (see module docs).
        let display = remote_ref
            .strip_prefix("refs/remotes/")
            .unwrap_or(remote_ref.as_str())
            .to_string();
        matches.push(ResolvedBranchUpstream {
            remote,
            merge,
            display,
        });
    }
    if matches.len() > 1 {
        let remote_ref = branch_tracking_ref_candidate(start);
        branch_tracking_ambiguous(&remote_ref, &matches);
        return Err(GitError::Exit(128));
    }
    Ok(matches.into_iter().next())
}

pub(super) fn branch_tracking_ambiguous(remote_ref: &str, matches: &[ResolvedBranchUpstream]) {
    eprintln!("fatal: not tracking: ambiguous information for ref '{remote_ref}'");
    eprintln!("hint: There are multiple remotes whose fetch refspecs map to the remote");
    eprintln!("hint: tracking ref '{remote_ref}':");
    for resolved in matches {
        eprintln!("hint:   {}", resolved.remote);
    }
    eprintln!("hint:");
    eprintln!("hint: This is typically a configuration error.");
    eprintln!("hint:");
    eprintln!("hint: To support setting up tracking branches, ensure that");
    eprintln!("hint: different remotes' fetch refspecs map into different");
    eprintln!("hint: tracking namespaces.");
}

/// Resolve `branch.autosetuprebase` (environment.c), returning whether the
/// newly-created branch should get `branch.<name>.rebase = true` given whether
/// its upstream is on a remote (`is_remote`). Errors on a malformed value, like
/// git's `git branch` does.
pub(super) fn should_setup_rebase(config: &GitConfig, is_remote: bool) -> Result<bool> {
    match validate_autosetuprebase(config)? {
        AutoRebase::Never => Ok(false),
        AutoRebase::Local => Ok(!is_remote),
        AutoRebase::Remote => Ok(is_remote),
        AutoRebase::Always => Ok(true),
    }
}

pub(super) fn install_tracking_config(
    git_dir: &Path,
    _store: &FileRefStore,
    branch: &str,
    resolved: &ResolvedBranchUpstream,
    quiet: bool,
) -> Result<()> {
    // Read-modify-write must use the on-disk repo config only. `read_repo_config`
    // layers command-line `-c` / `GIT_CONFIG_*` injections; writing those back
    // would persist process-local settings (e.g. `git -c checkout.defaultRemote=…`
    // leaking into `.git/config` and breaking later multi-remote DWIM).
    let effective = read_repo_config(git_dir)?;
    let mut config = read_repo_config_on_disk(git_dir)?;
    let rebasing = should_setup_rebase(&effective, resolved.remote != ".")?;
    set_config_value(
        &mut config,
        "branch",
        Some(branch),
        "remote",
        &resolved.remote,
    );
    set_config_value(
        &mut config,
        "branch",
        Some(branch),
        "merge",
        &resolved.merge,
    );
    if rebasing {
        set_config_value(&mut config, "branch", Some(branch), "rebase", "true");
    }
    write_branch_repo_config(git_dir, &config)?;
    if !quiet {
        if rebasing {
            println!(
                "branch '{branch}' set up to track '{}' by rebasing.",
                resolved.display
            );
        } else {
            println!("branch '{branch}' set up to track '{}'.", resolved.display);
        }
    }
    Ok(())
}

pub(super) fn branch_create_direct_upstream(
    store: &FileRefStore,
    start: Option<&String>,
) -> Result<String> {
    match start.map(String::as_str) {
        None | Some("HEAD") => Ok(store.current_branch()?.unwrap_or_else(|| "HEAD".into())),
        Some(start) => Ok(start.to_string()),
    }
}

pub(super) fn set_branch_upstream_quiet(
    git_dir: &Path,
    store: &FileRefStore,
    branch: &str,
    upstream: &str,
    quiet: bool,
) -> Result<()> {
    let config = read_repo_config(git_dir)?;
    let format = repository_object_format(git_dir)?;
    let Some(upstream) = resolve_branch_upstream(git_dir, format, store, &config, upstream)? else {
        eprintln!("fatal: the requested upstream branch '{upstream}' does not exist");
        return Err(GitError::Exit(128));
    };
    if upstream.remote == "." && upstream.merge == branch_ref_name(branch)? {
        eprintln!("warning: not setting branch '{branch}' as its own upstream");
        return Ok(());
    }
    install_tracking_config(git_dir, store, branch, &upstream, quiet)
}

pub(super) fn branch_create_inherit_upstream(
    git_dir: &Path,
    store: &FileRefStore,
    branch: &str,
    start: Option<&String>,
    quiet: bool,
) -> Result<()> {
    // On-disk only: never persist command-line `-c` injections (see
    // `install_tracking_config`).
    let config = read_repo_config_on_disk(git_dir)?;
    let source = branch_create_inherit_source(store, start)?;
    let Some(remote) = config
        .get("branch", Some(&source.name), "remote")
        .map(str::to_string)
    else {
        if !quiet {
            eprintln!(
                "warning: asked to inherit tracking from '{}', but no remote is set",
                source.display
            );
        }
        return Ok(());
    };
    let Some(merge) = config
        .get("branch", Some(&source.name), "merge")
        .map(str::to_string)
    else {
        if !quiet {
            eprintln!(
                "warning: asked to inherit tracking from '{}', but no merge configuration is set",
                source.display
            );
        }
        return Ok(());
    };
    let mut config = config;
    set_config_value(&mut config, "branch", Some(branch), "remote", &remote);
    set_config_value(&mut config, "branch", Some(branch), "merge", &merge);
    write_branch_repo_config(git_dir, &config)?;
    if !quiet {
        let display = branch_tracking_display(&config, &remote, &merge);
        println!("branch '{branch}' set up to track '{display}'.");
    }
    Ok(())
}

pub(super) struct BranchInheritSource {
    name: String,
    display: String,
}

pub(super) fn branch_create_inherit_source(
    store: &FileRefStore,
    start: Option<&String>,
) -> Result<BranchInheritSource> {
    let start = start.map(String::as_str).unwrap_or("HEAD");
    if start == "HEAD"
        && let Some(branch) = store.current_branch()?
    {
        return Ok(BranchInheritSource {
            name: branch.clone(),
            display: branch,
        });
    }
    if let Some(branch) = start.strip_prefix("refs/heads/") {
        return Ok(BranchInheritSource {
            name: branch.to_string(),
            display: branch.to_string(),
        });
    }
    if start.starts_with("refs/remotes/") {
        return Ok(BranchInheritSource {
            name: start.to_string(),
            display: start.to_string(),
        });
    }
    let remote_ref = format!("refs/remotes/{start}");
    if store.read_ref(&remote_ref)?.is_some() {
        return Ok(BranchInheritSource {
            name: remote_ref.clone(),
            display: remote_ref,
        });
    }
    if store.read_ref(&branch_ref_name(start)?)?.is_some() {
        return Ok(BranchInheritSource {
            name: start.to_string(),
            display: start.to_string(),
        });
    }
    Ok(BranchInheritSource {
        name: start.to_string(),
        display: start.to_string(),
    })
}

pub(super) fn branch_tracking_display(config: &GitConfig, remote: &str, merge: &str) -> String {
    if remote == "." {
        return merge
            .strip_prefix("refs/heads/")
            .unwrap_or(merge)
            .to_string();
    }
    if let Some(fetch) = config.get("remote", Some(remote), "fetch")
        && let Some(refname) = map_remote_fetch_refspec(fetch, merge)
        && let Some(short) = refname.strip_prefix("refs/remotes/")
    {
        return short.to_string();
    }
    format!(
        "{remote}/{}",
        merge.strip_prefix("refs/heads/").unwrap_or(merge)
    )
}

pub(super) fn resolve_branch_start(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    replace_objects: bool,
    start: &str,
) -> Result<ObjectId> {
    let peel_branch_start = |oid: ObjectId| -> Result<ObjectId> {
        // git stores the peeled commit when branching from an annotated tag
        // (e.g. `git branch topic v1.0`), not the tag object itself.
        let db = FileObjectDatabase::from_git_dir(git_dir, format);
        sley_rev::peel_to_commit(&db, format, &oid).map_err(|_| {
            GitError::InvalidObject(format!(
                "branch start '{start}' does not resolve to a commit"
            ))
        })
    };
    match resolve_revision(git_dir, format, start, replace_objects) {
        Ok(oid) => peel_branch_start(oid),
        Err(err) => {
            // A trailing range operator with an empty other side (`main..`,
            // `main...`) resolves to the named committish, exactly as git's
            // `get_oid_committish` does (t3200 #9).
            if let Some(base) = start
                .strip_suffix("...")
                .or_else(|| start.strip_suffix(".."))
                && !base.is_empty()
                && !base.contains("..")
                && let Ok(oid) = resolve_revision(git_dir, format, base, replace_objects)
            {
                return peel_branch_start(oid);
            }
            let remote_ref = format!("refs/remotes/{start}");
            match store.read_ref(&remote_ref)? {
                Some(RefTarget::Direct(oid)) => peel_branch_start(oid),
                _ => {
                    let remote_head = format!("{remote_ref}/HEAD");
                    if let Some(RefTarget::Symbolic(target)) = store.read_ref(&remote_head)?
                        && store.read_ref(&target)?.is_none()
                    {
                        eprintln!("fatal: dangling symref {remote_head}");
                        return Err(GitError::Exit(128));
                    }
                    Err(err)
                }
            }
        }
    }
}

pub(crate) fn create_branch_from_start(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    config: &GitConfig,
    replace_objects: bool,
    branch: &str,
    start: Option<&String>,
) -> Result<()> {
    create_branch_from_start_with_reflog(
        git_dir,
        format,
        store,
        config,
        replace_objects,
        branch,
        start,
        false,
    )
}

pub(super) fn create_branch_from_start_with_reflog(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    config: &GitConfig,
    replace_objects: bool,
    branch: &str,
    start: Option<&String>,
    create_reflog: bool,
) -> Result<()> {
    let refname = validate_branch_creation_name(branch)?;
    if store.read_ref(&refname)?.is_some() {
        eprintln!("fatal: a branch named '{branch}' already exists");
        return Err(GitError::Exit(128));
    }
    let start_rev = start.map_or("HEAD", String::as_str);
    let start_oid = resolve_branch_start(git_dir, format, store, replace_objects, start_rev)?;
    let message = branch_create_reflog_message(store, start)?;
    let reflog = if branch_should_write_reflog(git_dir, &refname, create_reflog)? {
        Some(ReflogEntry {
            old_oid: ObjectId::null(format),
            new_oid: start_oid,
            committer: commit_identity_from_env("COMMITTER", config)?,
            message,
        })
    } else {
        None
    };
    let mut tx = store.transaction();
    tx.update(RefUpdate {
        name: refname,
        expected: None,
        new: RefTarget::Direct(start_oid),
        reflog,
    });
    tx.commit()?;
    Ok(())
}

pub(super) fn branch_should_write_reflog(
    git_dir: &Path,
    name: &str,
    create_reflog: bool,
) -> Result<bool> {
    if create_reflog || branch_reflog_path(git_dir, name)?.exists() {
        return Ok(true);
    }
    if let Some(value) = global_config_value("core.logAllRefUpdates")? {
        return Ok(branch_log_all_ref_updates_matches(name, &value));
    }
    let common_git_dir = common_git_dir_for_git_dir(git_dir)?;
    let Ok(config) = GitConfig::read(common_git_dir.join("config")) else {
        return Ok(false);
    };
    if let Some(value) = config.get("core", None, "logAllRefUpdates") {
        return Ok(branch_log_all_ref_updates_matches(name, value));
    }
    if config.get_bool("core", None, "bare").unwrap_or(false) {
        return Ok(false);
    }
    Ok(branch_log_all_ref_updates_matches(name, "true"))
}

pub(super) fn branch_create_reflog_message(
    store: &FileRefStore,
    start: Option<&String>,
) -> Result<Vec<u8>> {
    let display = match start {
        Some(start) => start.clone(),
        None => store.current_branch()?.unwrap_or_else(|| "HEAD".into()),
    };
    Ok(format!("branch: Created from {display}").into_bytes())
}

pub(super) fn branch_reset_reflog_message(
    store: &FileRefStore,
    start: Option<&String>,
) -> Result<Vec<u8>> {
    let display = match start {
        Some(start) => start.clone(),
        None => store.current_branch()?.unwrap_or_else(|| "HEAD".into()),
    };
    Ok(format!("branch: Reset to {display}").into_bytes())
}

pub(super) fn branch_reflog_path(git_dir: &Path, name: &str) -> Result<PathBuf> {
    let common_git_dir = common_git_dir_for_git_dir(git_dir)?;
    Ok(common_git_dir.join("logs").join(name))
}

pub(super) fn branch_log_all_ref_updates_matches(name: &str, value: &str) -> bool {
    if value.eq_ignore_ascii_case("always") {
        return true;
    }
    if !sley_config::parse_config_bool(value).unwrap_or(false) {
        return false;
    }
    name == "HEAD"
        || name.starts_with("refs/heads/")
        || name.starts_with("refs/remotes/")
        || name.starts_with("refs/notes/")
}
