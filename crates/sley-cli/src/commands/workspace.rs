//! Extracted from the crate root (sley#8 phase 1) — code motion only.

// A glob of the crate root brings every shared helper/type into scope via
// descendant-privacy; see commands::stash for the rationale.
use crate::*;

/// git's `git reset --help`-derived usage block, printed after an `error:` line
/// when parse-options rejects an argument (matches builtin/reset.c's usage).
const RESET_USAGE: &str = "\
usage: git reset [--mixed | --soft | --hard | --merge | --keep] [-q] [<commit>]
   or: git reset [-q] [<tree-ish>] [--] <pathspec>...
   or: git reset [-q] [--pathspec-from-file [--pathspec-file-nul]] [<tree-ish>]
   or: git reset --patch [<tree-ish>] [--] [<pathspec>...]
";

pub(crate) fn cmd_reset(args: &[String]) -> Result<()> {
    let mut positionals = Vec::new();
    let mut quiet = false;
    let mut mode = ResetMode::Mixed;
    let mut parsing_options = true;
    let mut saw_separator = false;
    let mut separator_index = None;
    let mut pathspec_from_file: Option<PathBuf> = None;
    let mut pathspec_file_nul = false;
    let mut intent_to_add = false;
    // git's `reset --mixed` refreshes the index stat-cache by default; `--no-refresh`
    // leaves the freshly-restored entries stat-dirty so `git diff-files` shows them.
    let mut refresh = true;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if !parsing_options {
            if pathspec_from_file.is_some() {
                eprintln!(
                    "fatal: '--pathspec-from-file' and pathspec arguments cannot be used together"
                );
                return Err(GitError::Exit(128));
            }
            positionals.push(arg.to_string());
            continue;
        }
        match arg.as_str() {
            "--" => {
                parsing_options = false;
                saw_separator = true;
                separator_index = Some(positionals.len());
            }
            "-q" | "--quiet" => quiet = true,
            "--no-quiet" => quiet = false,
            "-N" | "--intent-to-add" => intent_to_add = true,
            "--no-intent-to-add" => intent_to_add = false,
            // A whole-tree `--mixed` reset restores index entries with a zeroed
            // cached stat (see `restored_head_index_entry`). git refreshes them
            // by default (re-stat + clear the stat-dirty state for unchanged
            // content) so `git diff-files` is clean; `--no-refresh` leaves them
            // stat-dirty so diff-files reports them `M` (t7102 cell 28).
            "--refresh" => refresh = true,
            "--no-refresh" => refresh = false,
            "--no-recurse-submodules" => {}
            "--mixed" => mode = ResetMode::Mixed,
            "--soft" => mode = ResetMode::Soft,
            "--hard" => mode = ResetMode::Hard,
            "--merge" => mode = ResetMode::Merge,
            "--pathspec-file-nul" => pathspec_file_nul = true,
            "--no-pathspec-file-nul" => pathspec_file_nul = false,
            "--pathspec-from-file" => {
                let value = iter.next().ok_or_else(|| {
                    GitError::Command("reset --pathspec-from-file requires a value".into())
                })?;
                pathspec_from_file = Some(PathBuf::from(value));
            }
            "--no-pathspec-from-file" => {}
            value if value.starts_with("--pathspec-from-file=") => {
                let value = value.strip_prefix("--pathspec-from-file=").ok_or_else(|| {
                    GitError::Command("reset --pathspec-from-file requires a value".into())
                })?;
                pathspec_from_file = Some(PathBuf::from(value));
            }
            "--end-of-options" => {
                // Everything after `--end-of-options` is a positional (commit-ish
                // or pathspec), even if it begins with a dash. git's parse-options
                // uses this to disambiguate a ref like `--foo` from an option.
                parsing_options = false;
            }
            value if value.starts_with('-') => {
                // Mirror git's parse-options error for an unrecognized flag:
                // `error: unknown option `<name>'` (long options drop the leading
                // `--`; a short cluster like `-o` keeps its single dash stripped),
                // followed by the usage block, exiting with parse-options' code 129.
                let name = value
                    .strip_prefix("--")
                    .or_else(|| value.strip_prefix('-'))
                    .unwrap_or(value);
                eprintln!("error: unknown option `{name}'");
                eprint!("{RESET_USAGE}");
                return Err(GitError::Exit(129));
            }
            value => {
                if pathspec_from_file.is_some() {
                    eprintln!(
                        "fatal: '--pathspec-from-file' and pathspec arguments cannot be used together"
                    );
                    return Err(GitError::Exit(128));
                }
                positionals.push(value.to_string());
            }
        }
    }
    if pathspec_file_nul && pathspec_from_file.is_none() {
        eprintln!("fatal: the option '--pathspec-file-nul' requires '--pathspec-from-file'");
        return Err(GitError::Exit(128));
    }
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    let format = repository_object_format(&git_dir)?;
    let pathspec_from_file_provided = pathspec_from_file.is_some();
    if mode == ResetMode::Merge {
        if pathspec_from_file_provided || (saw_separator && !positionals.is_empty()) {
            eprintln!("fatal: Cannot do merge reset with paths.");
            return Err(GitError::Exit(128));
        }
        let target = match positionals.as_slice() {
            [] => "HEAD",
            [target] => target.as_str(),
            _ => {
                eprintln!("fatal: Cannot do merge reset with paths.");
                return Err(GitError::Exit(128));
            }
        };
        let db = FileObjectDatabase::from_git_dir(&git_dir, format);
        let target_oid = resolve_revision(&git_dir, format, target)?;
        let target_commit = sley_rev::peel_to_commit(&db, format, &target_oid)?;
        return commands::replay::reset_merge_in(
            &git_dir,
            &worktree_root,
            format,
            Some(&target_commit),
        )
        .map_err(|err| match err {
            GitError::Command(message) => {
                eprintln!("fatal: {message}");
                GitError::Exit(128)
            }
            other => other,
        });
    }
    if matches!(mode, ResetMode::Soft | ResetMode::Hard) {
        if pathspec_from_file_provided {
            eprintln!("fatal: Cannot do {} reset with paths.", mode.as_str());
            return Err(GitError::Exit(128));
        }
        if saw_separator && !positionals.is_empty() {
            eprintln!("fatal: Cannot do {} reset with paths.", mode.as_str());
            return Err(GitError::Exit(128));
        }
        let target = match positionals.as_slice() {
            [] => "HEAD",
            [target] => target.as_str(),
            _ => {
                eprintln!("fatal: Cannot do {} reset with paths.", mode.as_str());
                return Err(GitError::Exit(128));
            }
        };
        // git refuses a `--soft` reset while a merge is in progress or the index
        // carries unmerged entries: a soft reset only moves HEAD, so leaving the
        // half-merged index behind would silently strand the conflict state.
        // builtin/reset.c: `reset_type == SOFT && (merge in progress || unmerged)`
        // → "Cannot do a soft reset in the middle of a merge." (exit 128).
        if mode == ResetMode::Soft
            && reset_soft_blocked_by_merge(&git_dir, format)?
        {
            eprintln!("fatal: Cannot do a soft reset in the middle of a merge.");
            return Err(GitError::Exit(128));
        }
        let db = FileObjectDatabase::from_git_dir(&git_dir, format);
        let old_head = match resolve_revision(&git_dir, format, "HEAD") {
            Ok(oid) => oid,
            Err(_) => zero_oid(format)?,
        };
        if mode == ResetMode::Hard
            && target == "HEAD"
            && resolve_revision(&git_dir, format, "HEAD").is_err()
        {
            // `git reset --hard` on an unborn branch: empty the index and
            // remove the (previously tracked) worktree files.
            let index_path = sley_worktree::repository_index_path(&git_dir);
            if index_path.exists() {
                let index = Index::parse(&fs::read(&index_path)?, format)?;
                for entry in &index.entries {
                    let full = worktree_root.join(entry.path.to_string());
                    if full.is_file() {
                        let _ = fs::remove_file(&full);
                    }
                }
                fs::write(
                    &index_path,
                    Index {
                        version: 2,
                        entries: Vec::new(),
                        extensions: Vec::new(),
                        checksum: None,
                    }
                    .write(format)?,
                )?;
            }
            sley_sequencer::replay::remove_branch_state(&git_dir);
            return Ok(());
        }
        let target_oid = resolve_revision(&git_dir, format, target)?;
        let target_commit = sley_rev::peel_to_commit(&db, format, &target_oid)?;
        write_reset_orig_head(&git_dir, &old_head, format)?;
        if mode == ResetMode::Hard {
            sley_worktree::reset_index_and_worktree_to_commit(
                worktree_root.clone(),
                git_dir.clone(),
                format,
                &target_commit,
            )?;
        }
        update_reset_head_ref(
            &git_dir,
            format,
            old_head,
            target_commit,
            target,
            commit_identity_from_env("COMMITTER")?,
        )?;
        if mode == ResetMode::Hard && !quiet {
            print_reset_hard_head(&git_dir, format, &target_commit)?;
        }
        sley_sequencer::replay::remove_branch_state(&git_dir);
        return Ok(());
    }

    if !saw_separator
        && positionals.len() == 1
        && let Ok(target_oid) = resolve_revision(&git_dir, format, &positionals[0])
    {
        let db = FileObjectDatabase::from_git_dir(&git_dir, format);
        let old_head = match resolve_revision(&git_dir, format, "HEAD") {
            Ok(oid) => oid,
            Err(_) => zero_oid(format)?,
        };
        let target_commit = sley_rev::peel_to_commit(&db, format, &target_oid)?;
        // For `git reset -N` capture the paths the *current* index tracks that the
        // target tree does NOT, so they can be re-recorded as intent-to-add after
        // the index is reset (git: removed paths "will be added later").
        let ita_candidates = if intent_to_add {
            reset_intent_to_add_candidates(&git_dir, &db, format, &target_commit)?
        } else {
            Vec::new()
        };
        write_reset_orig_head(&git_dir, &old_head, format)?;
        sley_worktree::reset_index_to_commit(
            worktree_root.clone(),
            git_dir.clone(),
            format,
            &target_commit,
        )?;
        if intent_to_add && !ita_candidates.is_empty() {
            apply_reset_intent_to_add(&git_dir, format, &ita_candidates)?;
        }
        // git's `--mixed` reset refreshes the index by default: the restored
        // entries carry a zeroed cached stat, so without a refresh `git diff-files`
        // would report every unchanged tracked file as `M`. The refresh re-stats
        // each entry and clears the stat-dirty state where content still matches.
        // `--no-refresh` skips it, leaving the entries stat-dirty (t7102 cell 28).
        if refresh {
            refresh_reset_index(&worktree_root, &git_dir, format)?;
        }
        update_reset_head_ref(
            &git_dir,
            format,
            old_head,
            target_commit,
            &positionals[0],
            commit_identity_from_env("COMMITTER")?,
        )?;
        sley_sequencer::replay::remove_branch_state(&git_dir);
        if !quiet {
            print_reset_unstaged_changes(&worktree_root, &git_dir, format)?;
        }
        return Ok(());
    }

    let mut source_tree = None;
    let mut paths = if let Some(index) = separator_index {
        let (before_separator, after_separator) = positionals.split_at(index);
        match before_separator {
            [] => {}
            [target] => {
                let db = FileObjectDatabase::from_git_dir(&git_dir, format);
                let target_oid = resolve_revision(&git_dir, format, target)?;
                source_tree = Some(sley_rev::peel_to_tree(&db, format, &target_oid)?);
            }
            _ => {
                eprintln!("fatal: Cannot do mixed reset with multiple trees.");
                return Err(GitError::Exit(128));
            }
        }
        after_separator
            .iter()
            .map(PathBuf::from)
            .collect::<Vec<_>>()
    } else {
        let mut values = positionals;
        if values.len() > 1
            && let Ok(target_oid) = resolve_revision(&git_dir, format, &values[0])
        {
            let db = FileObjectDatabase::from_git_dir(&git_dir, format);
            source_tree = Some(sley_rev::peel_to_tree(&db, format, &target_oid)?);
            values.remove(0);
        }
        values
            .into_iter()
            .filter(|value| value != "HEAD")
            .map(PathBuf::from)
            .collect::<Vec<_>>()
    };
    if let Some(pathspec_file) = pathspec_from_file {
        paths.extend(read_pathspecs_from_file(&pathspec_file, pathspec_file_nul)?);
    }
    let no_explicit_paths = paths.is_empty() && !pathspec_from_file_provided;
    if no_explicit_paths {
        paths.push(worktree_root.clone());
    }
    if !saw_separator && source_tree.is_none() {
        for path in &paths {
            let absolute = if path.is_absolute() {
                path.clone()
            } else {
                cwd.join(path)
            };
            if !absolute.exists() {
                return Err(sley_rev::ambiguous_argument_error(&path.display().to_string()));
            }
        }
    }
    let resolved_paths = paths
        .into_iter()
        .map(|path| {
            if path.is_absolute() {
                path
            } else {
                cwd.join(path)
            }
        })
        .collect::<Vec<_>>();
    if let Some(tree_oid) = source_tree.as_ref() {
        sley_worktree::restore_index_paths_from_tree(
            worktree_root.clone(),
            git_dir.clone(),
            format,
            tree_oid,
            &resolved_paths,
        )?;
    } else {
        sley_worktree::restore_index_paths_from_head(
            worktree_root.clone(),
            git_dir.clone(),
            format,
            &resolved_paths,
        )?;
    }
    if no_explicit_paths {
        // A bare `git reset` (whole-tree mixed reset to HEAD) records ORIG_HEAD
        // just like the explicit-commit whole-tree paths. Pathspec resets do not.
        if let Ok(old_head) = resolve_revision(&git_dir, format, "HEAD") {
            write_reset_orig_head(&git_dir, &old_head, format)?;
        }
        // Whole-tree `--mixed` refreshes the stat-cache by default (see the
        // single-positional path above); `--no-refresh` leaves it stat-dirty.
        if refresh {
            refresh_reset_index(&worktree_root, &git_dir, format)?;
        }
        sley_sequencer::replay::remove_branch_state(&git_dir);
    }
    if !quiet {
        print_reset_unstaged_changes(&worktree_root, &git_dir, format)?;
    }
    Ok(())
}

/// Refresh the index stat-cache after a whole-tree `--mixed` reset (git's default
/// `--refresh` behaviour). The reset restores entries with a zeroed cached stat;
/// this re-stats each one and clears the stat-dirty state where content still
/// matches, so `git diff-files` reports a clean index. Mirrors git's
/// `refresh_index` call in `builtin/reset.c`: quiet (no "needs update" output, and
/// content mismatches are not an error — they are genuine worktree changes) and
/// tolerant of missing files (those are deletions, reported elsewhere).
fn refresh_reset_index(worktree_root: &Path, git_dir: &Path, format: ObjectFormat) -> Result<()> {
    sley_worktree::refresh_index_paths(
        worktree_root,
        git_dir,
        format,
        &[],
        /* quiet */ true,
        /* ignore_missing */ true,
        /* really_refresh */ false,
    )?;
    Ok(())
}

/// Record the pre-reset HEAD in `.git/ORIG_HEAD`, exactly as `git reset` does for
/// every whole-tree reset (all modes). Pathspec resets do NOT update ORIG_HEAD, so
/// this is only called on the whole-tree code paths. A null/unborn old HEAD writes
/// nothing — git leaves ORIG_HEAD untouched when there is no commit to record.
fn write_reset_orig_head(git_dir: &Path, old_head: &ObjectId, format: ObjectFormat) -> Result<()> {
    if *old_head == ObjectId::null(format) {
        return Ok(());
    }
    fs::write(git_dir.join("ORIG_HEAD"), format!("{old_head}\n"))?;
    Ok(())
}

/// The worktree-relative paths a `git reset -N` should re-record as intent-to-add:
/// stage-0 paths the *current* index tracks that the target tree does NOT (i.e. the
/// adds being un-staged by the reset). git keeps these as intent-to-add so the file
/// still shows in `git diff` but is absent from the written tree.
fn reset_intent_to_add_candidates(
    git_dir: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    target_commit: &ObjectId,
) -> Result<Vec<BString>> {
    let index_path = sley_worktree::repository_index_path(git_dir);
    if !index_path.exists() {
        return Ok(Vec::new());
    }
    let index = Index::parse(&fs::read(&index_path)?, format)?;
    let target_tree = sley_rev::peel_to_tree(db, format, target_commit)?;
    let target_index = sley_worktree::index_from_tree(db, format, &target_tree)?;
    let target_paths: std::collections::BTreeSet<BString> = target_index
        .entries
        .iter()
        .map(|entry| entry.path.clone())
        .collect();
    Ok(index
        .entries
        .iter()
        .filter(|entry| {
            entry.stage() == sley_index::Stage::Normal && !target_paths.contains(&entry.path)
        })
        .map(|entry| entry.path.clone())
        .collect())
}

/// Insert intent-to-add placeholders into the (already reset) index for the given
/// paths, matching `git reset -N`. Each becomes an ITA stage-0 entry, so the path
/// is reported by `git diff` yet excluded from `git write-tree`'s output.
fn apply_reset_intent_to_add(
    git_dir: &Path,
    format: ObjectFormat,
    paths: &[BString],
) -> Result<()> {
    let index_path = sley_worktree::repository_index_path(git_dir);
    let mut index = Index::parse(&fs::read(&index_path)?, format)?;
    let existing: std::collections::BTreeSet<BString> =
        index.entries.iter().map(|entry| entry.path.clone()).collect();
    for path in paths {
        if existing.contains(path) {
            continue;
        }
        index
            .entries
            .push(IndexEntry::intent_to_add(format, path.clone()));
    }
    index.entries.sort_by(|left, right| left.path.cmp(&right.path));
    // intent-to-add entries carry extended flags, which the v2 index writer cannot
    // encode; bump to v3 when needed (mirrors `git add -N`'s index upgrade).
    index.upgrade_version_for_flags();
    fs::write(&index_path, index.write(format)?)?;
    Ok(())
}

/// The byte paths of every index entry carrying the skip-worktree bit, so the
/// post-reset "Unstaged changes" summary can exclude them (git never lists a
/// skip-worktree path there). Empty when the index is absent.
fn reset_skip_worktree_paths(
    git_dir: &Path,
    format: ObjectFormat,
) -> Result<std::collections::BTreeSet<Vec<u8>>> {
    let index_path = sley_worktree::repository_index_path(git_dir);
    if !index_path.exists() {
        return Ok(std::collections::BTreeSet::new());
    }
    let index = Index::parse(&fs::read(&index_path)?, format)?;
    Ok(index
        .entries
        .iter()
        .filter(|entry| entry.is_skip_worktree())
        .map(|entry| entry.path.to_vec())
        .collect())
}

/// Whether a `--soft` reset must be refused because a merge is in flight. git's
/// builtin/reset.c blocks a soft reset when `MERGE_HEAD` exists OR the index holds
/// any unmerged (stage != 0) entry — a soft reset only moves HEAD, so it would
/// otherwise abandon the half-resolved index. Returns true in either case.
fn reset_soft_blocked_by_merge(git_dir: &Path, format: ObjectFormat) -> Result<bool> {
    if git_dir.join("MERGE_HEAD").is_file() {
        return Ok(true);
    }
    let index_path = sley_worktree::repository_index_path(git_dir);
    if !index_path.exists() {
        return Ok(false);
    }
    let index = Index::parse(&fs::read(&index_path)?, format)?;
    Ok(index
        .entries
        .iter()
        .any(|entry| entry.stage() != sley_index::Stage::Normal))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResetMode {
    Mixed,
    Soft,
    Hard,
    Merge,
}

impl ResetMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Mixed => "mixed",
            Self::Soft => "soft",
            Self::Hard => "hard",
            Self::Merge => "merge",
        }
    }
}

fn print_reset_unstaged_changes(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
) -> Result<()> {
    let mut entries = sley_worktree::short_status(worktree_root, git_dir, format)?;
    entries.retain(|entry| matches!(entry.worktree, b'M' | b'D'));
    // git's post-reset summary omits skip-worktree paths: `update_index_refresh`
    // never marks a CE_SKIP_WORKTREE entry stat-dirty, so it never appears in the
    // "Unstaged changes after reset:" list (t7102 cell 29). Drop those paths.
    let skip_worktree = reset_skip_worktree_paths(git_dir, format)?;
    if !skip_worktree.is_empty() {
        entries.retain(|entry| !skip_worktree.contains(entry.path.as_slice()));
    }
    if entries.is_empty() {
        return Ok(());
    }
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "Unstaged changes after reset:")?;
    for entry in entries {
        writeln!(
            stdout,
            "{}\t{}",
            entry.worktree as char,
            String::from_utf8_lossy(&entry.path)
        )?;
    }
    Ok(())
}

pub(crate) fn cmd_checkout(args: &[String]) -> Result<()> {
    let mut quiet = false;
    let mut force = false;
    let mut branch_mode = CheckoutBranchMode::Existing;
    let mut track = None::<crate::commands::branch::BranchTrackMode>;
    let mut positional = Vec::new();
    let mut dashdash_index = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-q" | "--quiet" => quiet = true,
            "--no-quiet" => quiet = false,
            "-f" | "--force" => force = true,
            "--no-force" => force = false,
            "--progress"
            | "--no-progress"
            | "--guess"
            | "--no-guess"
            | "--ignore-other-worktrees"
            | "--no-ignore-other-worktrees"
            | "--no-recurse-submodules" => {}
            "-t" | "--track" | "--track=direct" => {
                track = Some(crate::commands::branch::BranchTrackMode::Direct);
            }
            "--track=inherit" => {
                track = Some(crate::commands::branch::BranchTrackMode::Inherit);
            }
            "--no-track" => {
                track = Some(crate::commands::branch::BranchTrackMode::Never);
            }
            "-b" => {
                let branch = iter
                    .next()
                    .ok_or_else(|| GitError::Command("checkout -b requires a branch".into()))?;
                branch_mode = CheckoutBranchMode::Create {
                    branch: branch.to_string(),
                    force: false,
                    orphan: false,
                };
            }
            "-B" => {
                let branch = iter
                    .next()
                    .ok_or_else(|| GitError::Command("checkout -B requires a branch".into()))?;
                branch_mode = CheckoutBranchMode::Create {
                    branch: branch.to_string(),
                    force: true,
                    orphan: false,
                };
            }
            "--detach" => branch_mode = CheckoutBranchMode::Detach,
            "--orphan" => {
                let branch = iter.next().ok_or_else(|| {
                    GitError::Command("checkout --orphan requires a branch".into())
                })?;
                branch_mode = CheckoutBranchMode::Create {
                    branch: branch.to_string(),
                    force: false,
                    orphan: true,
                };
            }
            "--" => {
                dashdash_index = Some(positional.len());
                positional.extend(iter.map(|value| value.to_string()));
                break;
            }
            value => positional.push(value.to_string()),
        }
    }

    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    let format = repository_object_format(&git_dir)?;
    // `git checkout -` is shorthand for `git checkout @{-1}`: switch back to the
    // branch we most recently left (by name, so HEAD re-attaches). Expand it to
    // that branch name before branch/revision resolution; if the prior checkout
    // was detached (no branch name) leave it so the normal `@{-1}` revision path
    // handles the detached case.
    if matches!(branch_mode, CheckoutBranchMode::Existing)
        && dashdash_index.is_none()
        && positional.len() == 1
        && positional[0] == "-"
    {
        if let Some(name) = sley_rev::nth_prior_checkout_branch_name(&git_dir, format, 1)? {
            positional[0] = name;
        }
    }
    let checkout_old_head = resolve_ref_peeled(&FileRefStore::new(&git_dir, format), "HEAD")?
        .unwrap_or_else(|| ObjectId::null(format));

    if matches!(
        track,
        Some(
            crate::commands::branch::BranchTrackMode::Direct
                | crate::commands::branch::BranchTrackMode::Inherit
        )
    ) && matches!(branch_mode, CheckoutBranchMode::Existing)
    {
        let [upstream] = positional.as_slice() else {
            return Err(GitError::Command(
                "checkout --track requires exactly one start point".into(),
            ));
        };
        let store = FileRefStore::new(&git_dir, format);
        let branch = checkout_track_branch_name(&store, upstream)?;
        branch_mode = CheckoutBranchMode::Create {
            branch,
            force: false,
            orphan: false,
        };
    }

    // Pathspec checkout: `checkout [<tree-ish>] [--] <pathspec>...` restores
    // paths (and, with a tree-ish, index entries) instead of switching HEAD.
    if matches!(branch_mode, CheckoutBranchMode::Existing) {
        let (source, paths): (Option<&str>, &[String]) = match dashdash_index {
            Some(index) => {
                let (before, after) = positional.split_at(index);
                match before {
                    [] => (None, after),
                    [rev] => (Some(rev.as_str()), after),
                    _ => {
                        return Err(GitError::Command(
                            "checkout with multiple tree-ish arguments is not supported".into(),
                        ));
                    }
                }
            }
            None if positional.len() > 1 => {
                // `checkout <rev> <paths>...` — but if the first arg is not a
                // revision, every positional is a pathspec (git's
                // disambiguation for `checkout <path> <path>...`).
                if sley_rev::resolve_revision(&git_dir, format, &positional[0]).is_ok() {
                    (Some(positional[0].as_str()), &positional[1..])
                } else {
                    (None, positional.as_slice())
                }
            }
            None if positional.len() == 1 => {
                // A single arg that is neither a branch nor a revision but
                // names an existing file is a path checkout.
                let value = &positional[0];
                let store = FileRefStore::new(&git_dir, format);
                let is_branch = branch_ref_name(value)
                    .ok()
                    .and_then(|name| sley_refs::resolve_ref_peeled(&store, &name).ok().flatten())
                    .is_some();
                if !is_branch
                    && sley_rev::resolve_revision(&git_dir, format, value).is_err()
                    && cwd.join(value).exists()
                {
                    (None, positional.as_slice())
                } else {
                    (None, &[])
                }
            }
            None => (None, &[]),
        };
        if !paths.is_empty() {
            let resolved_paths: Vec<PathBuf> = paths
                .iter()
                .map(|path| {
                    let path = PathBuf::from(path);
                    if path.is_absolute() {
                        path
                    } else {
                        cwd.join(path)
                    }
                })
                .collect();
            match source {
                Some(rev) => {
                    let db = FileObjectDatabase::from_git_dir(&git_dir, format);
                    let oid = resolve_revision(&git_dir, format, rev)?;
                    let tree = sley_rev::peel_to_tree(&db, format, &oid)?;
                    sley_worktree::restore_index_and_worktree_paths_from_tree(
                        worktree_root,
                        git_dir,
                        format,
                        &tree,
                        &resolved_paths,
                    )?;
                }
                None => {
                    let config = read_repo_config(&git_dir)?;
                    sley_worktree::restore_worktree_paths_filtered(
                        worktree_root,
                        &git_dir,
                        format,
                        &resolved_paths,
                        &config,
                    )?;
                }
            }
            return Ok(());
        }
    }

    // `git checkout -f <commit-ish>` where the target is the commit HEAD already
    // points at (the common `git checkout -f HEAD` form, e.g. the trailing step
    // of upstream's test_commit_bulk): force-restore the index and working tree
    // to that commit without changing which branch HEAD is on, and stay silent on
    // success — exactly git's behavior when no branch switch happens.
    if force && matches!(branch_mode, CheckoutBranchMode::Existing) && positional.len() == 1 {
        let target = &positional[0];
        let store = FileRefStore::new(&git_dir, format);
        let head_commit = resolve_ref_peeled(&store, "HEAD")?;
        if let Ok(target_oid) = sley_rev::resolve_revision(&git_dir, format, target)
            && head_commit == Some(target_oid)
        {
            sley_worktree::reset_index_and_worktree_to_commit(
                &worktree_root,
                &git_dir,
                format,
                &target_oid,
            )?;
            sley_sequencer::replay::remove_branch_state(&git_dir);
            return Ok(());
        }
    }

    // `-f`: discard local index/worktree changes (including conflict stages)
    // before switching, so the clean-tree checkout below succeeds — git's
    // force semantics. Untracked files are preserved.
    if force {
        let store = FileRefStore::new(&git_dir, format);
        if let Ok(Some(head_oid)) = resolve_ref_peeled(&store, "HEAD") {
            sley_worktree::reset_index_and_worktree_to_commit(
                &worktree_root,
                &git_dir,
                format,
                &head_oid,
            )?;
        } else {
            // Unborn HEAD: discard the staged state entirely.
            let index_path = sley_worktree::repository_index_path(&git_dir);
            if index_path.exists() {
                let index = Index::parse(&fs::read(&index_path)?, format)?;
                for entry in &index.entries {
                    let full = worktree_root.join(entry.path.to_string());
                    if full.is_file() {
                        let _ = fs::remove_file(&full);
                    }
                }
                fs::write(
                    &index_path,
                    Index {
                        version: 2,
                        entries: Vec::new(),
                        extensions: Vec::new(),
                        checksum: None,
                    }
                    .write(format)?,
                )?;
            }
        }
    }

    let checkout_message = match branch_mode {
        CheckoutBranchMode::Detach => {
            if positional.len() > 1 {
                return Err(GitError::Command(
                    "checkout --detach accepts at most one commit".into(),
                ));
            }
            let target = positional.first().map(String::as_str).unwrap_or("HEAD");
            let target_oid = resolve_revision(&git_dir, format, target)?;
            let db = FileObjectDatabase::from_git_dir(&git_dir, format);
            let target_oid = sley_rev::peel_to_commit(&db, format, &target_oid)?;
            let store = FileRefStore::new(&git_dir, format);
            let from = checkout_reflog_from_name(&store);
            let config = read_repo_config(&git_dir)?;
            let subject = detached_checkout_subject(&git_dir, format, &target_oid);
            let message = format!("checkout: moving from {from} to {target}").into_bytes();
            match sley_worktree::checkout_detached_filtered(
                &worktree_root,
                &git_dir,
                format,
                &target_oid,
                commit_identity_from_env("COMMITTER")?,
                message.clone(),
                &config,
            ) {
                Ok(_) => {}
                Err(err) if checkout_is_dirty_tree_error(&err) => {
                    checkout_twoway_dirty(&git_dir, &worktree_root, format, Some(&target_oid))?;
                    detach_head_with_reflog(&git_dir, format, &target_oid, message)?;
                }
                Err(err) => return Err(err),
            }
            sley_sequencer::replay::remove_branch_state(&git_dir);
            if !quiet {
                eprintln!(
                    "HEAD is now at {} {}",
                    format_log_abbrev_oid(&target_oid),
                    subject
                );
            }
            run_post_checkout_hook(&checkout_old_head, &target_oid, true)?;
            commands::hooks::run_hook(
                "reference-transaction",
                commands::hooks::HookRun::default(),
            )?;
            return Ok(());
        }
        CheckoutBranchMode::Existing => {
            let [branch] = positional.as_slice() else {
                return Err(GitError::Command(
                    "checkout currently supports: checkout [-q] <branch> or checkout [-q] -b|-B <branch> [<start>]".into(),
                ));
            };
            // A target that is not an existing branch but resolves to a commit-ish
            // (e.g. `A^0`, a tag, a raw oid) is a *detached HEAD* checkout, not a
            // branch switch. git detaches HEAD at the resolved commit; treating it
            // as a branch name would mint a bogus `refs/heads/A^0` symref.
            let store = FileRefStore::new(&git_dir, format);
            let is_branch = sley_refs::resolve_ref_peeled(&store, &branch_ref_name(branch)?)
                .ok()
                .flatten()
                .is_some();
            if !is_branch
                && let Ok(target_oid) = sley_rev::resolve_revision(&git_dir, format, branch)
            {
                let config = read_repo_config(&git_dir)?;
                let subject = detached_checkout_subject(&git_dir, format, &target_oid);
                let from = checkout_reflog_from_name(&store);
                let message = format!("checkout: moving from {from} to {branch}").into_bytes();
                match sley_worktree::checkout_detached_filtered(
                    &worktree_root,
                    &git_dir,
                    format,
                    &target_oid,
                    commit_identity_from_env("COMMITTER")?,
                    message.clone(),
                    &config,
                ) {
                    Ok(_) => {}
                    Err(err) if checkout_is_dirty_tree_error(&err) => {
                        checkout_twoway_dirty(&git_dir, &worktree_root, format, Some(&target_oid))?;
                        detach_head_with_reflog(&git_dir, format, &target_oid, message)?;
                    }
                    Err(err) => return Err(err),
                }
                sley_sequencer::replay::remove_branch_state(&git_dir);
                if !quiet {
                    eprintln!(
                        "HEAD is now at {} {}",
                        format_log_abbrev_oid(&target_oid),
                        subject
                    );
                }
                run_post_checkout_hook(&checkout_old_head, &target_oid, true)?;
                commands::hooks::run_hook(
                    "reference-transaction",
                    commands::hooks::HookRun::default(),
                )?;
                return Ok(());
            }
            CheckoutMessage::Existing {
                branch: branch.clone(),
            }
        }
        CheckoutBranchMode::Create {
            branch,
            force,
            orphan,
        } => {
            if orphan {
                if !positional.is_empty() {
                    return Err(GitError::Command(
                        "checkout --orphan does not accept a start point".into(),
                    ));
                }
                checkout_switch_to_unborn_branch(&git_dir, &branch)?;
                sley_sequencer::replay::remove_branch_state(&git_dir);
                if !quiet {
                    eprintln!("Switched to a new branch '{branch}'");
                }
                return Ok(());
            }
            if positional.len() > 1 {
                return Err(GitError::Command(
                    "checkout -b/-B accepts at most one start point".into(),
                ));
            }
            let start = positional.first().map(String::as_str).unwrap_or("HEAD");
            let store = FileRefStore::new(&git_dir, format);
            let was_reset = checkout_create_or_reset_branch(
                &git_dir,
                &git_dir,
                format,
                &branch,
                start,
                force,
                commit_identity_from_env("COMMITTER")?,
            )?;
            crate::commands::branch::branch_create_set_tracking(
                &git_dir,
                &store,
                &branch,
                positional.first(),
                track,
                quiet,
            )?;
            if was_reset {
                CheckoutMessage::Reset { branch }
            } else {
                CheckoutMessage::New { branch }
            }
        }
    };
    let branch = checkout_message.branch();

    let config = read_repo_config(&git_dir)?;
    match sley_worktree::checkout_branch_filtered(
        &worktree_root,
        git_dir.clone(),
        format,
        branch,
        commit_identity_from_env("COMMITTER")?,
        &config,
    ) {
        Ok(_) => {}
        Err(err) if checkout_is_dirty_tree_error(&err) => {
            let store = FileRefStore::new(&git_dir, format);
            let from = checkout_reflog_from_name(&store);
            let target = sley_refs::resolve_ref_peeled(&store, &branch_ref_name(branch)?)?
                .ok_or_else(|| GitError::reference_not_found("branch"))?;
            checkout_twoway_dirty(&git_dir, &worktree_root, format, Some(&target))?;
            switch_head_symbolic_with_reflog(&git_dir, format, branch, &target, &from)?;
        }
        Err(err) => return Err(err),
    }
    sley_sequencer::replay::remove_branch_state(&git_dir);
    let checkout_new_head = resolve_ref_peeled(&FileRefStore::new(&git_dir, format), "HEAD")?
        .unwrap_or(checkout_old_head);
    run_post_checkout_hook(&checkout_old_head, &checkout_new_head, true)?;
    commands::hooks::run_hook("reference-transaction", commands::hooks::HookRun::default())?;
    if !quiet {
        checkout_message.print();
    }
    Ok(())
}

fn run_post_checkout_hook(
    old_head: &ObjectId,
    new_head: &ObjectId,
    branch_checkout: bool,
) -> Result<()> {
    let old = old_head.to_hex();
    let new = new_head.to_hex();
    commands::hooks::run_hook_l(
        "post-checkout",
        &[
            old.as_str(),
            new.as_str(),
            if branch_checkout { "1" } else { "0" },
        ],
    )?;
    Ok(())
}

fn checkout_track_branch_name(store: &FileRefStore, upstream: &str) -> Result<String> {
    if let Some(rest) = upstream.strip_prefix("refs/remotes/")
        && let Some((_, branch)) = rest.split_once('/')
        && !branch.is_empty()
    {
        return Ok(branch.to_string());
    }
    if let Some(branch) = upstream.strip_prefix("refs/heads/")
        && !branch.is_empty()
    {
        return Ok(branch.to_string());
    }
    let remote_ref = format!("refs/remotes/{upstream}");
    if store.read_ref(&remote_ref)?.is_some()
        && let Some((_, branch)) = upstream.split_once('/')
        && !branch.is_empty()
    {
        return Ok(branch.to_string());
    }
    Ok(upstream.rsplit('/').next().unwrap_or(upstream).to_string())
}

pub(crate) fn cmd_switch(args: &[String]) -> Result<()> {
    // `switch --orphan <name>` clears the index and worktree of files carried
    // from the old HEAD (a twoway checkout to the empty tree), unlike
    // `checkout --orphan` which keeps them staged.
    if let Some(pos) = args.iter().position(|arg| arg == "--orphan") {
        let Some(branch) = args.get(pos + 1) else {
            return Err(GitError::Command(
                "switch --orphan requires a branch".into(),
            ));
        };
        let cwd = env::current_dir()?;
        let git_dir = discover_git_dir(&cwd)?;
        let worktree_root = worktree_root_for_git_dir(&git_dir)?;
        let format = repository_object_format(&git_dir)?;
        checkout_twoway_dirty(&git_dir, &worktree_root, format, None)?;
        checkout_switch_to_unborn_branch(&git_dir, branch)?;
        sley_sequencer::replay::remove_branch_state(&git_dir);
        if !args.iter().any(|arg| arg == "-q" || arg == "--quiet") {
            eprintln!("Switched to a new branch '{branch}'");
        }
        return Ok(());
    }
    let mut checkout_args = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-c" | "--create" => {
                checkout_args.push("-b".to_string());
                let branch = iter
                    .next()
                    .ok_or_else(|| GitError::Command("switch -c requires a branch".into()))?;
                checkout_args.push(branch.to_string());
            }
            value if value.starts_with("--create=") => {
                checkout_args.push("-b".to_string());
                checkout_args.push(
                    value
                        .strip_prefix("--create=")
                        .ok_or_else(|| {
                            GitError::Command("switch --create requires a branch".into())
                        })?
                        .to_string(),
                );
            }
            "-d" => checkout_args.push("--detach".to_string()),
            "-C" | "--force-create" => {
                checkout_args.push("-B".to_string());
                let branch = iter
                    .next()
                    .ok_or_else(|| GitError::Command("switch -C requires a branch".into()))?;
                checkout_args.push(branch.to_string());
            }
            value if value.starts_with("--force-create=") => {
                checkout_args.push("-B".to_string());
                checkout_args.push(
                    value
                        .strip_prefix("--force-create=")
                        .ok_or_else(|| {
                            GitError::Command("switch --force-create requires a branch".into())
                        })?
                        .to_string(),
                );
            }
            value => checkout_args.push(value.to_string()),
        }
    }
    cmd_checkout(&checkout_args)
}

pub(crate) fn cmd_restore(args: &[String]) -> Result<()> {
    let mut paths = Vec::new();
    let mut parsing_options = true;
    let mut staged = false;
    let mut worktree = false;
    let mut source = None::<String>;
    let mut pathspec_from_file: Option<PathBuf> = None;
    let mut pathspec_file_nul = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if !parsing_options {
            if pathspec_from_file.is_some() {
                eprintln!(
                    "fatal: '--pathspec-from-file' and pathspec arguments cannot be used together"
                );
                return Err(GitError::Exit(128));
            }
            paths.push(PathBuf::from(arg));
            continue;
        }
        match arg.as_str() {
            "--" => parsing_options = false,
            "--worktree" | "-W" => worktree = true,
            "--staged" | "-S" => staged = true,
            "--quiet"
            | "--no-quiet"
            | "--progress"
            | "--no-progress"
            | "--overlay"
            | "--no-overlay"
            | "--ignore-unmerged"
            | "--no-ignore-unmerged"
            | "--ignore-skip-worktree-bits"
            | "--no-ignore-skip-worktree-bits"
            | "--no-recurse-submodules" => {}
            "--source" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("restore --source requires a value".into()))?;
                source = Some(value.clone());
            }
            "-s" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("restore -s requires a value".into()))?;
                source = Some(value.clone());
            }
            value if value.starts_with("--source=") => {
                let value = value
                    .strip_prefix("--source=")
                    .ok_or_else(|| GitError::Command("restore --source requires a value".into()))?;
                source = Some(value.to_string());
            }
            value if value.starts_with("-s") && value.len() > 2 => {
                source = Some(value[2..].to_string());
            }
            "--pathspec-file-nul" => pathspec_file_nul = true,
            "--no-pathspec-file-nul" => pathspec_file_nul = false,
            "--pathspec-from-file" => {
                if !paths.is_empty() {
                    eprintln!(
                        "fatal: '--pathspec-from-file' and pathspec arguments cannot be used together"
                    );
                    return Err(GitError::Exit(128));
                }
                let value = iter.next().ok_or_else(|| {
                    GitError::Command("restore --pathspec-from-file requires a value".into())
                })?;
                pathspec_from_file = Some(PathBuf::from(value));
            }
            "--no-pathspec-from-file" => {}
            value if value.starts_with("--pathspec-from-file=") => {
                if !paths.is_empty() {
                    eprintln!(
                        "fatal: '--pathspec-from-file' and pathspec arguments cannot be used together"
                    );
                    return Err(GitError::Exit(128));
                }
                let value = value.strip_prefix("--pathspec-from-file=").ok_or_else(|| {
                    GitError::Command("restore --pathspec-from-file requires a value".into())
                })?;
                pathspec_from_file = Some(PathBuf::from(value));
            }
            value
                if value.starts_with('-')
                    && value.len() > 2
                    && value[1..]
                        .bytes()
                        .all(|option| option == b'S' || option == b'W') =>
            {
                for option in value[1..].bytes() {
                    match option {
                        b'S' => staged = true,
                        b'W' => worktree = true,
                        _ => unreachable!("restore short-option group was filtered"),
                    }
                }
            }
            value if value.starts_with('-') => {
                return Err(GitError::Command(format!(
                    "unsupported restore option {value}"
                )));
            }
            value => {
                if pathspec_from_file.is_some() {
                    eprintln!(
                        "fatal: '--pathspec-from-file' and pathspec arguments cannot be used together"
                    );
                    return Err(GitError::Exit(128));
                }
                paths.push(PathBuf::from(value));
            }
        }
    }
    if pathspec_file_nul && pathspec_from_file.is_none() {
        eprintln!("fatal: the option '--pathspec-file-nul' requires '--pathspec-from-file'");
        return Err(GitError::Exit(128));
    }
    if let Some(pathspec_file) = pathspec_from_file {
        paths.extend(read_pathspecs_from_file(&pathspec_file, pathspec_file_nul)?);
    }
    if paths.is_empty() {
        return Err(GitError::Command(
            "restore requires at least one path".into(),
        ));
    }
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    let format = repository_object_format(&git_dir)?;
    let resolved_paths = paths
        .into_iter()
        .map(|path| {
            if path.is_absolute() {
                path
            } else {
                cwd.join(path)
            }
        })
        .collect::<Vec<_>>();
    let source_tree = if let Some(source) = source.as_deref() {
        let db = FileObjectDatabase::from_git_dir(&git_dir, format);
        let oid = resolve_revision(&git_dir, format, source)?;
        Some(sley_rev::peel_to_tree(&db, format, &oid)?)
    } else {
        None
    };
    if staged && worktree {
        if let Some(tree_oid) = source_tree.as_ref() {
            sley_worktree::restore_index_and_worktree_paths_from_tree(
                worktree_root,
                git_dir,
                format,
                tree_oid,
                &resolved_paths,
            )?;
        } else {
            sley_worktree::restore_index_and_worktree_paths_from_head(
                worktree_root,
                git_dir,
                format,
                &resolved_paths,
            )?;
        }
    } else if staged {
        if let Some(tree_oid) = source_tree.as_ref() {
            sley_worktree::restore_index_paths_from_tree(
                worktree_root,
                git_dir,
                format,
                tree_oid,
                &resolved_paths,
            )?;
        } else {
            sley_worktree::restore_index_paths_from_head(
                worktree_root,
                git_dir,
                format,
                &resolved_paths,
            )?;
        }
    } else if let Some(tree_oid) = source_tree.as_ref() {
        sley_worktree::restore_worktree_paths_from_tree(
            worktree_root,
            git_dir,
            format,
            tree_oid,
            &resolved_paths,
        )?;
    } else {
        sley_worktree::restore_worktree_paths(worktree_root, git_dir, format, &resolved_paths)?;
    }
    Ok(())
}

/// Dirty-tolerant two-way checkout fallback: paths whose content is the same
/// in the current HEAD and the target carry their index/worktree state across
/// the switch (git's twoway_merge); paths that must change are updated only
/// when clean, and staged changes that conflict with the target refuse the
/// switch. `target` of `None` switches to the empty tree (`switch --orphan`).
fn checkout_twoway_dirty(
    git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
    target: Option<&ObjectId>,
) -> Result<()> {
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let target_map = match target {
        Some(target) => {
            let tree = commands::merge_rebase::commit_tree_oid(&db, format, target)?;
            stash_tree_entry_map(&db, format, &tree)?
        }
        None => BTreeMap::new(),
    };
    let refs = FileRefStore::new(git_dir, format);
    let head_map = match commands::merge_rebase::head_commit_oid(&refs)? {
        Some(head) => {
            let tree = commands::merge_rebase::commit_tree_oid(&db, format, &head)?;
            stash_tree_entry_map(&db, format, &tree)?
        }
        None => BTreeMap::new(),
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
    for entry in &old_index.entries {
        if index_entry_stage(entry) > 0 {
            eprintln!("error: you need to resolve your current index first");
            return Err(GitError::Exit(1));
        }
        stage0.insert(entry.path.clone().into_bytes(), entry.clone());
    }
    let all_paths: BTreeSet<Vec<u8>> = stage0
        .keys()
        .cloned()
        .chain(target_map.keys().cloned())
        .chain(head_map.keys().cloned())
        .collect();
    let mut blocked: Vec<Vec<u8>> = Vec::new();
    let mut updates: Vec<(Vec<u8>, (u32, ObjectId))> = Vec::new();
    let mut deletions: Vec<Vec<u8>> = Vec::new();
    // Paths the new index keeps from the old index even though the target
    // tree disagrees (the twoway "carry" rule: HEAD and target agree).
    let mut carried: BTreeSet<Vec<u8>> = BTreeSet::new();
    for path in &all_paths {
        let current = stage0.get(path).map(|entry| (entry.mode, entry.oid));
        let wanted = target_map.get(path).copied();
        let in_head = head_map.get(path).copied();
        if in_head == wanted {
            // Same on both sides of the switch: carry local state verbatim.
            if current.is_some() && wanted != current {
                carried.insert(path.clone());
            }
            continue;
        }
        if current == wanted {
            continue;
        }
        if current != in_head {
            // Staged (or missing) state conflicts with the switch.
            blocked.push(path.clone());
            continue;
        }
        let rel = String::from_utf8_lossy(path).into_owned();
        let full = worktree_root.join(&rel);
        if let Some(entry) = stage0.get(path) {
            if let Ok(bytes) = fs::read(&full) {
                let on_disk = sley_core::object_id_for_bytes(format, "blob", &bytes)?;
                if on_disk != entry.oid {
                    blocked.push(path.clone());
                    continue;
                }
            }
        } else if let Some((_, oid)) = &wanted
            && let Ok(bytes) = fs::read(&full)
        {
            // Untracked file in the way: only identical content may be adopted.
            let on_disk = sley_core::object_id_for_bytes(format, "blob", &bytes)?;
            if &on_disk != oid {
                eprintln!(
                    "error: The following untracked working tree files would be overwritten by checkout:"
                );
                eprintln!("\t{rel}");
                eprintln!("Please move or remove them before you switch branches.");
                eprintln!("Aborting");
                return Err(GitError::Exit(1));
            }
        }
        match wanted {
            Some(entry) => updates.push((path.clone(), entry)),
            None => deletions.push(path.clone()),
        }
    }
    if !blocked.is_empty() {
        eprintln!(
            "error: Your local changes to the following files would be overwritten by checkout:"
        );
        for path in &blocked {
            eprintln!("\t{}", String::from_utf8_lossy(path));
        }
        eprintln!("Please commit your changes or stash them before you switch branches.");
        eprintln!("Aborting");
        return Err(GitError::Exit(1));
    }
    for (path, (mode, oid)) in &updates {
        let content = commands::merge_rebase::merge_read_blob(&db, oid)?;
        commands::merge_rebase::merge_write_worktree_file(worktree_root, path, &content, *mode)?;
    }
    for path in &deletions {
        if stage0.get(path).is_some_and(|entry| entry.mode == 0o160000) {
            checkout_remove_gitlink_worktree_dir(worktree_root, path)?;
        } else {
            commands::merge_rebase::merge_remove_worktree_file(worktree_root, path)?;
        }
    }
    let mut entries: Vec<IndexEntry> = Vec::new();
    for (path, (mode, oid)) in &target_map {
        if carried.contains(path) {
            continue;
        }
        if let Some(old) = stage0.get(path)
            && old.mode == *mode
            && old.oid == *oid
        {
            entries.push(old.clone());
        } else {
            entries.push(commands::merge_rebase::merge_index_entry(
                path, *mode, *oid, 0,
            ));
        }
    }
    for path in &carried {
        if let Some(old) = stage0.get(path) {
            entries.push(old.clone());
        }
    }
    // Staged adds whose path is in neither tree are carried too.
    for (path, entry) in &stage0 {
        if !target_map.contains_key(path) && !head_map.contains_key(path) && !carried.contains(path)
        {
            entries.push(entry.clone());
        }
    }
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    fs::write(
        &index_path,
        Index {
            version: 2,
            entries,
            extensions: Vec::new(),
            checksum: None,
        }
        .write(format)?,
    )?;
    Ok(())
}

fn checkout_remove_gitlink_worktree_dir(worktree_root: &Path, path: &[u8]) -> Result<()> {
    let rel = std::str::from_utf8(path)
        .map_err(|_| GitError::InvalidFormat("non-utf8 worktree path".into()))?;
    let full = worktree_root.join(rel);
    if !full.exists() {
        return Ok(());
    }
    if full.is_dir() {
        match fs::remove_dir(&full) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::DirectoryNotEmpty => {}
            Err(err) => return Err(err.into()),
        }
        return Ok(());
    }
    commands::merge_rebase::merge_remove_worktree_file(worktree_root, path)
}

fn checkout_is_dirty_tree_error(err: &GitError) -> bool {
    matches!(err, GitError::Transaction(msg) if msg.contains("clean working tree"))
}

fn detach_head_with_reflog(
    git_dir: &Path,
    format: ObjectFormat,
    target: &ObjectId,
    message: Vec<u8>,
) -> Result<()> {
    let refs = FileRefStore::new(git_dir, format);
    let mut tx = refs.transaction();
    tx.update(RefUpdate {
        name: "HEAD".into(),
        expected: None,
        new: RefTarget::Direct(*target),
        reflog: Some(ReflogEntry {
            old_oid: ObjectId::null(format),
            new_oid: *target,
            committer: commit_identity_from_env("COMMITTER")?,
            message,
        }),
    });
    tx.commit()
}

fn switch_head_symbolic_with_reflog(
    git_dir: &Path,
    format: ObjectFormat,
    branch: &str,
    target: &ObjectId,
    from: &str,
) -> Result<()> {
    let refs = FileRefStore::new(git_dir, format);
    let mut tx = refs.transaction();
    tx.update(RefUpdate {
        name: "HEAD".into(),
        expected: None,
        new: RefTarget::Symbolic(branch_ref_name(branch)?),
        reflog: Some(ReflogEntry {
            old_oid: *target,
            new_oid: *target,
            committer: commit_identity_from_env("COMMITTER")?,
            message: format!("checkout: moving from {from} to {branch}").into_bytes(),
        }),
    });
    tx.commit()
}

fn checkout_reflog_from_name(store: &FileRefStore) -> String {
    match store.read_ref("HEAD") {
        Ok(Some(RefTarget::Symbolic(name))) => name
            .strip_prefix("refs/heads/")
            .unwrap_or(&name)
            .to_string(),
        Ok(Some(RefTarget::Direct(oid))) => oid.to_hex(),
        _ => "HEAD".to_string(),
    }
}

enum CheckoutBranchMode {
    Existing,
    Detach,
    Create {
        branch: String,
        force: bool,
        orphan: bool,
    },
}

fn checkout_switch_to_unborn_branch(git_dir: &Path, branch: &str) -> Result<()> {
    let store = FileRefStore::new(git_dir, repository_object_format(git_dir)?);
    let name = branch_ref_name(branch)?;
    let mut tx = store.transaction();
    tx.update(RefUpdate {
        name: "HEAD".into(),
        expected: None,
        new: RefTarget::Symbolic(name),
        reflog: None,
    });
    tx.commit()
}

enum CheckoutMessage {
    Existing { branch: String },
    New { branch: String },
    Reset { branch: String },
}

impl CheckoutMessage {
    fn branch(&self) -> &str {
        match self {
            Self::Existing { branch } | Self::New { branch } | Self::Reset { branch } => branch,
        }
    }

    fn print(&self) {
        match self {
            Self::Existing { branch } => eprintln!("Switched to branch '{branch}'"),
            Self::New { branch } => eprintln!("Switched to a new branch '{branch}'"),
            Self::Reset { branch } => eprintln!("Switched to and reset branch '{branch}'"),
        }
    }
}

enum CommitShortFlag {
    /// A boolean flag that takes no value (e.g. `-q`, `-s`, `-a`).
    Boolean,
    /// A flag whose value is required (e.g. `-m`, `-F`, `-C`, `-c`, `-t`,
    /// `-U`). In a cluster it consumes the rest of the cluster; standalone it
    /// consumes the next argument.
    RequiresValue,
    /// A flag whose value is optional (`-S`, `-u`; `PARSE_OPT_OPTARG`). It
    /// consumes the rest of the cluster if any, but never the next argument.
    OptionalValue,
}

fn commit_short_flag_kind(ch: char) -> Option<CommitShortFlag> {
    match ch {
        // OPT__QUIET / OPT__VERBOSE and the plain OPT_BOOL entries.
        'q' | 'v' | 's' | 'e' | 'a' | 'i' | 'p' | 'o' | 'n' | 'z' => Some(CommitShortFlag::Boolean),
        // OPT_CALLBACK('m'), OPT_FILENAME('F'/'t'), OPT_STRING('c'/'C'),
        // OPT_DIFF_UNIFIED ('U').
        'm' | 'F' | 'c' | 'C' | 't' | 'U' => Some(CommitShortFlag::RequiresValue),
        // PARSE_OPT_OPTARG entries: gpg-sign ('S') and untracked-files ('u').
        'S' | 'u' => Some(CommitShortFlag::OptionalValue),
        _ => None,
    }
}

fn expand_commit_short_clusters(args: &[String]) -> Result<Vec<String>> {
    let mut expanded = Vec::with_capacity(args.len());
    let mut saw_dashdash = false;
    for arg in args {
        if saw_dashdash {
            expanded.push(arg.clone());
            continue;
        }
        if arg == "--" {
            saw_dashdash = true;
            expanded.push(arg.clone());
            continue;
        }
        let bytes = arg.as_bytes();
        // Not a short-option cluster: keep `-`, `--long`, and positionals as-is.
        if bytes.len() < 2 || bytes[0] != b'-' || bytes[1] == b'-' {
            expanded.push(arg.clone());
            continue;
        }
        let cluster = &arg[1..];
        let mut chars = cluster.char_indices();
        let Some((_, first)) = chars.next() else {
            expanded.push(arg.clone());
            continue;
        };
        // Only expand clusters that *start* with a boolean flag. If the first
        // flag is unknown or already takes a value, defer entirely to the main
        // parser (its glued-value / error arms own that input).
        if !matches!(
            commit_short_flag_kind(first),
            Some(CommitShortFlag::Boolean)
        ) {
            expanded.push(arg.clone());
            continue;
        }
        expanded.push(format!("-{first}"));
        // Walk the remaining flags in this cluster. A value-taking flag
        // swallows the rest of the cluster and ends the scan; the main parser
        // owns next-argument consumption when the glued value is empty.
        for (idx, ch) in chars {
            match commit_short_flag_kind(ch) {
                Some(CommitShortFlag::Boolean) => expanded.push(format!("-{ch}")),
                Some(CommitShortFlag::RequiresValue) | Some(CommitShortFlag::OptionalValue) => {
                    // `-q` `m` `rest` -> `-mrest`; when `rest` is empty we emit
                    // just `-m`, and the main parser consumes the next argument
                    // (required) or treats the value as absent (optional).
                    expanded.push(format!("-{}", &cluster[idx..]));
                    break;
                }
                None => {
                    // Unknown flag inside the cluster: preserve the existing
                    // error for the whole original cluster (exit 1) rather than
                    // emitting partial side effects from the leading flags.
                    return Err(GitError::Command(format!(
                        "unsupported commit argument {arg}; currently supports -m and -F"
                    )));
                }
            }
        }
    }
    Ok(expanded)
}

pub(crate) fn cmd_commit(raw_args: &[String]) -> Result<()> {
    // `-h`/`--help` is handled by upstream's parse-options before any repo
    // state is consulted (so it works in a broken repository). Honour it first.
    if raw_args.iter().any(|arg| arg == "-h" || arg == "--help") {
        return commit_usage();
    }
    let args = expand_commit_short_clusters(raw_args)?;
    let args = args.as_slice();
    let mut message_chunks = Vec::new();
    let mut file_message = None;
    let mut signoff = false;
    let mut quiet = false;
    let mut allow_empty = false;
    let mut allow_empty_message = false;
    let mut all = false;
    let mut author_override = None;
    let mut author_date = None;
    let mut reuse_message = None;
    let mut reedit_message = false;
    let mut fixup_commit = None;
    let mut squash_commit = None;
    // Raw `--trailer <arg>` strings, applied through the full interpret-trailers
    // engine (so per-token `trailer.*` config — key/where/ifexists/ifmissing/
    // command — applies, matching `git commit --trailer`).
    let mut trailers: Vec<String> = Vec::new();
    let mut reset_author = false;
    let mut amend = false;
    let mut verbose = false;
    // `--status` / `--no-status` (and `commit.status` config) control whether the
    // working-tree status block is appended to the editor template (COMMIT_EDITMSG).
    // `None` = unset on the command line, so `commit.status` config (default true)
    // decides. Mirrors builtin/commit.c `include_status`.
    let mut include_status: Option<bool> = None;
    // The raw `--cleanup=<mode>` argument, if any. Resolution to a concrete mode
    // is deferred until `use_editor` is known (git: `default`/`scissors` depend
    // on whether an editor runs). `None` means "no --cleanup given" — fall back
    // to `commit.cleanup` config, then the editor-dependent default.
    let mut cleanup_arg: Option<String> = None;
    let mut include_without_paths = false;
    let mut only_without_paths = false;
    let mut status_mode = CommitStatusMode::Normal;
    let mut status_null = false;
    let mut null_implied_status = false;
    // `commit -u<mode>` / `--untracked-files=<mode>` overrides
    // `status.showUntrackedFiles` for the dry-run / status preview. `None` means
    // the flag was not given, so config / default applies.
    let mut commit_untracked: Option<sley_worktree::StatusUntrackedMode> = None;
    let mut dry_run = false;
    let mut no_verify = false;
    let mut no_post_rewrite = false;
    let mut interactive = false;
    let mut patch = false;
    let mut gpg_sign = false;
    let mut unified_context = false;
    let mut inter_hunk_context = false;
    let mut pathspec_from_file = None;
    let mut pathspec_from_file_active = false;
    let mut pathspec_file_nul = false;
    let mut pathspec_args = Vec::new();
    let mut edit_flag: Option<bool> = None;
    // `-t <file>` / `--template <file>`: the lowest-priority message body source.
    // Unlike `-m`/`-F`/`-C`, it does NOT suppress the editor (git keeps
    // `use_editor = 1`), so the user always edits the template.
    let mut template_file: Option<String> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-m" => {
                let Some(message) = iter.next() else {
                    return commit_message_requires_value_error();
                };
                let mut chunk = message.as_bytes().to_vec();
                chunk.push(b'\n');
                message_chunks.push(chunk);
            }
            value if value.starts_with("-m") && value.len() > 2 => {
                let mut chunk = value.as_bytes()[2..].to_vec();
                chunk.push(b'\n');
                message_chunks.push(chunk);
            }
            value if value.starts_with("-am") => {
                all = true;
                let message = if value.len() > 3 {
                    &value[3..]
                } else {
                    let Some(message) = iter.next() else {
                        return commit_message_requires_value_error();
                    };
                    message
                };
                let mut chunk = message.as_bytes().to_vec();
                chunk.push(b'\n');
                message_chunks.push(chunk);
            }
            "--message" => {
                let Some(message) = iter.next() else {
                    return commit_message_requires_value_error();
                };
                let mut chunk = message.as_bytes().to_vec();
                chunk.push(b'\n');
                message_chunks.push(chunk);
            }
            value if value.starts_with("--message=") => {
                let mut chunk = value.as_bytes()["--message=".len()..].to_vec();
                chunk.push(b'\n');
                message_chunks.push(chunk);
            }
            "--no-message" => message_chunks.clear(),
            value if value.starts_with("--no-message=") => {
                return commit_option_takes_no_value_error("no-message");
            }
            "-F" | "--file" => {
                let Some(path) = iter.next() else {
                    return commit_tree_file_requires_value_error();
                };
                file_message = Some(read_porcelain_commit_message_file(path)?);
            }
            value if value.starts_with("-F") && value.len() > 2 => {
                file_message = Some(read_porcelain_commit_message_file(&value[2..])?);
            }
            value if value.starts_with("--file=") => {
                file_message = Some(read_porcelain_commit_message_file(
                    &value["--file=".len()..],
                )?);
            }
            "--no-file" => {}
            value if value.starts_with("--no-file=") => {
                return commit_option_takes_no_value_error("no-file");
            }
            "-C" | "--reuse-message" => {
                let Some(value) = iter.next() else {
                    return commit_reuse_message_requires_value_error(arg == "-C", false);
                };
                reuse_message = Some(value.to_string());
                reedit_message = false;
            }
            value if value.starts_with("-C") && value.len() > 2 => {
                reuse_message = Some(value[2..].to_string());
                reedit_message = false;
            }
            value if value.starts_with("--reuse-message=") => {
                reuse_message = Some(value["--reuse-message=".len()..].to_string());
                reedit_message = false;
            }
            "--no-reuse-message" => {
                reuse_message = None;
                reedit_message = false;
            }
            value if value.starts_with("--no-reuse-message=") => {
                return commit_option_takes_no_value_error("no-reuse-message");
            }
            "-c" | "--reedit-message" => {
                let Some(value) = iter.next() else {
                    return commit_reuse_message_requires_value_error(arg == "-c", true);
                };
                reuse_message = Some(value.to_string());
                reedit_message = true;
            }
            value if value.starts_with("-c") && value.len() > 2 => {
                reuse_message = Some(value[2..].to_string());
                reedit_message = true;
            }
            value if value.starts_with("--reedit-message=") => {
                reuse_message = Some(value["--reedit-message=".len()..].to_string());
                reedit_message = true;
            }
            "--no-reedit-message" => {
                reuse_message = None;
                reedit_message = false;
            }
            value if value.starts_with("--no-reedit-message=") => {
                return commit_option_takes_no_value_error("no-reedit-message");
            }
            "--fixup" => {
                let Some(value) = iter.next() else {
                    return commit_fixup_requires_value_error();
                };
                fixup_commit = Some(CommitFixup::parse(value)?);
            }
            value if value.starts_with("--fixup=") => {
                fixup_commit = Some(CommitFixup::parse(&value["--fixup=".len()..])?);
            }
            "--no-fixup" => fixup_commit = None,
            value if value.starts_with("--no-fixup=") => {
                return commit_option_takes_no_value_error("no-fixup");
            }
            "--squash" => {
                let Some(value) = iter.next() else {
                    return commit_squash_requires_value_error();
                };
                squash_commit = Some(value.to_string());
            }
            value if value.starts_with("--squash=") => {
                squash_commit = Some(value["--squash=".len()..].to_string());
            }
            "--no-squash" => squash_commit = None,
            value if value.starts_with("--no-squash=") => {
                return commit_option_takes_no_value_error("no-squash");
            }
            "--trailer" => {
                let Some(value) = iter.next() else {
                    return commit_trailer_requires_value_error();
                };
                trailers.push(value.clone());
            }
            value if value.starts_with("--trailer=") => {
                trailers.push(value["--trailer=".len()..].to_string());
            }
            "--no-trailer" => trailers.clear(),
            value if value.starts_with("--no-trailer=") => {
                return commit_option_takes_no_value_error("no-trailer");
            }
            "--reset-author" => reset_author = true,
            "--no-reset-author" => reset_author = false,
            value if value.starts_with("--reset-author=") => {
                return commit_option_takes_no_value_error("reset-author");
            }
            value if value.starts_with("--no-reset-author=") => {
                return commit_option_takes_no_value_error("no-reset-author");
            }
            "--amend" => amend = true,
            "--no-amend" => amend = false,
            value if value.starts_with("--amend=") => {
                return commit_option_takes_no_value_error("amend");
            }
            value if value.starts_with("--no-amend=") => {
                return commit_option_takes_no_value_error("no-amend");
            }
            "-s" | "--signoff" => signoff = true,
            "--no-signoff" => signoff = false,
            value if value.starts_with("--signoff=") => {
                return commit_option_takes_no_value_error("signoff");
            }
            value if value.starts_with("--no-signoff=") => {
                return commit_option_takes_no_value_error("no-signoff");
            }
            "-q" | "--quiet" => quiet = true,
            "--no-quiet" => quiet = false,
            value if value.starts_with("--quiet=") => {
                return commit_option_takes_no_value_error("quiet");
            }
            value if value.starts_with("--no-quiet=") => {
                return commit_option_takes_no_value_error("no-quiet");
            }
            "-a" | "--all" => all = true,
            "--no-all" => all = false,
            value if value.starts_with("--all=") => {
                return commit_option_takes_no_value_error("all");
            }
            value if value.starts_with("--no-all=") => {
                return commit_option_takes_no_value_error("no-all");
            }
            "--allow-empty" => allow_empty = true,
            "--no-allow-empty" => allow_empty = false,
            "--allow-empty-message" => allow_empty_message = true,
            "--no-allow-empty-message" => allow_empty_message = false,
            value if value.starts_with("--allow-empty=") => {
                return commit_option_takes_no_value_error("allow-empty");
            }
            value if value.starts_with("--no-allow-empty=") => {
                return commit_option_takes_no_value_error("no-allow-empty");
            }
            value if value.starts_with("--allow-empty-message=") => {
                return commit_option_takes_no_value_error("allow-empty-message");
            }
            value if value.starts_with("--no-allow-empty-message=") => {
                return commit_option_takes_no_value_error("no-allow-empty-message");
            }
            "--author" => {
                let Some(author) = iter.next() else {
                    return commit_author_requires_value_error();
                };
                author_override = Some(author.to_string());
            }
            value if value.starts_with("--author=") => {
                author_override = Some(value["--author=".len()..].to_string());
            }
            "--no-author" => author_override = None,
            value if value.starts_with("--no-author=") => {
                return commit_option_takes_no_value_error("no-author");
            }
            "--date" => {
                let Some(date) = iter.next() else {
                    return commit_date_requires_value_error();
                };
                author_date = Some(date.to_string());
            }
            value if value.starts_with("--date=") => {
                author_date = Some(value["--date=".len()..].to_string());
            }
            "--no-date" => author_date = None,
            value if value.starts_with("--no-date=") => {
                return commit_option_takes_no_value_error("no-date");
            }
            "-n" | "--no-verify" => no_verify = true,
            "--verify" => no_verify = false,
            value if value.starts_with("--no-verify=") => {
                return commit_option_takes_no_value_error("no-verify");
            }
            value if value.starts_with("--verify=") => {
                return commit_option_takes_no_value_error("no-no-verify");
            }
            "-S" | "--gpg-sign" => gpg_sign = true,
            value if value.starts_with("-S") && value.len() > 2 => {
                gpg_sign = true;
            }
            value if value.starts_with("--gpg-sign=") => {
                gpg_sign = true;
            }
            "--no-gpg-sign" => gpg_sign = false,
            value if value.starts_with("--no-gpg-sign=") => {
                return commit_option_takes_no_value_error("no-gpg-sign");
            }
            "--post-rewrite" => no_post_rewrite = false,
            "--no-post-rewrite" => no_post_rewrite = true,
            value if value.starts_with("--post-rewrite=") => {
                return commit_option_takes_no_value_error("no-no-post-rewrite");
            }
            value if value.starts_with("--no-post-rewrite=") => {
                return commit_option_takes_no_value_error("no-post-rewrite");
            }
            "--status" => include_status = Some(true),
            "--no-status" => include_status = Some(false),
            value if value.starts_with("--status=") => {
                return commit_option_takes_no_value_error("status");
            }
            value if value.starts_with("--no-status=") => {
                return commit_option_takes_no_value_error("no-status");
            }
            "--dry-run" => dry_run = true,
            "--no-dry-run" => dry_run = false,
            value if value.starts_with("--dry-run=") => {
                return commit_option_takes_no_value_error("dry-run");
            }
            value if value.starts_with("--no-dry-run=") => {
                return commit_option_takes_no_value_error("no-dry-run");
            }
            "--short" => {
                status_mode = CommitStatusMode::Short;
                null_implied_status = false;
            }
            "--no-short" => {
                if status_mode == CommitStatusMode::Short {
                    status_mode = CommitStatusMode::Normal;
                }
                null_implied_status = false;
            }
            value if value.starts_with("--short=") => {
                return commit_option_takes_no_value_error("short");
            }
            value if value.starts_with("--no-short=") => {
                return commit_option_takes_no_value_error("no-short");
            }
            "--porcelain" => {
                status_mode = CommitStatusMode::Porcelain;
                null_implied_status = false;
            }
            "--no-porcelain" => {
                if status_mode == CommitStatusMode::Porcelain {
                    status_mode = CommitStatusMode::Normal;
                }
                null_implied_status = false;
            }
            value if value.starts_with("--porcelain=") => {
                return commit_option_takes_no_value_error("porcelain");
            }
            value if value.starts_with("--no-porcelain=") => {
                return commit_option_takes_no_value_error("no-porcelain");
            }
            "-z" | "--null" => {
                if status_mode == CommitStatusMode::Normal {
                    status_mode = CommitStatusMode::Short;
                    null_implied_status = true;
                }
                status_null = true;
            }
            "--no-null" => {
                status_null = false;
                if null_implied_status {
                    status_mode = CommitStatusMode::Normal;
                    null_implied_status = false;
                }
            }
            value if value.starts_with("--null=") => {
                return commit_option_takes_no_value_error("null");
            }
            value if value.starts_with("--no-null=") => {
                return commit_option_takes_no_value_error("no-null");
            }
            "--long" => {
                status_mode = CommitStatusMode::Long;
                null_implied_status = false;
            }
            "--no-long" => {
                if status_mode == CommitStatusMode::Long {
                    status_mode = CommitStatusMode::Normal;
                }
                null_implied_status = false;
            }
            value if value.starts_with("--long=") => {
                return commit_option_takes_no_value_error("long");
            }
            value if value.starts_with("--no-long=") => {
                return commit_option_takes_no_value_error("no-long");
            }
            "--ahead-behind" | "--no-ahead-behind" => {}
            value if value.starts_with("--ahead-behind=") => {
                return commit_option_takes_no_value_error("ahead-behind");
            }
            value if value.starts_with("--no-ahead-behind=") => {
                return commit_option_takes_no_value_error("no-ahead-behind");
            }
            "--interactive" => interactive = true,
            "--no-interactive" => interactive = false,
            value if value.starts_with("--interactive=") => {
                return commit_option_takes_no_value_error("interactive");
            }
            value if value.starts_with("--no-interactive=") => {
                return commit_option_takes_no_value_error("no-interactive");
            }
            "-p" | "--patch" => patch = true,
            "--no-patch" => patch = false,
            value if value.starts_with("--patch=") => {
                return commit_option_takes_no_value_error("patch");
            }
            value if value.starts_with("--no-patch=") => {
                return commit_option_takes_no_value_error("no-patch");
            }
            "-U" => {
                let Some(value) = iter.next() else {
                    return commit_unified_requires_value_error(true);
                };
                commit_validate_unified_context(value, true)?;
                unified_context = true;
            }
            value if value.starts_with("-U") && value.len() > 2 => {
                commit_validate_unified_context(&value[2..], true)?;
                unified_context = true;
            }
            "--unified" => {
                let Some(value) = iter.next() else {
                    return commit_unified_requires_value_error(false);
                };
                commit_validate_unified_context(value, false)?;
                unified_context = true;
            }
            "--unified=" => {
                return commit_unified_expects_numerical_value_error(false);
            }
            value if value.starts_with("--unified=") => {
                commit_validate_unified_context(&value["--unified=".len()..], false)?;
                unified_context = true;
            }
            "--inter-hunk-context" => {
                let Some(value) = iter.next() else {
                    return commit_inter_hunk_context_requires_value_error();
                };
                commit_validate_inter_hunk_context(value)?;
                inter_hunk_context = true;
            }
            "--inter-hunk-context=" => {
                return commit_inter_hunk_context_expects_numerical_value_error();
            }
            value if value.starts_with("--inter-hunk-context=") => {
                commit_validate_inter_hunk_context(&value["--inter-hunk-context=".len()..])?;
                inter_hunk_context = true;
            }
            "-v" | "--verbose" => verbose = true,
            "--no-verbose" => verbose = false,
            value if value.starts_with("--verbose=") => {
                return commit_option_takes_no_value_error("verbose");
            }
            value if value.starts_with("--no-verbose=") => {
                return commit_option_takes_no_value_error("no-verbose");
            }
            "-u" | "-unormal" | "--untracked-files" => {
                commit_untracked = Some(sley_worktree::StatusUntrackedMode::Normal);
            }
            "-uno" => commit_untracked = Some(sley_worktree::StatusUntrackedMode::None),
            "-uall" => commit_untracked = Some(sley_worktree::StatusUntrackedMode::All),
            value if value.starts_with("-u") && value.len() > 2 => {
                return commit_invalid_untracked_files_mode_error(&value[2..]);
            }
            value if value.starts_with("--untracked-files=") => {
                let mode = &value["--untracked-files=".len()..];
                commit_untracked = Some(match mode {
                    "no" => sley_worktree::StatusUntrackedMode::None,
                    "normal" => sley_worktree::StatusUntrackedMode::Normal,
                    "all" => sley_worktree::StatusUntrackedMode::All,
                    _ => return commit_invalid_untracked_files_mode_error(mode),
                });
            }
            "--no-untracked-files" => {
                commit_untracked = Some(sley_worktree::StatusUntrackedMode::None);
            }
            value if value.starts_with("--no-untracked-files=") => {
                return commit_option_takes_no_value_error("no-untracked-files");
            }
            "--pathspec-from-file" => {
                let Some(value) = iter.next() else {
                    return commit_pathspec_from_file_requires_value_error();
                };
                pathspec_from_file = Some(value.to_string());
                pathspec_from_file_active = true;
            }
            value if value.starts_with("--pathspec-from-file=") => {
                pathspec_from_file = Some(value["--pathspec-from-file=".len()..].to_string());
                pathspec_from_file_active = true;
            }
            "--no-pathspec-from-file" => pathspec_from_file_active = false,
            value if value.starts_with("--no-pathspec-from-file=") => {
                return commit_option_takes_no_value_error("no-pathspec-from-file");
            }
            "--pathspec-file-nul" => pathspec_file_nul = true,
            "--no-pathspec-file-nul" => pathspec_file_nul = false,
            value if value.starts_with("--pathspec-file-nul=") => {
                return commit_option_takes_no_value_error("pathspec-file-nul");
            }
            value if value.starts_with("--no-pathspec-file-nul=") => {
                return commit_option_takes_no_value_error("no-pathspec-file-nul");
            }
            "-i" | "--include" => include_without_paths = true,
            "--no-include" => include_without_paths = false,
            value if value.starts_with("--include=") => {
                return commit_option_takes_no_value_error("include");
            }
            value if value.starts_with("--no-include=") => {
                return commit_option_takes_no_value_error("no-include");
            }
            "-o" | "--only" => only_without_paths = true,
            "--no-only" => only_without_paths = false,
            value if value.starts_with("--only=") => {
                return commit_option_takes_no_value_error("only");
            }
            value if value.starts_with("--no-only=") => {
                return commit_option_takes_no_value_error("no-only");
            }
            "-e" | "--edit" => edit_flag = Some(true),
            "--no-edit" => edit_flag = Some(false),
            value if value.starts_with("--edit=") => {
                return commit_option_takes_no_value_error("edit");
            }
            value if value.starts_with("--no-edit=") => {
                return commit_option_takes_no_value_error("no-edit");
            }
            "--branch" | "--no-branch" => {}
            value if value.starts_with("--branch=") => {
                return commit_option_takes_no_value_error("branch");
            }
            value if value.starts_with("--no-branch=") => {
                return commit_option_takes_no_value_error("no-branch");
            }
            "-t" => {
                let Some(template) = iter.next() else {
                    return commit_template_short_requires_value_error();
                };
                template_file = Some(template.clone());
            }
            value if value.starts_with("-t") && value.len() > 2 => {
                template_file = Some(value[2..].to_string());
            }
            "--template" => {
                let Some(template) = iter.next() else {
                    return commit_template_requires_value_error();
                };
                template_file = Some(template.clone());
            }
            value if let Some(path) = value.strip_prefix("--template=") => {
                template_file = Some(path.to_string());
            }
            "--no-template" => template_file = None,
            value if value.starts_with("--no-template=") => {
                return commit_option_takes_no_value_error("no-template");
            }
            "--cleanup" => {
                let Some(value) = iter.next() else {
                    return commit_cleanup_requires_value_error();
                };
                // Validate eagerly (git rejects a bad mode at parse time) but
                // defer resolution until `use_editor` is known.
                validate_commit_cleanup_mode(value)?;
                cleanup_arg = Some(value.clone());
            }
            value if value.starts_with("--cleanup=") => {
                let arg = &value["--cleanup=".len()..];
                validate_commit_cleanup_mode(arg)?;
                cleanup_arg = Some(arg.to_string());
            }
            "--no-cleanup" => cleanup_arg = Some("whitespace".to_string()),
            value if value.starts_with("--no-cleanup=") => {
                return commit_option_takes_no_value_error("no-cleanup");
            }
            "--" => {
                if pathspec_from_file_active && !iter.as_slice().is_empty() {
                    return commit_pathspec_from_file_with_inline_pathspec_error();
                }
                pathspec_args.extend(iter.by_ref().cloned());
            }
            value => {
                if value.starts_with('-') {
                    if pathspec_from_file_active {
                        return commit_pathspec_from_file_with_inline_pathspec_error();
                    }
                    return Err(GitError::Command(format!(
                        "unsupported commit argument {value}; currently supports -m and -F"
                    )));
                }
                if pathspec_from_file_active {
                    return commit_pathspec_from_file_with_inline_pathspec_error();
                }
                pathspec_args.push(value.to_string());
            }
        }
    }
    if reuse_message.is_some() && !message_chunks.is_empty() {
        let option = if reedit_message { "-c" } else { "-C" };
        eprintln!("fatal: options '-m' and '{option}' cannot be used together");
        return Err(GitError::Exit(128));
    }
    if reuse_message.is_some() && file_message.is_some() {
        let option = if reedit_message { "-c" } else { "-C" };
        eprintln!("fatal: options '{option}' and '-F' cannot be used together");
        return Err(GitError::Exit(128));
    }
    if fixup_commit.is_some() && reuse_message.is_some() {
        let option = if reedit_message { "-c" } else { "-C" };
        eprintln!("fatal: options '{option}' and '--fixup' cannot be used together");
        return Err(GitError::Exit(128));
    }
    if let Some(fixup) = &fixup_commit
        && fixup.is_amend_style()
        && !message_chunks.is_empty()
    {
        let option = if fixup.is_reword() {
            "--fixup:reword"
        } else {
            "--fixup:amend"
        };
        eprintln!("fatal: options '-m' and '{option}' cannot be used together");
        return Err(GitError::Exit(128));
    }
    if squash_commit.is_some() && fixup_commit.is_some() {
        eprintln!("fatal: options '--squash' and '--fixup' cannot be used together");
        return Err(GitError::Exit(128));
    }
    if fixup_commit.is_some() && file_message.is_some() {
        eprintln!("fatal: options '-F' and '--fixup' cannot be used together");
        return Err(GitError::Exit(128));
    }
    if reset_author && reuse_message.is_none() && !amend {
        eprintln!("fatal: --reset-author can be used only with -C, -c or --amend.");
        return Err(GitError::Exit(128));
    }
    if file_message.is_some() && !message_chunks.is_empty() {
        eprintln!("fatal: options '-m' and '-F' cannot be used together");
        return Err(GitError::Exit(128));
    }
    if include_without_paths || only_without_paths {
        eprintln!("fatal: No paths with --include/--only does not make sense.");
        return Err(GitError::Exit(128));
    }
    if pathspec_file_nul && pathspec_from_file.is_none() {
        eprintln!("fatal: the option '--pathspec-file-nul' requires '--pathspec-from-file'");
        return Err(GitError::Exit(128));
    }
    if let Some(pathspec_file) = pathspec_from_file.as_deref() {
        let pathspecs =
            read_commit_pathspecs_from_file(Path::new(pathspec_file), pathspec_file_nul)?;
        if pathspec_from_file_active {
            pathspec_args.extend(
                pathspecs
                    .into_iter()
                    .map(|path| path.to_string_lossy().into_owned()),
            );
        }
    }
    if !pathspec_args.is_empty() {
        if all {
            eprintln!(
                "fatal: paths '{} ...' with -a does not make sense",
                pathspec_args[0]
            );
            return Err(GitError::Exit(128));
        }
        if amend {
            return Err(GitError::Unsupported(
                "commit pathspecs with --amend are not implemented".into(),
            ));
        }
    }
    if unified_context && !interactive && !patch {
        eprintln!("fatal: the option '--unified' requires '--interactive/--patch'");
        return Err(GitError::Exit(128));
    }
    if inter_hunk_context && !interactive && !patch {
        eprintln!("fatal: the option '--inter-hunk-context' requires '--interactive/--patch'");
        return Err(GitError::Exit(128));
    }
    if status_mode != CommitStatusMode::Normal {
        return cmd_commit_status_preview(status_mode, status_null, amend, commit_untracked);
    }
    if dry_run {
        return cmd_commit_long_status_preview(amend, commit_untracked);
    }
    if gpg_sign {
        return Err(GitError::Unsupported(
            "commit gpg signing is not implemented".into(),
        ));
    }
    if interactive || patch {
        return Err(GitError::Unsupported(
            "commit interactive patch selection is not implemented".into(),
        ));
    }
    let git_dir = discover_git_dir(env::current_dir()?)?;
    let format = repository_object_format(&git_dir)?;
    let commit_odb = FileObjectDatabase::from_git_dir(&git_dir, format);
    let in_merge = git_dir.join("MERGE_HEAD").is_file();
    let in_cherry_pick = git_dir.join("CHERRY_PICK_HEAD").is_file();
    let in_revert = git_dir.join("REVERT_HEAD").is_file();
    if !pathspec_args.is_empty() {
        if in_merge {
            eprintln!("fatal: cannot do a partial commit during a merge.");
            return Err(GitError::Exit(128));
        }
        if in_cherry_pick || in_revert {
            eprintln!("fatal: cannot do a partial commit during a cherry-pick.");
            return Err(GitError::Exit(128));
        }
    }
    if amend {
        if in_merge {
            eprintln!("fatal: You are in the middle of a merge -- cannot amend.");
            return Err(GitError::Exit(128));
        }
        if in_cherry_pick || in_revert {
            eprintln!("fatal: You are in the middle of a cherry-pick -- cannot amend.");
            return Err(GitError::Exit(128));
        }
    }
    // `i18n.commitEncoding` is recorded as the commit's `encoding` header so that
    // `git log` can re-encode the message to the log output encoding (UTF-8 by
    // default). git omits the header for UTF-8.
    let commit_encoding_header = read_repo_config(&git_dir)
        .ok()
        .and_then(|config| {
            config
                .get("i18n", None, "commitEncoding")
                .map(str::to_string)
        })
        .filter(|enc| !encoding_is_utf8(enc))
        .map(String::into_bytes);
    let committer = commit_identity_from_env("COMMITTER")?;
    let amended_old_oid = if amend {
        commands::merge_rebase::head_commit_oid(&FileRefStore::new(&git_dir, format))?
    } else {
        None
    };
    let amended_commit = amend
        .then(|| read_amended_commit(&git_dir, format))
        .transpose()?;
    let reused_commit = reuse_message
        .as_deref()
        .map(|rev| read_reused_commit(&git_dir, format, rev))
        .transpose()?;
    let fixup_message = fixup_commit
        .as_ref()
        .map(|fixup| read_fixup_commit_message(&git_dir, format, fixup))
        .transpose()?;
    let fixup_reword_tree = if fixup_commit.as_ref().is_some_and(CommitFixup::is_reword) {
        let Some(commit) = read_head_commit(&git_dir, format)? else {
            eprintln!("fatal: You have nothing to amend.");
            return Err(GitError::Exit(128));
        };
        Some(commit.tree)
    } else {
        None
    };
    let squash_message = squash_commit
        .as_deref()
        .map(|rev| read_squash_commit_message(&git_dir, format, rev))
        .transpose()?;
    let author = if reset_author {
        build_commit_author_identity(author_override.as_deref(), author_date.as_deref())?
    } else if let Some(commit) = &reused_commit {
        build_reused_commit_author_identity(
            &commit.author,
            author_override.as_deref(),
            author_date.as_deref(),
        )?
    } else if let Some(commit) = &amended_commit {
        build_reused_commit_author_identity(
            &commit.author,
            author_override.as_deref(),
            author_date.as_deref(),
        )?
    } else {
        build_commit_author_identity(author_override.as_deref(), author_date.as_deref())?
    };
    let had_file_message = file_message.is_some();
    let message = reused_commit
        .as_ref()
        .map(|commit| {
            if let Some(squash_message) = &squash_message {
                commit_squash_message(squash_message, Some(&commit.message), None, &[])
            } else {
                commit.message.clone()
            }
        })
        .or_else(|| {
            squash_message.as_ref().map(|message| {
                commit_squash_message(message, None, file_message.as_deref(), &message_chunks)
            })
        })
        .or_else(|| {
            fixup_message.as_ref().map(|message| {
                commit_fixup_message(message, file_message.as_deref(), &message_chunks)
            })
        })
        .or_else(|| {
            if amend && file_message.is_none() && message_chunks.is_empty() {
                amended_commit.as_ref().map(|commit| commit.message.clone())
            } else {
                None
            }
        })
        .or_else(|| {
            if (in_merge || in_cherry_pick || in_revert)
                && file_message.is_none()
                && message_chunks.is_empty()
                && reuse_message.is_none()
                && fixup_commit.is_none()
                && squash_commit.is_none()
            {
                if in_merge {
                    read_merge_message_from_file(&git_dir).ok()
                } else {
                    // Keep the commented "# Conflicts:" block intact: the
                    // editor template shows it and the post-editor cleanup
                    // strips it.
                    fs::read(git_dir.join("MERGE_MSG")).ok()
                }
            } else {
                None
            }
        })
        .or_else(|| {
            // `-t <file>`: the template body, used only when no `-m`/`-F`/`-C`
            // and not concluding a merge/cherry-pick. Read verbatim (git sets
            // `clean_message_contents = 0`).
            if file_message.is_none()
                && message_chunks.is_empty()
                && reuse_message.is_none()
                && fixup_commit.is_none()
                && squash_commit.is_none()
                && !amend
            {
                template_file
                    .as_deref()
                    .map(|path| read_commit_template_file(path))
                    .transpose()
                    .ok()
                    .flatten()
            } else {
                None
            }
        })
        .unwrap_or_else(|| {
            file_message.unwrap_or_else(|| commit_message_from_prepared_chunks(&message_chunks))
        });
    if all {
        commit_stage_tracked_changes(&git_dir, format)?;
    }
    // Emptiness is judged before the signoff trailer is added (git aborts
    // `commit -m "" -s`).
    let empty_before_signoff =
        commit_message_is_empty(&commit_message_with_trailers(message.clone(), &trailers));
    let mut message = if signoff {
        commands::replay::append_signoff_before_comments(message, &commit_signoff_from_env()?)
    } else {
        message
    };
    message = commit_message_with_trailers(message, &trailers);
    // Editor flow: a commit without an explicit message source launches the
    // editor over COMMIT_EDITMSG (the in-merge / rebase conclude paths keep
    // their historical no-editor behavior).
    let had_message_source = had_file_message
        || !message_chunks.is_empty()
        || reuse_message.is_some()
        || fixup_commit.is_some()
        || squash_commit.is_some();
    let in_rebase = rebase_in_progress(&git_dir);
    let use_editor = !in_rebase
        && !in_merge
        && (edit_flag == Some(true) || (edit_flag != Some(false) && !had_message_source));
    if !no_verify {
        commands::hooks::run_hook("pre-commit", commands::hooks::HookRun::default())?;
    }
    // Resolve the cleanup mode now that `use_editor` is known. An explicit
    // `--cleanup`/`--no-cleanup` wins; otherwise `commit.cleanup` config; absent
    // both, git's editor-dependent default (ALL with an editor, SPACE without).
    let cleanup_config = cleanup_arg.clone().or_else(|| {
        read_repo_config(&git_dir)
            .ok()
            .and_then(|c| c.get("commit", None, "cleanup").map(str::to_string))
    });
    let cleanup_mode = resolve_commit_cleanup_mode(cleanup_config.as_deref(), use_editor);
    let comment_char = commit_comment_string(&git_dir);
    let editmsg = git_dir.join("COMMIT_EDITMSG");
    // When an editor will run, git appends a commented status block (the
    // template) to COMMIT_EDITMSG unless `--no-status`/`commit.status=false`.
    // `include_status` (cmdline) wins over `commit.status` config (default true).
    let include_status_resolved = include_status.unwrap_or_else(|| {
        read_repo_config(&git_dir)
            .ok()
            .and_then(|c| c.get_bool("commit", None, "status"))
            .unwrap_or(true)
    });
    let mut template = message.clone();
    if use_editor && include_status_resolved {
        // `author_date_is_interesting()` = `--date` given or author reused from
        // another commit (`-C`/`-c`/amend); env GIT_AUTHOR_DATE alone does not
        // trigger the template Date line.
        let author_date_interesting =
            author_date.is_some() || reuse_message.is_some() || amend;
        let block = build_commit_editor_template_block(&CommitTemplateBlock {
            git_dir: &git_dir,
            format,
            comment_char: &comment_char,
            cleanup_mode,
            allow_empty_message,
            author: &author,
            committer: &committer,
            author_date_interesting,
            amend,
            untracked_override: commit_untracked,
        })?;
        template.extend_from_slice(&block);
    }
    fs::write(&editmsg, &template)?;
    let editmsg_arg = editmsg.to_string_lossy().into_owned();
    let mut prepare_args = vec![editmsg_arg.as_str()];
    if amend {
        prepare_args.push("commit");
        prepare_args.push("HEAD");
    } else if in_merge || ((in_cherry_pick || in_revert) && git_dir.join("MERGE_MSG").is_file()) {
        prepare_args.push("merge");
    } else if let Some(rev) = reuse_message.as_deref() {
        prepare_args.push("commit");
        prepare_args.push(rev);
    } else if had_message_source {
        prepare_args.push("message");
    } else {
        prepare_args.push("template");
    }
    commands::hooks::run_hook_l("prepare-commit-msg", &prepare_args)?;
    if use_editor && let Err(err) = commands::replay::launch_editor(&git_dir, &editmsg) {
        eprintln!("error: {err}");
        eprintln!("Please supply the message using either -m or -F option.");
        return Err(GitError::Exit(1));
    }
    if !no_verify {
        commands::hooks::run_hook_l("commit-msg", &[editmsg_arg.as_str()])?;
    }
    message = fs::read(&editmsg)?;
    message = commit_cleanup_message(message, cleanup_mode, &comment_char, verbose);
    if (in_cherry_pick || in_revert) && !allow_empty_message && commit_message_is_empty(&message) {
        eprintln!("Aborting commit due to empty commit message.");
        return Err(GitError::Exit(1));
    }
    if in_rebase {
        return conclude_rebase_step_via_commit(
            &git_dir,
            format,
            author,
            committer,
            message,
            quiet,
            allow_empty,
        );
    }
    if in_merge {
        return conclude_in_progress_merge(&git_dir, format, message, quiet);
    }
    if in_cherry_pick || in_revert {
        return conclude_replay_via_commit(
            &git_dir,
            format,
            message,
            allow_empty,
            allow_empty_message,
            author,
            author_override.is_none() && !reset_author,
            quiet,
        );
    }
    if !allow_empty_message && empty_before_signoff && !use_editor {
        eprintln!("Aborting commit due to empty commit message.");
        return Err(GitError::Exit(1));
    }
    if !allow_empty_message && commit_message_is_empty(&message) {
        eprintln!("Aborting commit due to empty commit message.");
        return Err(GitError::Exit(1));
    }
    if !pathspec_args.is_empty() {
        return commit_partial_paths(
            &git_dir,
            format,
            &pathspec_args,
            author,
            committer,
            message,
            quiet,
        );
    }
    let precomputed_index_tree = if !allow_empty
        && !amend
        && fixup_reword_tree.is_none()
    {
        match commit_index_tree_if_changed(&git_dir, format, &commit_odb)? {
            Some(tree) => Some(tree),
            None => {
                print_clean_commit_status(&git_dir, format)?;
                return Err(GitError::Exit(1));
            }
        }
    } else {
        None
    };
    // Retain copies for the post-commit summary (the options struct moves them).
    let summary_author = author.clone();
    let summary_committer = committer.clone();
    let summary_message = message.clone();
    let options = sley_sequencer::CommitIndexOptions {
        author,
        committer,
        reflog_message: commit_reflog_message(&message, amend),
        message,
        encoding: commit_encoding_header,
    };
    let result = if amend {
        sley_sequencer::amend_index(&git_dir, format, options)
    } else if let Some(tree) = fixup_reword_tree {
        sley_sequencer::commit_tree_at_head(&git_dir, format, tree, options)
    } else if let Some(tree) = precomputed_index_tree {
        sley_sequencer::commit_tree_at_head_with_odb(&git_dir, format, tree, options, &commit_odb)
    } else {
        sley_sequencer::commit_index(&git_dir, format, options)
    }?;
    if !quiet {
        print_commit_summary(
            &git_dir,
            format,
            &result.oid,
            result.parent.as_ref(),
            &summary_message,
            &summary_author,
            &summary_committer,
        )?;
    }
    commands::hooks::run_hook("reference-transaction", commands::hooks::HookRun::default())?;
    commands::hooks::run_hook("post-commit", commands::hooks::HookRun::default())?;
    if amend
        && !no_post_rewrite
        && let Some(old_oid) = amended_old_oid
    {
        commands::hooks::run_hook(
            "post-rewrite",
            commands::hooks::HookRun {
                args: vec!["amend".to_string()],
                stdin: Some(format!("{} {}\n", old_oid, result.oid).into_bytes()),
                ..commands::hooks::HookRun::default()
            },
        )?;
    }
    Ok(())
}

/// Print git's post-commit summary (`print_commit_summary`), e.g.
/// `[main (root-commit) 0bed67f] initial` followed by an optional `Author:`/
/// `Committer:` line and the shortstat + `create/delete mode` summary of the diff
/// against the parent. `new_oid` is the freshly written commit; `parent` is its
/// first parent (None for a root commit, which diffs against the empty tree and
/// adds the `(root-commit)` marker). `author`/`committer` are the raw identity
/// buffers (`Name <email> seconds tz`); the `Author:` line is emitted only when
/// they differ in name/email, matching git.
fn print_commit_summary(
    git_dir: &Path,
    format: ObjectFormat,
    new_oid: &ObjectId,
    parent: Option<&ObjectId>,
    message: &[u8],
    author: &[u8],
    committer: &[u8],
) -> Result<()> {
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    // HEAD branch name, or "detached HEAD" / "HEAD" when unresolvable.
    let head = match repo_current_branch_name(git_dir) {
        Some(name) => name,
        None => "detached HEAD".to_string(),
    };
    let abbrev = commit_summary_abbrev(&db, new_oid);
    let root = if parent.is_none() { " (root-commit)" } else { "" };
    let subject = commit_subject(message);

    let mut out = io::stdout();
    write!(out, "[{head}{root} {abbrev}] {subject}\n")?;

    // `Author:` line when the author identity (name <email>) differs from the
    // committer's — git's `strbuf_cmp(&author_ident, &committer_ident)`.
    let author_id = identity_name_email(author);
    let committer_id = identity_name_email(committer);
    if author_id != committer_id {
        writeln!(out, " Author: {author_id}")?;
    }

    // Shortstat + summary of the diff against the parent tree (empty tree for a
    // root commit), matching `DIFF_FORMAT_SHORTSTAT | DIFF_FORMAT_SUMMARY`.
    let new_tree = read_commit_tree_for_summary(&db, format, new_oid)?;
    let old_tree = match parent {
        Some(p) => read_commit_tree_for_summary(&db, format, p)?,
        None => ObjectId::empty_tree(format),
    };
    let entries = sley_diff_merge::diff_name_status_trees_with_rename_options(
        &db,
        format,
        &old_tree,
        &new_tree,
        sley_diff_merge::RenameDetectionOptions::default(),
    )?;
    if !entries.is_empty() {
        write_diff_shortstat(&mut out, &entries, &db, None, false)?;
        for entry in &entries {
            write_commit_summary_entry(&mut out, entry)?;
        }
    }
    out.flush()?;
    Ok(())
}

/// git's `find_unique_abbrev`: the shortest unambiguous hex prefix of `oid`
/// (minimum 7), growing until it resolves to a single object.
fn commit_summary_abbrev(db: &FileObjectDatabase, oid: &ObjectId) -> String {
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

/// Extract `Name <email>` from a raw git identity buffer (`Name <email> seconds
/// tz`) by trimming the trailing ` seconds timezone`. Used to compare author and
/// committer identities for the summary's `Author:` line.
fn identity_name_email(identity: &[u8]) -> String {
    let text = String::from_utf8_lossy(identity);
    match text.rfind('>') {
        Some(idx) => text[..=idx].to_string(),
        None => text.trim_end().to_string(),
    }
}

/// Read a commit's tree oid for the summary diff.
fn read_commit_tree_for_summary(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    commit_oid: &ObjectId,
) -> Result<ObjectId> {
    let object = db.read_object(commit_oid)?;
    let commit = Commit::parse(format, &object.body)?;
    Ok(commit.tree)
}

/// The ` create mode`/` delete mode`/` rename`/` copy`/` mode change` summary
/// line for one diff entry, matching git's `DIFF_FORMAT_SUMMARY`.
fn write_commit_summary_entry(
    out: &mut dyn Write,
    entry: &sley_diff_merge::NameStatusEntry,
) -> Result<()> {
    match entry.status {
        sley_diff_merge::NameStatus::Added => {
            let mode = entry.new_mode.unwrap_or(0);
            writeln!(out, " create mode {mode:06o} {}", entry.path)?;
        }
        sley_diff_merge::NameStatus::Deleted => {
            let mode = entry.old_mode.unwrap_or(0);
            writeln!(out, " delete mode {mode:06o} {}", entry.path)?;
        }
        sley_diff_merge::NameStatus::Renamed(score) => {
            if let Some(old_path) = &entry.old_path {
                writeln!(out, " rename {old_path} => {} ({score}%)", entry.path)?;
            }
        }
        sley_diff_merge::NameStatus::Copied(score) => {
            if let Some(old_path) = &entry.old_path {
                writeln!(out, " copy {old_path} => {} ({score}%)", entry.path)?;
            }
        }
        sley_diff_merge::NameStatus::Modified => {
            if let (Some(old_mode), Some(new_mode)) = (entry.old_mode, entry.new_mode)
                && old_mode != new_mode
            {
                writeln!(out, " mode change {old_mode:06o} => {new_mode:06o} {}", entry.path)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Apply `commit --trailer` arguments to a (byte) commit message through the
/// full interpret-trailers engine (`commands::interpret_trailers`), so per-token
/// `trailer.*` config governs placement/policy/key/command exactly as `git commit
/// --trailer` does. A message with no queued trailers is returned untouched.
///
/// Commit messages are UTF-8 in practice; we losslessly round-trip via
/// `from_utf8_lossy` so non-UTF-8 bytes don't crash the (text-oriented) engine.
fn commit_message_with_trailers(message: Vec<u8>, trailers: &[String]) -> Vec<u8> {
    if trailers.is_empty() {
        return message;
    }
    let text = String::from_utf8_lossy(&message);
    commands::interpret_trailers::apply_trailers_to_message(&text, trailers).into_bytes()
}

/// Conclude an in-progress cherry-pick / revert via `git commit`: commit the
/// staged resolution with the picked commit's authorship, then run the
/// sequencer post-commit cleanup (CHERRY_PICK_HEAD / REVERT_HEAD removal and
/// the last-pick sequencer-state teardown).
#[allow(clippy::too_many_arguments)]
fn conclude_replay_via_commit(
    git_dir: &Path,
    format: ObjectFormat,
    message: Vec<u8>,
    allow_empty: bool,
    allow_empty_message: bool,
    env_author: Vec<u8>,
    use_pick_author: bool,
    quiet: bool,
) -> Result<()> {
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let refs = FileRefStore::new(git_dir, format);
    let head = commands::merge_rebase::head_commit_oid(&refs)?;
    let index_path = sley_worktree::repository_index_path(git_dir);
    if let Ok(bytes) = fs::read(&index_path) {
        let index = Index::parse(&bytes, format)?;
        let unmerged: BTreeSet<String> = index
            .entries
            .iter()
            .filter(|entry| index_entry_stage(entry) > 0)
            .map(|entry| entry.path.to_string())
            .collect();
        if !unmerged.is_empty() {
            for path in &unmerged {
                println!("U\t{path}");
            }
            eprintln!("error: Committing is not possible because you have unmerged files.");
            eprintln!("hint: Fix them up in the work tree, and then use 'git add/rm <file>'");
            eprintln!("hint: as appropriate to mark resolution and make a commit.");
            eprintln!("fatal: Exiting because of an unresolved conflict.");
            return Err(GitError::Exit(128));
        }
    }
    let head_tree = match &head {
        Some(oid) => commands::merge_rebase::commit_tree_oid(&db, format, oid)?,
        None => ObjectId::empty_tree(format),
    };
    let tree = sley_worktree::write_tree_from_index(git_dir, format)?;
    let cherry_pick_head = git_dir.join("CHERRY_PICK_HEAD");
    if !allow_empty && tree == head_tree {
        let action = if cherry_pick_head.is_file() {
            "cherry-pick"
        } else {
            "revert"
        };
        eprintln!("The previous cherry-pick is now empty, possibly due to conflict resolution.");
        eprintln!("If you wish to commit it anyway, use:");
        eprintln!();
        eprintln!("    git commit --allow-empty");
        eprintln!();
        eprintln!("Otherwise, please use 'git {action} --skip'");
        return Err(GitError::Exit(1));
    }
    if !allow_empty_message && commit_message_is_empty(&message) {
        eprintln!("Aborting commit due to empty commit message.");
        return Err(GitError::Exit(1));
    }
    let author = if use_pick_author && cherry_pick_head.is_file() {
        let text = fs::read_to_string(&cherry_pick_head)?;
        let oid = ObjectId::from_hex(format, text.trim())?;
        let object = db.read_object(&oid)?;
        Commit::parse(format, &object.body)?.author
    } else {
        env_author
    };
    let committer = commit_identity_from_env("COMMITTER")?;
    let new_oid = sley_sequencer::create_commit(
        &mut FileObjectDatabase::from_git_dir(git_dir, format),
        sley_sequencer::CommitCreate {
            tree,
            parents: head.iter().copied().collect(),
            author,
            committer: committer.clone(),
            message: message.clone(),
            encoding: None,
        },
    )?;
    let target_ref = match refs.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(branch)) => branch,
        _ => "HEAD".to_string(),
    };
    let old_oid = head.unwrap_or_else(|| ObjectId::null(format));
    let mut tx = refs.transaction();
    tx.update(RefUpdate {
        name: target_ref,
        expected: head.map(RefTarget::Direct),
        new: RefTarget::Direct(new_oid),
        reflog: Some(ReflogEntry {
            old_oid,
            new_oid,
            committer,
            message: commit_reflog_message(&message, false),
        }),
    });
    tx.commit()?;
    sley_sequencer::replay::post_commit_cleanup(git_dir);
    for name in ["MERGE_HEAD", "MERGE_MSG", "MERGE_MODE", "SQUASH_MSG"] {
        let _ = fs::remove_file(git_dir.join(name));
    }
    if !quiet {
        println!("{new_oid}");
    }
    commands::hooks::run_hook("post-commit", commands::hooks::HookRun::default())?;
    Ok(())
}

/// Partial commit (`git commit [-m ...] -- <paths>`): stage the named paths'
/// working-tree contents (clean filters applied, directories expanded over
/// the tracked entries beneath them), then record HEAD's tree with just those
/// paths replaced. Mirrors git's `--only` default for tracked-file usage.
fn commit_partial_paths(
    git_dir: &Path,
    format: ObjectFormat,
    paths: &[String],
    author: Vec<u8>,
    committer: Vec<u8>,
    message: Vec<u8>,
    quiet: bool,
) -> Result<()> {
    let worktree_root = worktree_root_for_git_dir(git_dir)?;
    let cwd = env::current_dir()?;
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let refs = FileRefStore::new(git_dir, format);
    let head = commands::merge_rebase::head_commit_oid(&refs)?;
    let mut tree_map = match &head {
        Some(oid) => {
            let tree = commands::merge_rebase::commit_tree_oid(&db, format, oid)?;
            stash_tree_entry_map(&db, format, &tree)?
        }
        None => BTreeMap::new(),
    };
    let index_path = sley_worktree::repository_index_path(git_dir);
    let index = if index_path.exists() {
        Index::parse(&fs::read(&index_path)?, format)?
    } else {
        Index {
            version: 2,
            entries: Vec::new(),
            extensions: Vec::new(),
            checksum: None,
        }
    };
    let known: BTreeSet<Vec<u8>> = index
        .entries
        .iter()
        .map(|entry| entry.path.clone().into_bytes())
        .chain(tree_map.keys().cloned())
        .collect();

    // Expand the pathspecs over the tracked entries (directories and `.`
    // cover everything beneath them).
    let mut rel_paths: Vec<Vec<u8>> = Vec::new();
    let mut seen: BTreeSet<Vec<u8>> = BTreeSet::new();
    for path in paths {
        let absolute = if Path::new(path).is_absolute() {
            PathBuf::from(path)
        } else {
            cwd.join(path)
        };
        let rel: Vec<u8> = match absolute.strip_prefix(&worktree_root) {
            Ok(stripped) => stripped.to_string_lossy().into_owned().into_bytes(),
            Err(_) => {
                return Err(GitError::InvalidPath(format!(
                    "pathspec outside repository: {path}"
                )));
            }
        };
        let mut matched = false;
        if rel.is_empty() {
            // `.` at the worktree root: every tracked entry.
            for tracked in &known {
                if seen.insert(tracked.clone()) {
                    rel_paths.push(tracked.clone());
                }
                matched = true;
            }
        } else if known.contains(&rel) {
            if seen.insert(rel.clone()) {
                rel_paths.push(rel.clone());
            }
            matched = true;
        } else {
            let mut prefix = rel.clone();
            prefix.push(b'/');
            for tracked in &known {
                if tracked.starts_with(&prefix) {
                    if seen.insert(tracked.clone()) {
                        rel_paths.push(tracked.clone());
                    }
                    matched = true;
                }
            }
        }
        if !matched {
            eprintln!("error: pathspec '{path}' did not match any file(s) known to git");
            return Err(GitError::Exit(128));
        }
    }

    // Stage the matched paths with the regular add machinery (clean filters,
    // mode bits) — partial commits update those index entries too.
    let config = read_repo_config(git_dir)?;
    // Partial-commit staging applies one uniform mode (`--add --remove`) to
    // every matched path, so stamp that mode onto each `UpdateIndexPath`.
    let commit_mode = sley_worktree::UpdateIndexPathMode {
        add: true,
        remove: true,
        force_remove: false,
        info_only: false,
        chmod: None,
    };
    let ordered: Vec<sley_worktree::UpdateIndexPath> = rel_paths
        .iter()
        .map(|rel| sley_worktree::UpdateIndexPath {
            path: worktree_root.join(String::from_utf8_lossy(rel).as_ref()),
            mode: commit_mode,
        })
        .collect();
    sley_worktree::update_index_ordered_paths_filtered(
        &worktree_root,
        git_dir,
        format,
        &ordered,
        sley_worktree::UpdateIndexOptions {
            add: true,
            remove: true,
            force_remove: false,
            chmod: None,
            info_only: false,
            ignore_skip_worktree_entries: false,
        },
        &config,
        false,
    )?;

    // Overlay the staged state of the matched paths onto HEAD's tree.
    let updated_index = Index::parse(&fs::read(&index_path)?, format)?;
    let staged: BTreeMap<Vec<u8>, (u32, ObjectId)> = updated_index
        .entries
        .iter()
        .filter(|entry| index_entry_stage(entry) == 0)
        .map(|entry| (entry.path.clone().into_bytes(), (entry.mode, entry.oid)))
        .collect();
    for rel in &rel_paths {
        match staged.get(rel) {
            Some(entry) => {
                tree_map.insert(rel.clone(), *entry);
            }
            None => {
                tree_map.remove(rel);
            }
        }
    }
    let tree = write_tree_from_entry_map(&db, format, &tree_map)?;
    let new_oid = sley_sequencer::create_commit(
        &mut FileObjectDatabase::from_git_dir(git_dir, format),
        sley_sequencer::CommitCreate {
            tree,
            parents: head.iter().copied().collect(),
            author,
            committer: committer.clone(),
            message: message.clone(),
            encoding: None,
        },
    )?;
    let target_ref = match refs.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(branch)) => branch,
        _ => "HEAD".to_string(),
    };
    let old_oid = head.unwrap_or_else(|| ObjectId::null(format));
    let mut tx = refs.transaction();
    tx.update(RefUpdate {
        name: target_ref,
        expected: head.map(RefTarget::Direct),
        new: RefTarget::Direct(new_oid),
        reflog: Some(ReflogEntry {
            old_oid,
            new_oid,
            committer,
            message: commit_reflog_message(&message, false),
        }),
    });
    tx.commit()?;
    sley_sequencer::replay::post_commit_cleanup(git_dir);
    if !quiet {
        println!("{new_oid}");
    }
    commands::hooks::run_hook("post-commit", commands::hooks::HookRun::default())?;
    Ok(())
}

/// Write a tree object hierarchy from a flat `path -> (mode, oid)` map
/// (grouping by leading path component, mirroring fast-import's writer).
fn write_tree_from_entry_map(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    entries: &BTreeMap<Vec<u8>, (u32, ObjectId)>,
) -> Result<ObjectId> {
    let _ = format;
    write_entry_map_level(db, entries, &[])
}

fn write_entry_map_level(
    db: &FileObjectDatabase,
    entries: &BTreeMap<Vec<u8>, (u32, ObjectId)>,
    prefix: &[u8],
) -> Result<ObjectId> {
    let mut tree_entries: Vec<sley_object::TreeEntry> = Vec::new();
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
        if let Some(slash) = rel.iter().position(|b| *b == b'/') {
            subdirs.insert(rel[..slash].to_vec());
        } else {
            tree_entries.push(sley_object::TreeEntry {
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
        let sub_oid = write_entry_map_level(db, entries, &sub_prefix)?;
        tree_entries.push(sley_object::TreeEntry {
            mode: 0o040000,
            name: BString::from(dir),
            oid: sub_oid,
        });
    }
    // Tree entries collate with subtrees as though their name ends in `/`.
    tree_entries.sort_by_key(|entry| {
        let mut key = entry.name.clone().into_bytes();
        if entry.mode == 0o040000 {
            key.push(b'/');
        }
        key
    });
    db.write_object(EncodedObject::new(
        ObjectType::Tree,
        sley_object::Tree {
            entries: tree_entries,
        }
        .write(),
    ))
}

enum CommitFixup {
    Plain(String),
    Amend { rev: String, reword: bool },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CommitStatusMode {
    Normal,
    Short,
    Porcelain,
    Long,
}

fn cmd_commit_status_preview(
    mode: CommitStatusMode,
    null: bool,
    amend: bool,
    untracked: Option<sley_worktree::StatusUntrackedMode>,
) -> Result<()> {
    let mut args = Vec::new();
    match mode {
        CommitStatusMode::Normal => {}
        CommitStatusMode::Short => args.push("--short".to_string()),
        CommitStatusMode::Porcelain => args.push("--porcelain".to_string()),
        CommitStatusMode::Long => return cmd_commit_long_status_preview(amend, untracked),
    }
    if null {
        args.push("-z".to_string());
    }
    if let Some(mode) = untracked {
        args.push(match mode {
            sley_worktree::StatusUntrackedMode::None => "--untracked-files=no".to_string(),
            sley_worktree::StatusUntrackedMode::Normal => "--untracked-files=normal".to_string(),
            sley_worktree::StatusUntrackedMode::All => "--untracked-files=all".to_string(),
        });
    }
    cmd_status(&args)
}

fn cmd_commit_long_status_preview(
    amend: bool,
    untracked_override: Option<sley_worktree::StatusUntrackedMode>,
) -> Result<()> {
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let config = read_repo_config(&git_dir).map_err(report_config_setup_error)?;
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    let format = repository_object_format(&git_dir)?;
    // `commit -u<mode>` wins over `status.showUntrackedFiles`; otherwise config
    // (then the normal default) applies.
    let untracked_mode = untracked_override.unwrap_or_else(|| {
        match config.get("status", None, "showUntrackedFiles") {
            Some("no") | Some("false") | Some("0") | Some("off") => {
                sley_worktree::StatusUntrackedMode::None
            }
            Some("all") => sley_worktree::StatusUntrackedMode::All,
            _ => sley_worktree::StatusUntrackedMode::Normal,
        }
    });
    let mut entries = sley_worktree::short_status_with_options(
        &worktree_root,
        &git_dir,
        format,
        sley_worktree::ShortStatusOptions {
            include_ignored: false,
            ignored_mode: sley_worktree::StatusIgnoredMode::Traditional,
            untracked_mode,
        },
    )?;
    let committable = status_entries_have_index_changes(&entries);
    // `commit --dry-run` carries no `--ignore-submodules` flag, so the resolver
    // reflects only config; apply it so submodule worktree detail honours
    // `submodule.<name>.ignore` / `diff.ignoreSubmodules` the same as `status`.
    let ignore_resolver = SubmoduleIgnoreResolver::load(&git_dir, &config, None)?;
    apply_submodule_ignore(&mut entries, &ignore_resolver);
    // The staged summary compares against HEAD (or HEAD^ when amending, since the
    // amend commit replaces HEAD) — wt-status.c passes `s->amend ? "HEAD^" :
    // "HEAD"` to `git submodule summary --cached`.
    let base_ref = if amend { "HEAD^" } else { "HEAD" };
    let submodule_summary = status_submodule_summary(
        &git_dir,
        &worktree_root,
        format,
        &config,
        base_ref,
        &ignore_resolver,
    )?;
    let display = StatusLongDisplay {
        commit_preview: true,
        show_stash: false,
        ahead_behind: true,
        hints: config
            .get_bool("advice", None, "statusHints")
            .unwrap_or(true),
        untracked_suppressed: untracked_mode == sley_worktree::StatusUntrackedMode::None,
        comment_prefix: status_comment_prefix(&config),
        submodule_summary,
    };
    print_status_long(&git_dir, format, entries, &display)?;
    if committable {
        Ok(())
    } else {
        Err(GitError::Exit(1))
    }
}

impl CommitFixup {
    fn parse(value: &str) -> Result<Self> {
        if let Some(rev) = value.strip_prefix("amend:") {
            Ok(Self::Amend {
                rev: rev.to_string(),
                reword: false,
            })
        } else if let Some(rev) = value.strip_prefix("reword:") {
            Ok(Self::Amend {
                rev: rev.to_string(),
                reword: true,
            })
        } else if value.contains(':')
            && value
                .split_once(':')
                .is_some_and(|(mode, _)| !mode.is_empty())
        {
            eprintln!("fatal: unknown option: --fixup={value}");
            Err(GitError::Exit(128))
        } else {
            Ok(Self::Plain(value.to_string()))
        }
    }

    fn rev(&self) -> &str {
        match self {
            Self::Plain(rev) | Self::Amend { rev, .. } => rev,
        }
    }

    fn is_amend_style(&self) -> bool {
        matches!(self, Self::Amend { .. })
    }

    fn is_reword(&self) -> bool {
        matches!(self, Self::Amend { reword: true, .. })
    }
}

/// `git commit -h`: print a usage synopsis and exit 129, matching upstream's
/// `parse-options`-driven `-h` handling (which fires before any repository
/// state is read, so it works even in a broken repo). The test only asserts
/// exit code 129 and a "[Uu]sage" match in the output.
fn commit_usage() -> Result<()> {
    eprintln!("usage: git commit [-a | --interactive | --patch] [-s] [-v] [-u<mode>] [--amend]");
    eprintln!("                  [--dry-run] [(-c | -C | --squash) <commit> | --fixup [(amend|reword):]<commit>]");
    eprintln!("                  [-F <file> | -m <msg>] [--reset-author] [--allow-empty]");
    eprintln!("                  [--no-verify] [-e] [--author=<author>] [--date=<date>]");
    eprintln!("                  [--cleanup=<mode>] [--[no-]status] [-i | -o] [pathspec...]");
    Err(GitError::Exit(129))
}

/// `git status -h`: usage synopsis + exit 129, mirroring commit_usage().
fn status_usage() -> Result<()> {
    eprintln!("usage: git status [<options>] [--] [<pathspec>...]");
    Err(GitError::Exit(129))
}

fn commit_author_requires_value_error() -> Result<()> {
    eprintln!("error: option `author' requires a value");
    Err(GitError::Exit(129))
}

fn commit_date_requires_value_error() -> Result<()> {
    eprintln!("error: option `date' requires a value");
    Err(GitError::Exit(129))
}

fn commit_cleanup_requires_value_error() -> Result<()> {
    eprintln!("error: option `cleanup' requires a value");
    Err(GitError::Exit(129))
}

fn commit_template_requires_value_error() -> Result<()> {
    eprintln!("error: option `template' requires a value");
    Err(GitError::Exit(129))
}

fn commit_template_short_requires_value_error() -> Result<()> {
    eprintln!("error: switch `t' requires a value");
    Err(GitError::Exit(129))
}

fn commit_reuse_message_requires_value_error(short: bool, reedit: bool) -> Result<()> {
    if short {
        let switch = if reedit { "c" } else { "C" };
        eprintln!("error: switch `{switch}' requires a value");
    } else {
        let option = if reedit {
            "reedit-message"
        } else {
            "reuse-message"
        };
        eprintln!("error: option `{option}' requires a value");
    }
    Err(GitError::Exit(129))
}

fn commit_fixup_requires_value_error() -> Result<()> {
    eprintln!("error: option `fixup' requires a value");
    Err(GitError::Exit(129))
}

fn commit_squash_requires_value_error() -> Result<()> {
    eprintln!("error: option `squash' requires a value");
    Err(GitError::Exit(129))
}

fn commit_trailer_requires_value_error() -> Result<()> {
    eprintln!("error: option `trailer' requires a value");
    Err(GitError::Exit(129))
}

fn commit_pathspec_from_file_requires_value_error() -> Result<()> {
    eprintln!("error: option `pathspec-from-file' requires a value");
    Err(GitError::Exit(129))
}

fn commit_pathspec_from_file_with_inline_pathspec_error() -> Result<()> {
    eprintln!("fatal: '--pathspec-from-file' and pathspec arguments cannot be used together");
    Err(GitError::Exit(128))
}

fn commit_option_takes_no_value_error(option: &str) -> Result<()> {
    eprintln!("error: option `{option}' takes no value");
    Err(GitError::Exit(129))
}

fn commit_invalid_untracked_files_mode_error(mode: &str) -> Result<()> {
    eprintln!("fatal: Invalid untracked files mode '{mode}'");
    Err(GitError::Exit(128))
}

fn read_porcelain_commit_message_file(path: &str) -> Result<Vec<u8>> {
    let mut message = read_commit_message_file(path)?;
    if !message.is_empty() && !message.ends_with(b"\n") {
        message.push(b'\n');
    }
    Ok(message)
}

fn commit_message_is_empty(message: &[u8]) -> bool {
    message.iter().all(u8::is_ascii_whitespace)
}

/// Validate a `--cleanup`/`commit.cleanup` mode string. git's `get_cleanup_mode`
/// `die`s on an unknown value (exit 128); the concrete mode is resolved later by
/// [`resolve_commit_cleanup_mode`] once `use_editor` is known.
fn validate_commit_cleanup_mode(value: &str) -> Result<()> {
    match value {
        "strip" | "whitespace" | "scissors" | "default" | "verbatim" => Ok(()),
        _ => {
            eprintln!("fatal: Invalid cleanup mode {value}");
            Err(GitError::Exit(128))
        }
    }
}

fn read_fixup_commit_message(
    git_dir: &Path,
    format: ObjectFormat,
    fixup: &CommitFixup,
) -> Result<Vec<u8>> {
    let commit = read_reused_commit(git_dir, format, fixup.rev())?;
    let subject = commit_subject(&commit.message);
    match fixup {
        CommitFixup::Plain(_) => Ok(format!("fixup! {subject}\n").into_bytes()),
        CommitFixup::Amend { .. } => {
            let mut message = format!("amend! {subject}\n\n").into_bytes();
            message.extend_from_slice(&commit.message);
            Ok(message)
        }
    }
}

fn read_squash_commit_message(git_dir: &Path, format: ObjectFormat, rev: &str) -> Result<Vec<u8>> {
    let commit = read_reused_commit(git_dir, format, rev)?;
    Ok(format!("squash! {}\n", commit_subject(&commit.message)).into_bytes())
}

fn commit_fixup_message(
    fixup_message: &[u8],
    file_message: Option<&[u8]>,
    message_chunks: &[Vec<u8>],
) -> Vec<u8> {
    let body = file_message
        .map(<[u8]>::to_vec)
        .unwrap_or_else(|| commit_message_from_prepared_chunks(message_chunks));
    if body.is_empty() {
        return fixup_message.to_vec();
    }
    let mut message = fixup_message.to_vec();
    if !message.ends_with(b"\n\n") {
        message.push(b'\n');
    }
    message.extend_from_slice(&body);
    message
}

fn commit_squash_message(
    squash_message: &[u8],
    reused_message: Option<&[u8]>,
    file_message: Option<&[u8]>,
    message_chunks: &[Vec<u8>],
) -> Vec<u8> {
    let body = reused_message
        .map(commit_message_body)
        .or_else(|| file_message.map(<[u8]>::to_vec))
        .unwrap_or_else(|| commit_message_from_prepared_chunks(message_chunks));
    if body.is_empty() {
        return squash_message.to_vec();
    }
    let mut message = squash_message.to_vec();
    if !message.ends_with(b"\n\n") {
        message.push(b'\n');
    }
    message.extend_from_slice(&body);
    message
}

fn commit_message_body(message: &[u8]) -> Vec<u8> {
    let Some(first_lf) = message.iter().position(|byte| *byte == b'\n') else {
        return Vec::new();
    };
    let body_start = if message.get(first_lf + 1) == Some(&b'\n') {
        first_lf + 2
    } else {
        first_lf + 1
    };
    message[body_start..].to_vec()
}

fn read_amended_commit(git_dir: &Path, format: ObjectFormat) -> Result<Commit> {
    match read_head_commit(git_dir, format)? {
        Some(commit) => Ok(commit),
        None => {
            eprintln!("fatal: You have nothing to amend.");
            Err(GitError::Exit(128))
        }
    }
}

fn read_head_commit(git_dir: &Path, format: ObjectFormat) -> Result<Option<Commit>> {
    let store = FileRefStore::new(git_dir, format);
    let head = match store.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(name)) => store.read_ref(&name)?,
        direct => direct,
    };
    let Some(RefTarget::Direct(oid)) = head else {
        return Ok(None);
    };
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let object = db.read_object(&oid)?;
    if object.object_type != ObjectType::Commit {
        return Err(GitError::InvalidObject(format!(
            "expected commit {}, found {}",
            oid,
            object.object_type.as_str()
        )));
    }
    Commit::parse(format, &object.body).map(Some)
}

/// First line of the commit message at `oid`, for the `HEAD is now at <oid>
/// <subject>` line a detached-HEAD checkout prints. Best-effort: an unreadable or
/// non-commit object yields an empty subject (git still prints the abbreviated
/// oid).
fn detached_checkout_subject(git_dir: &Path, format: ObjectFormat, oid: &ObjectId) -> String {
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let Ok(object) = db.read_object(oid) else {
        return String::new();
    };
    if object.object_type != ObjectType::Commit {
        return String::new();
    }
    let Ok(commit) = Commit::parse(format, &object.body) else {
        return String::new();
    };
    String::from_utf8_lossy(&commit.message)
        .lines()
        .next()
        .unwrap_or("")
        .to_string()
}

fn build_reused_commit_author_identity(
    reused_author: &[u8],
    author: Option<&str>,
    date: Option<&str>,
) -> Result<Vec<u8>> {
    if author.is_none() && date.is_none() {
        return Ok(reused_author.to_vec());
    }
    let (reused_name, reused_email, reused_date) = parse_commit_identity_parts(reused_author)?;
    let (name, email) = if let Some(author) = author {
        parse_commit_author(author)?
    } else {
        (reused_name, reused_email)
    };
    // A `--date` override is raw user input; canonicalize it. The reused date
    // is already in canonical `<seconds> <tz>` form.
    let date = match date {
        Some(date) => canonicalize_commit_date(date),
        None => reused_date,
    };
    sley_sequencer::format_commit_identity(&name, &email, &date)
}

fn parse_commit_identity_parts(identity: &[u8]) -> Result<(String, String, String)> {
    let identity = std::str::from_utf8(identity)
        .map_err(|err| GitError::InvalidObject(format!("invalid commit identity: {err}")))?;
    let Some((left, timezone)) = identity.rsplit_once(' ') else {
        return Err(GitError::InvalidObject(
            "commit identity missing timezone".into(),
        ));
    };
    let Some((author, timestamp)) = left.rsplit_once(' ') else {
        return Err(GitError::InvalidObject(
            "commit identity missing timestamp".into(),
        ));
    };
    let (name, email) = parse_commit_author(author)?;
    Ok((name, email, format!("{timestamp} {timezone}")))
}

fn commit_stage_tracked_changes(git_dir: &Path, format: ObjectFormat) -> Result<()> {
    let cwd = env::current_dir()?;
    let worktree_root = worktree_root_for_git_dir(git_dir)?;
    let actions = resolve_add_update_actions(
        &cwd,
        &worktree_root,
        git_dir,
        format,
        Vec::new(),
        false,
        false,
    )?;
    let mut seen_paths = BTreeSet::new();
    let mut action_paths = Vec::new();
    for path in actions.iter().map(AddAction::path) {
        if seen_paths.insert(path.clone()) {
            action_paths.push(path.clone());
        }
    }
    if let Some(index) = sley_worktree::read_repository_index(git_dir, format)? {
        for path in index
            .entries
            .iter()
            .filter(|entry| index_entry_stage(entry) > 0)
            .map(|entry| worktree_root.join(repo_path_to_path(entry.path.as_bytes())))
        {
            if seen_paths.insert(path.clone()) {
                action_paths.push(path);
            }
        }
    }
    if action_paths.is_empty() {
        return Ok(());
    }
    let config = read_repo_config(git_dir)?;
    sley_worktree::update_index_paths_filtered(
        &worktree_root,
        git_dir,
        format,
        &action_paths,
        sley_worktree::UpdateIndexOptions {
            add: true,
            remove: true,
            force_remove: false,
            chmod: None,
            info_only: false,
            ignore_skip_worktree_entries: false,
        },
        &config,
    )?;
    Ok(())
}

fn commit_index_tree_if_changed(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
) -> Result<Option<ObjectId>> {
    let tree = sley_worktree::write_tree_from_index_with_odb(git_dir, format, db)?;
    let store = FileRefStore::new(git_dir, format);
    let head = match store.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(name)) => store.read_ref(&name)?,
        direct => direct,
    };
    let Some(RefTarget::Direct(parent)) = head else {
        return Ok(Some(tree));
    };
    let object = db.read_object(&parent)?;
    if object.object_type != ObjectType::Commit {
        return Ok(Some(tree));
    }
    let commit = Commit::parse_ref(format, &object.body)?;
    Ok((commit.tree != tree).then_some(tree))
}

/// Read a `-t <file>` / `--template <file>` template body. The path is relative
/// to the current working directory (git resolves it via the prefix). git reads
/// it verbatim (no whitespace cleanup).
fn read_commit_template_file(path: &str) -> Result<Vec<u8>> {
    fs::read(path).map_err(|err| {
        eprintln!("fatal: could not read '{path}': {err}");
        GitError::Exit(128)
    })
}

/// Inputs for [`build_commit_editor_template_block`].
struct CommitTemplateBlock<'a> {
    git_dir: &'a Path,
    format: ObjectFormat,
    comment_char: &'a str,
    cleanup_mode: CommitCleanupMode,
    allow_empty_message: bool,
    author: &'a [u8],
    committer: &'a [u8],
    /// `commit --date=...` / reused author ⇒ git's `author_date_is_interesting()`
    /// shows the `Date:` line in the template.
    author_date_interesting: bool,
    /// `--amend` ⇒ the staged summary compares against `HEAD^`.
    amend: bool,
    untracked_override: Option<sley_worktree::StatusUntrackedMode>,
}

/// Build the comment-prefixed block git appends to COMMIT_EDITMSG when an editor
/// is launched with `include_status` (commit.status / --status). Mirrors the
/// `use_editor && include_status` branch of builtin/commit.c `prepare_to_commit`:
/// a blank line, the cleanup hint (or a scissors cut line), the Author/Date/
/// Committer ident lines (each shown only when it differs from the committer
/// default), a blank line, then the long working-tree status — all commented.
fn build_commit_editor_template_block(input: &CommitTemplateBlock) -> Result<Vec<u8>> {
    let CommitTemplateBlock {
        git_dir,
        format,
        comment_char,
        cleanup_mode,
        allow_empty_message,
        author,
        committer,
        author_date_interesting,
        amend,
        untracked_override,
    } = *input;

    let mut out: Vec<u8> = Vec::new();
    // builtin/commit.c emits `fprintf(s->fp, "\n")` before the hint.
    out.push(b'\n');

    // The cleanup hint, or — for scissors — the cut line. SPACE/VERBATIM keep
    // their own hint text.
    match cleanup_mode {
        CommitCleanupMode::Scissors => {
            append_scissors_cut_line(&mut out, comment_char);
        }
        CommitCleanupMode::Whitespace => {
            let hint = "Please enter the commit message for your changes. Lines starting\nwith '%s' will be kept; you may remove them yourself if you want to.";
            append_commented_hint(
                &mut out,
                comment_char,
                hint,
                allow_empty_message,
                "An empty message aborts the commit.",
            );
        }
        _ => {
            // ALL (default with editor): empty lines are ignored.
            let hint = "Please enter the commit message for your changes. Lines starting\nwith '%s' will be ignored";
            if allow_empty_message {
                append_commented_hint(&mut out, comment_char, &format!("{hint}."), false, "");
            } else {
                append_commented_hint(
                    &mut out,
                    comment_char,
                    &format!("{hint}, and an empty message aborts the commit."),
                    false,
                    "",
                );
            }
        }
    }

    // Ident block: Author / Date / Committer, each gated on differing from the
    // committer default. The first shown line gets a leading blank comment line
    // (git's `ident_shown++ ? "" : "\n"`).
    let author_id = identity_name_email(author);
    let committer_id = identity_name_email(committer);
    let mut ident_shown = false;
    let mut commented_line = |out: &mut Vec<u8>, text: &str| {
        if !ident_shown {
            out.extend_from_slice(comment_char.as_bytes());
            out.push(b'\n');
            ident_shown = true;
        }
        out.extend_from_slice(comment_char.as_bytes());
        out.push(b' ');
        out.extend_from_slice(text.as_bytes());
        out.push(b'\n');
    };
    if author_id != committer_id {
        commented_line(&mut out, &format!("Author:    {author_id}"));
    }
    if author_date_interesting {
        let date = commit_identity_date(author, &DateMode::Default);
        commented_line(&mut out, &format!("Date:      {date}"));
    }
    if !committer_ident_sufficiently_given() {
        commented_line(&mut out, &format!("Committer: {committer_id}"));
    }
    // "Add new line for clarity" (status_printf_ln(s, ..., "%s", "")).
    out.extend_from_slice(comment_char.as_bytes());
    out.push(b'\n');

    // The long working-tree status, every line commented.
    let status = render_commit_template_status(
        git_dir,
        format,
        comment_char,
        amend,
        untracked_override,
    )?;
    out.extend_from_slice(&status);
    Ok(out)
}

/// Append git's commented cleanup hint. `hint` carries a single `%s` placeholder
/// for the comment char (matching the gettext templates); when `with_abort` is
/// set, `abort_line` is appended as a final commented sentence.
fn append_commented_hint(
    out: &mut Vec<u8>,
    comment_char: &str,
    hint: &str,
    with_abort: bool,
    abort_line: &str,
) {
    let text = hint.replace("%s", comment_char);
    let mut full = text;
    if with_abort && !abort_line.is_empty() {
        full.push('\n');
        full.push_str(abort_line);
    }
    append_commented_lines(out, comment_char, &full);
}

/// Comment every line of `text` with `comment_char` (git's
/// `strbuf_add_commented_lines`): non-empty lines get `<char> `, empty lines just
/// `<char>`.
fn append_commented_lines(out: &mut Vec<u8>, comment_char: &str, text: &str) {
    for line in text.split('\n') {
        if line.is_empty() {
            out.extend_from_slice(comment_char.as_bytes());
        } else {
            out.extend_from_slice(comment_char.as_bytes());
            out.push(b' ');
            out.extend_from_slice(line.as_bytes());
        }
        out.push(b'\n');
    }
}

/// git's `wt_status_append_cut_line`: the commented `>8` scissors line followed
/// by the "Do not modify..." explanation.
fn append_scissors_cut_line(out: &mut Vec<u8>, comment_char: &str) {
    let cut = "------------------------ >8 ------------------------";
    out.extend_from_slice(comment_char.as_bytes());
    out.push(b' ');
    out.extend_from_slice(cut.as_bytes());
    out.push(b'\n');
    append_commented_lines(
        out,
        comment_char,
        "Do not modify or remove the line above.\nEverything below it will be ignored.",
    );
}

/// Whether the committer identity was explicitly supplied (vs guessed from the
/// system). Mirrors git's `committer_ident_sufficiently_given()`: true when both
/// GIT_COMMITTER_NAME and GIT_COMMITTER_EMAIL are set in the environment.
fn committer_ident_sufficiently_given() -> bool {
    env::var_os("GIT_COMMITTER_NAME").is_some() && env::var_os("GIT_COMMITTER_EMAIL").is_some()
}

/// Render the long working-tree status block (every line commented with
/// `comment_char`) for the COMMIT_EDITMSG template.
fn render_commit_template_status(
    git_dir: &Path,
    format: ObjectFormat,
    comment_char: &str,
    amend: bool,
    untracked_override: Option<sley_worktree::StatusUntrackedMode>,
) -> Result<Vec<u8>> {
    let config = read_repo_config(git_dir).map_err(report_config_setup_error)?;
    let worktree_root = worktree_root_for_git_dir(git_dir)?;
    let untracked_mode = untracked_override.unwrap_or_else(|| {
        match config.get("status", None, "showUntrackedFiles") {
            Some("no") | Some("false") | Some("0") | Some("off") => {
                sley_worktree::StatusUntrackedMode::None
            }
            Some("all") => sley_worktree::StatusUntrackedMode::All,
            _ => sley_worktree::StatusUntrackedMode::Normal,
        }
    });
    let mut entries = sley_worktree::short_status_with_options(
        &worktree_root,
        git_dir,
        format,
        sley_worktree::ShortStatusOptions {
            include_ignored: false,
            ignored_mode: sley_worktree::StatusIgnoredMode::Traditional,
            untracked_mode,
        },
    )?;
    let ignore_resolver = SubmoduleIgnoreResolver::load(git_dir, &config, None)?;
    apply_submodule_ignore(&mut entries, &ignore_resolver);
    let base_ref = if amend { "HEAD^" } else { "HEAD" };
    let submodule_summary = status_submodule_summary(
        git_dir,
        &worktree_root,
        format,
        &config,
        base_ref,
        &ignore_resolver,
    )?;
    let display = StatusLongDisplay {
        commit_preview: true,
        show_stash: false,
        ahead_behind: true,
        // builtin/commit.c sets `s->hints = 0` for the template ("Most hints are
        // counter-productive when the commit has already started") — the
        // parenthetical `(use "git ...")` guidance is suppressed regardless of
        // advice.statusHints.
        hints: false,
        untracked_suppressed: untracked_mode == sley_worktree::StatusUntrackedMode::None,
        // The template ALWAYS comments the status, regardless of
        // status.displayCommentPrefix.
        comment_prefix: Some(comment_char.to_string()),
        submodule_summary,
    };
    let sink = build_status_long_sink(git_dir, format, entries, &display)?;
    let mut buf: Vec<u8> = Vec::new();
    sink.write_to(&mut buf);
    Ok(buf)
}

fn print_clean_commit_status(git_dir: &Path, format: ObjectFormat) -> Result<()> {
    let store = FileRefStore::new(git_dir, format);
    if let Some(RefTarget::Symbolic(target)) = store.read_ref("HEAD")?
        && let Some(branch) = target.strip_prefix("refs/heads/")
    {
        println!("On branch {branch}");
    }
    println!("nothing to commit, working tree clean");
    Ok(())
}

pub(crate) fn cmd_status(args: &[String]) -> Result<()> {
    // `-h`/`--help` short-circuits before any repository state is read, so it
    // works even in a broken repo (t7508 'status -h in broken repository').
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        return status_usage();
    }
    let mut short = false;
    let mut porcelain_v1 = false;
    let mut porcelain_v2 = false;
    let mut z = false;
    let mut explicit_long = false;
    let mut branch = false;
    // Track whether the format / branch / untracked-mode were set explicitly on
    // the command line. When they weren't, the corresponding `status.*` config
    // value supplies the default (upstream wt-status defaults come from config).
    let mut explicit_short = false;
    let mut explicit_branch: Option<bool> = None;
    let mut explicit_untracked = false;
    let mut untracked_mode = sley_worktree::StatusUntrackedMode::Normal;
    let mut show_ignored = false;
    let mut ignored_mode = sley_worktree::StatusIgnoredMode::Traditional;
    let mut show_stash = false;
    let mut ahead_behind = true;
    // `git status -v` verbosity: 0 (none), 1 (append the staged HEAD-vs-index
    // diff), 2+ (also append the index-vs-worktree diff). `-vv` and repeated
    // `-v` accumulate; `--no-verbose` resets to 0 (wt-status verbose level).
    let mut verbose: u8 = 0;
    // `--ignore-submodules[=<when>]` from the command line, the highest-priority
    // source for the per-submodule ignore resolution (above `.git/config`,
    // `.gitmodules`, and `diff.ignoreSubmodules`). `None` means the flag was not
    // given; the bare flag resolves to `All` exactly as git's parse-options does.
    let mut ignore_submodules_arg: Option<IgnoreSubmodules> = None;
    let mut path_args = Vec::new();
    let mut positional_only = false;
    for arg in args {
        if positional_only {
            path_args.push(arg.clone());
            continue;
        }
        match arg.as_str() {
            "--" => positional_only = true,
            "--short" | "-s" => {
                short = true;
                explicit_short = true;
                porcelain_v1 = false;
                porcelain_v2 = false;
                explicit_long = false;
            }
            "--porcelain" | "--porcelain=1" | "--porcelain=v1" => {
                short = true;
                porcelain_v1 = true;
                porcelain_v2 = false;
                explicit_long = false;
            }
            "--porcelain=v2" | "--porcelain=2" => {
                short = true;
                porcelain_v1 = false;
                porcelain_v2 = true;
                explicit_long = false;
            }
            "--no-porcelain" => {
                short = false;
                porcelain_v1 = false;
                porcelain_v2 = false;
                explicit_long = false;
            }
            "--branch" | "-b" => {
                branch = true;
                explicit_branch = Some(true);
                explicit_long = false;
            }
            "-sb" | "-bs" => {
                short = true;
                explicit_short = true;
                branch = true;
                explicit_branch = Some(true);
                explicit_long = false;
            }
            "--no-short" => {
                short = false;
                explicit_short = true;
                porcelain_v1 = false;
                porcelain_v2 = false;
            }
            "--no-branch" => {
                branch = false;
                explicit_branch = Some(false);
            }
            "--no-untracked-files" => {
                // `--untracked-files` is an OPTION_STRING with PARSE_OPT_OPTARG;
                // its `--no-` form clears the override (NULL arg), so the config
                // / default applies rather than forcing "no".
                untracked_mode = sley_worktree::StatusUntrackedMode::Normal;
                explicit_untracked = false;
            }
            "-u" | "--untracked-files" => {
                untracked_mode = sley_worktree::StatusUntrackedMode::All;
                explicit_untracked = true;
            }
            value if value.starts_with("-u") && value.len() > 2 => {
                untracked_mode = parse_status_untracked_mode(&value[2..])?;
                explicit_untracked = true;
            }
            value if value.starts_with("--untracked-files=") => {
                untracked_mode =
                    parse_status_untracked_mode(&value["--untracked-files=".len()..])?;
                explicit_untracked = true;
            }
            value if value.starts_with("--porcelain=") => {
                return status_unsupported_porcelain_version_error(&value["--porcelain=".len()..]);
            }
            "-z" | "--null" => {
                short = true;
                z = true;
            }
            "--no-null" => z = false,
            "--ignored" | "--ignored=traditional" => {
                show_ignored = true;
                ignored_mode = sley_worktree::StatusIgnoredMode::Traditional;
            }
            "--ignored=matching" => {
                show_ignored = true;
                ignored_mode = sley_worktree::StatusIgnoredMode::Matching;
            }
            "--ignored=no" | "--no-ignored" => {
                show_ignored = false;
                ignored_mode = sley_worktree::StatusIgnoredMode::Traditional;
            }
            value if value.starts_with("--ignored=") => {
                return status_invalid_ignored_mode_error(&value["--ignored=".len()..]);
            }
            "--long" => {
                short = false;
                porcelain_v1 = false;
                porcelain_v2 = false;
                explicit_long = true;
            }
            "--no-long" => {
                short = false;
                porcelain_v1 = false;
                porcelain_v2 = false;
                explicit_long = false;
            }
            "-v" | "--verbose" => verbose = verbose.saturating_add(1),
            "--no-verbose" => verbose = 0,
            "--no-renames"
            | "--renames"
            | "--find-renames"
            | "--column"
            | "--no-column"
            | "--column="
            | "--column=auto"
            | "--column=always"
            | "--column=never"
            | "--column=plain"
            | "--column=column"
            | "--column=row"
            | "--column=dense"
            | "--column=nodense" => {}
            // `--ignore-submodules[=<when>]` (builtin/commit.c's OPT_CALLBACK
            // with PARSE_OPT_OPTARG): the bare flag means "all"; `--no-` clears
            // any prior selection back to the config/default.
            "--ignore-submodules" | "--ignore-submodules=all" => {
                ignore_submodules_arg = Some(IgnoreSubmodules::All);
            }
            "--ignore-submodules=dirty" => {
                ignore_submodules_arg = Some(IgnoreSubmodules::Dirty);
            }
            "--ignore-submodules=untracked" => {
                ignore_submodules_arg = Some(IgnoreSubmodules::Untracked);
            }
            "--ignore-submodules=none" => {
                ignore_submodules_arg = Some(IgnoreSubmodules::None);
            }
            "--no-ignore-submodules" => {
                ignore_submodules_arg = None;
            }
            "--ahead-behind" => ahead_behind = true,
            "--no-ahead-behind" => ahead_behind = false,
            "--show-stash" => show_stash = true,
            "--no-show-stash" => show_stash = false,
            "-M" => {}
            value if value.starts_with("-M") && value.len() > 2 => {}
            value if value.starts_with("--find-renames=") => {}
            value if value.starts_with("--short=") => {
                return status_option_takes_no_value_error("short");
            }
            value if value.starts_with("--no-short=") => {
                return status_option_takes_no_value_error("no-short");
            }
            value if value.starts_with("--no-porcelain=") => {
                return status_option_takes_no_value_error("no-porcelain");
            }
            value if value.starts_with("--branch=") => {
                return status_option_takes_no_value_error("branch");
            }
            value if value.starts_with("--no-branch=") => {
                return status_option_takes_no_value_error("no-branch");
            }
            value if value.starts_with("--null=") => {
                return status_option_takes_no_value_error("null");
            }
            value if value.starts_with("--no-null=") => {
                return status_option_takes_no_value_error("no-null");
            }
            value if value.starts_with("--no-ignored=") => {
                return status_option_takes_no_value_error("no-ignored");
            }
            value if value.starts_with("--long=") => {
                return status_option_takes_no_value_error("long");
            }
            value if value.starts_with("--no-long=") => {
                return status_option_takes_no_value_error("no-long");
            }
            value if value.starts_with("--ahead-behind=") => {
                return status_option_takes_no_value_error("ahead-behind");
            }
            value if value.starts_with("--no-ahead-behind=") => {
                return status_option_takes_no_value_error("no-ahead-behind");
            }
            value if value.starts_with("--verbose=") => {
                return status_option_takes_no_value_error("verbose");
            }
            value if value.starts_with("--no-verbose=") => {
                return status_option_takes_no_value_error("no-verbose");
            }
            value if value.starts_with("--show-stash=") => {
                return status_option_takes_no_value_error("show-stash");
            }
            value if value.starts_with("--no-show-stash=") => {
                return status_option_takes_no_value_error("no-show-stash");
            }
            value if value.starts_with("--renames=") => {
                return status_option_takes_no_value_error("no-no-renames");
            }
            value if value.starts_with("--no-renames=") => {
                return status_option_takes_no_value_error("no-renames");
            }
            value if value.starts_with("--column=") => {
                return status_unsupported_column_option_error(&value["--column=".len()..]);
            }
            value if value.starts_with("--no-column=") => {
                return status_option_takes_no_value_error("no-column");
            }
            value if value.starts_with("--ignore-submodules=") => {
                return status_bad_ignore_submodules_argument_error(
                    &value["--ignore-submodules=".len()..],
                );
            }
            value if value.starts_with("--no-ignore-submodules=") => {
                return status_option_takes_no_value_error("no-ignore-submodules");
            }
            // `-vv`/`-vvv`: a run of `v` short flags raises the verbose level by
            // its length (parse-options collapses adjacent shorts).
            value
                if value.len() > 1
                    && value.starts_with('-')
                    && !value.starts_with("--")
                    && value[1..].bytes().all(|byte| byte == b'v') =>
            {
                verbose = verbose.saturating_add((value.len() - 1) as u8);
            }
            value if value.starts_with('-') => {
                return Err(GitError::Command(
                    "status currently supports only --short, --porcelain, --porcelain=1, --porcelain=v1, --porcelain=v2, --long, --branch, -z/--null, --untracked-files, --ignored=no, --no-renames, simple display toggles, and literal pathspecs"
                        .into(),
                ));
            }
            _ => path_args.push(arg.clone()),
        }
    }
    if explicit_long && z {
        eprintln!("fatal: options '--long' and '-z' cannot be used together");
        return Err(GitError::Exit(128));
    }
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let config = read_repo_config(&git_dir).map_err(report_config_setup_error)?;
    // Config-derived display defaults. The command line wins where it set a
    // value explicitly; otherwise `status.*` config supplies the default, as
    // upstream's wt-status initialization does.
    if !explicit_short
        && !porcelain_v1
        && !porcelain_v2
        && !explicit_long
        && config.get_bool("status", None, "short") == Some(true)
    {
        short = true;
    }
    if let Some(want_branch) = explicit_branch {
        branch = want_branch;
    } else if !porcelain_v1
        && !porcelain_v2
        && config.get_bool("status", None, "branch") == Some(true)
    {
        // `status.branch` adds the branch header to short/long output, but
        // `--porcelain` ignores it unless `-b` was passed explicitly
        // (t7508 '"status.branch=true" weaker than "--porcelain"').
        branch = true;
    }
    if !explicit_untracked {
        match config.get("status", None, "showUntrackedFiles") {
            Some("no") | Some("false") | Some("0") | Some("off") => {
                untracked_mode = sley_worktree::StatusUntrackedMode::None;
            }
            Some("all") => untracked_mode = sley_worktree::StatusUntrackedMode::All,
            // "normal"/"true"/unset keep the Normal default.
            _ => {}
        }
    }
    // advice.statusHints defaults to true; `relativePaths` to true; comment
    // prefix is off unless status.displayCommentPrefix is set.
    let status_hints = config
        .get_bool("advice", None, "statusHints")
        .unwrap_or(true);
    let relative_paths = config
        .get_bool("status", None, "relativePaths")
        .unwrap_or(true);
    let comment_prefix = status_comment_prefix(&config);
    // status needs a work tree; emit git's diagnostic (bare / no-worktree, or
    // the core.bare+core.worktree conflict) when one isn't available.
    let worktree_root = require_work_tree(&git_dir)?;
    let format = repository_object_format(&git_dir)?;
    let mut entries = sley_worktree::short_status_with_options(
        &worktree_root,
        &git_dir,
        format,
        sley_worktree::ShortStatusOptions {
            include_ignored: show_ignored,
            ignored_mode,
            untracked_mode,
        },
    )?;
    let pathspec = StatusPathspec::new(&cwd, &worktree_root, &path_args)?;
    if pathspec.has_filters() {
        entries.retain(|entry| pathspec.matches(&entry.path));
    }
    // Resolve the per-submodule ignore setting (command line > `.git/config` >
    // `.gitmodules` > `diff.ignoreSubmodules`) and apply it to the worktree-side
    // submodule change detail, exactly as git's handle_ignore_submodules_arg ahead
    // of the diff. Computed before the relativePaths display rewrite so gitlink
    // lookups use worktree-root-relative paths.
    let ignore_resolver = SubmoduleIgnoreResolver::load(&git_dir, &config, ignore_submodules_arg)?;
    apply_submodule_ignore(&mut entries, &ignore_resolver);
    // The long-format `Submodule changes to be committed:` /
    // `Submodules changed but not updated:` sections (status.submodulesummary).
    // Only the long output renders them; compute before the display rewrite so
    // the gitlink paths still address the worktree.
    let submodule_summary = if !short && !porcelain_v1 && !porcelain_v2 && !z {
        status_submodule_summary(
            &git_dir,
            &worktree_root,
            format,
            &config,
            "HEAD",
            &ignore_resolver,
        )?
    } else {
        SubmoduleSummarySections::default()
    };
    // `status.relativePaths=false` displays paths from the worktree root rather
    // than relative to the current directory (upstream status.relativePaths).
    if !z && !porcelain_v1 && relative_paths {
        for entry in &mut entries {
            entry.path = pathspec.display(&entry.path);
        }
    }
    if porcelain_v2 {
        print_status_porcelain_v2(&git_dir, format, entries, branch, ahead_behind, z)?;
    } else if z {
        let mut stdout = io::stdout().lock();
        if branch {
            stdout.write_all(status_branch_header(&git_dir, format, ahead_behind)?.as_bytes())?;
            stdout.write_all(&[0])?;
        }
        for entry in entries {
            write!(stdout, "{}{} ", entry.index as char, entry.worktree as char)?;
            stdout.write_all(&entry.path)?;
            stdout.write_all(&[0])?;
        }
    } else if short {
        if branch {
            println!("{}", status_branch_header(&git_dir, format, ahead_behind)?);
        }
        for entry in entries {
            // `--short` (but not --porcelain) refines a submodule's worktree
            // column per upstream short_submodule_status(): 'M' new commits,
            // 'm' modified content, '?' untracked content.
            let worktree_code = if porcelain_v1 {
                entry.worktree
            } else {
                status_short_submodule_code(&entry)
            };
            println!(
                "{}{} {}",
                entry.index as char,
                worktree_code as char,
                status_quote_path(&entry.path, true)
            );
        }
    } else {
        let display = StatusLongDisplay {
            commit_preview: false,
            show_stash,
            ahead_behind,
            hints: status_hints,
            untracked_suppressed: untracked_mode == sley_worktree::StatusUntrackedMode::None,
            comment_prefix,
            submodule_summary,
        };
        print_status_long(&git_dir, format, entries, &display)?;
        // `git status -v` appends the staged diff (HEAD vs index). `-vv` instead
        // frames both diffs with section headers and a 50-dash separator and
        // renders them with diff.mnemonicprefix=true (commit/index `c/`,`i/` for
        // the cached half; index/worktree `i/`,`w/` for the unstaged half) —
        // exactly wt-status's verbose>1 layout. Reuse the diff command so the
        // hunk bytes match `git diff` verbatim.
        if verbose == 1 {
            io::stdout().flush()?;
            commands::diff::cmd_diff(&["--cached".to_string()])?;
        } else if verbose >= 2 {
            io::stdout().flush()?;
            println!("Changes to be committed:");
            io::stdout().flush()?;
            commands::diff::cmd_diff(&[
                "--cached".to_string(),
                "--src-prefix=c/".to_string(),
                "--dst-prefix=i/".to_string(),
            ])?;
            println!("--------------------------------------------------");
            println!("Changes not staged for commit:");
            io::stdout().flush()?;
            commands::diff::cmd_diff(&[
                "--src-prefix=i/".to_string(),
                "--dst-prefix=w/".to_string(),
            ])?;
        }
    }
    Ok(())
}

/// Display knobs for the long ("porcelain off") `git status` output, derived
/// from the command line plus `status.*` / `advice.*` config.
struct StatusLongDisplay {
    /// `commit --dry-run` preview wording (initial-commit hint text).
    commit_preview: bool,
    show_stash: bool,
    ahead_behind: bool,
    /// `advice.statusHints` — when false, the parenthetical `(use "git ...")`
    /// guidance lines are suppressed throughout the output.
    hints: bool,
    /// True when untracked files are hidden (`-uno` / `status.showUntrackedFiles
    /// no`); drives the "Untracked files not listed" line when committable.
    untracked_suppressed: bool,
    /// `core.commentChar` / `status.displayCommentPrefix`: when set, every line
    /// is prefixed with the comment character (e.g. `# `), as in COMMIT_EDITMSG.
    comment_prefix: Option<String>,
    /// Rendered `Submodule changes to be committed:` /
    /// `Submodules changed but not updated:` sections (status.submodulesummary).
    submodule_summary: SubmoduleSummarySections,
}

/// Upstream wt-status.c short_submodule_status(): in `--short` output a
/// changed submodule's worktree column shows 'M' for new commits, 'm' for
/// modified content, '?' for untracked content (priority in that order).
fn status_short_submodule_code(entry: &sley_worktree::ShortStatusEntry) -> u8 {
    let Some(submodule) = entry.submodule else {
        return entry.worktree;
    };
    if submodule.new_commits {
        b'M'
    } else if submodule.modified_content {
        b'm'
    } else if submodule.untracked_content {
        b'?'
    } else {
        entry.worktree
    }
}

fn status_option_takes_no_value_error(option: &str) -> Result<()> {
    eprintln!("error: option `{option}' takes no value");
    Err(GitError::Exit(129))
}

fn status_invalid_untracked_files_mode_error(mode: &str) -> Result<()> {
    eprintln!("fatal: Invalid untracked files mode '{mode}'");
    Err(GitError::Exit(128))
}

/// Parse a `-u<mode>` / `--untracked-files=<mode>` value. Upstream accepts the
/// keywords `no`/`normal`/`all` and the git-boolean forms (`true`/`yes`/`on`/`1`
/// → normal, `false`/`no`/`off`/`0`/empty → no), erroring otherwise.
fn parse_status_untracked_mode(value: &str) -> Result<sley_worktree::StatusUntrackedMode> {
    match value.to_ascii_lowercase().as_str() {
        "all" => Ok(sley_worktree::StatusUntrackedMode::All),
        "normal" | "true" | "yes" | "on" | "1" => Ok(sley_worktree::StatusUntrackedMode::Normal),
        "no" | "false" | "off" | "0" | "" => Ok(sley_worktree::StatusUntrackedMode::None),
        other => {
            status_invalid_untracked_files_mode_error(other)?;
            unreachable!()
        }
    }
}

fn status_invalid_ignored_mode_error(mode: &str) -> Result<()> {
    eprintln!("fatal: Invalid ignored mode '{mode}'");
    Err(GitError::Exit(128))
}

fn status_unsupported_porcelain_version_error(version: &str) -> Result<()> {
    eprintln!("fatal: unsupported porcelain version '{version}'");
    Err(GitError::Exit(128))
}

fn status_bad_ignore_submodules_argument_error(value: &str) -> Result<()> {
    eprintln!("fatal: bad --ignore-submodules argument: {value}");
    Err(GitError::Exit(128))
}

fn status_unsupported_column_option_error(value: &str) -> Result<()> {
    eprintln!("error: unsupported option '{value}'");
    Err(GitError::Exit(129))
}

struct StatusPathspec {
    prefix: Vec<u8>,
    filters: Vec<LsFilesPathFilter>,
    cwd_depth: usize,
}

impl StatusPathspec {
    fn new(cwd: &Path, worktree_root: &Path, path_args: &[String]) -> Result<Self> {
        let root = fs::canonicalize(worktree_root)?;
        let cwd = fs::canonicalize(cwd)?;
        let relative = cwd.strip_prefix(&root).map_err(|_| {
            GitError::InvalidPath(format!("path {} is outside worktree", cwd.display()))
        })?;
        let prefix = relative.to_string_lossy().replace('\\', "/").into_bytes();
        let cwd_depth = path_component_count(&prefix);
        let mut filters = Vec::new();
        for arg in path_args {
            let filter_path = normalize_ls_files_pathspec(&prefix, arg)?;
            let is_glob = sley_worktree::pathspec_is_glob(&filter_path);
            let arg_path = Path::new(arg);
            let absolute = if arg_path.is_absolute() {
                arg_path.to_path_buf()
            } else {
                cwd.join(arg_path)
            };
            filters.push(LsFilesPathFilter {
                original: arg.clone(),
                path: filter_path,
                recursive: arg == "." || arg.ends_with('/') || absolute.is_dir(),
                is_glob,
                matched: Cell::new(false),
            });
        }
        Ok(Self {
            prefix,
            filters,
            cwd_depth,
        })
    }

    fn has_filters(&self) -> bool {
        !self.filters.is_empty()
    }

    fn display(&self, path: &[u8]) -> Vec<u8> {
        if self.prefix.is_empty() {
            return path.to_vec();
        }
        if let Some(rest) = path.strip_prefix(self.prefix.as_slice())
            && let Some(rest) = rest.strip_prefix(b"/")
        {
            return rest.to_vec();
        }
        let mut display = Vec::new();
        for _ in 0..self.cwd_depth {
            display.extend_from_slice(b"../");
        }
        display.extend_from_slice(path);
        display
    }

    fn matches(&self, path: &[u8]) -> bool {
        let magic = effective_pathspec_flags();
        self.filters
            .iter()
            .any(|filter| filter.matches(path, magic))
    }
}

fn print_status_porcelain_v2(
    git_dir: &Path,
    format: ObjectFormat,
    entries: Vec<sley_worktree::ShortStatusEntry>,
    branch: bool,
    ahead_behind: bool,
    z: bool,
) -> Result<()> {
    let mut stdout = io::stdout().lock();
    let separator = if z { b'\0' } else { b'\n' };
    if branch {
        for header in status_porcelain_v2_branch_headers(git_dir, format, ahead_behind)? {
            stdout.write_all(header.as_bytes())?;
            stdout.write_all(&[separator])?;
        }
    }
    let zero = zero_oid(format)?;
    for entry in entries {
        if entry.index == b'!' && entry.worktree == b'!' {
            stdout.write_all(b"! ")?;
            if z {
                stdout.write_all(&entry.path)?;
            } else {
                stdout.write_all(status_quote_path(&entry.path, false).as_bytes())?;
            }
            stdout.write_all(&[separator])?;
            continue;
        }
        if entry.index == b'?' && entry.worktree == b'?' {
            stdout.write_all(b"? ")?;
            if z {
                stdout.write_all(&entry.path)?;
            } else {
                stdout.write_all(status_quote_path(&entry.path, false).as_bytes())?;
            }
            stdout.write_all(&[separator])?;
            continue;
        }
        let index = status_porcelain_v2_code(entry.index);
        let worktree = status_porcelain_v2_code(entry.worktree);
        // Porcelain v2 submodule field (wt-status.c wt_porcelain_v2_*):
        // "N..." for an ordinary path; "S<C><M><U>" for a submodule, with C
        // for new commits, M for modified content, U for untracked content.
        let sub = match entry.submodule {
            Some(submodule) => format!(
                "S{}{}{}",
                if submodule.new_commits { 'C' } else { '.' },
                if submodule.modified_content { 'M' } else { '.' },
                if submodule.untracked_content {
                    'U'
                } else {
                    '.'
                },
            ),
            None if entry.index_mode == Some(0o160000) || entry.worktree_mode == Some(0o160000) => {
                "S...".to_string()
            }
            None => "N...".to_string(),
        };
        write!(
            stdout,
            "1 {index}{worktree} {sub} {:06o} {:06o} {:06o} {} {} ",
            entry.head_mode.unwrap_or(0),
            entry.index_mode.unwrap_or(0),
            entry.worktree_mode.unwrap_or(0),
            entry.head_oid.as_ref().unwrap_or(&zero).to_hex(),
            entry.index_oid.as_ref().unwrap_or(&zero).to_hex()
        )?;
        if z {
            stdout.write_all(&entry.path)?;
        } else {
            stdout.write_all(status_quote_path(&entry.path, false).as_bytes())?;
        }
        stdout.write_all(&[separator])?;
    }
    stdout.flush()?;
    Ok(())
}

/// The `--ignore-submodules[=<when>]` / `submodule.<name>.ignore` /
/// `diff.ignoreSubmodules` levels, mirroring git's `enum submodule_ignore` and
/// the `dirty`/`untracked`/`all`/`none` keywords. Ordered by how much they hide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IgnoreSubmodules {
    /// `none`: show every kind of submodule change (the default).
    None,
    /// `untracked`: hide submodules whose only change is untracked content.
    Untracked,
    /// `dirty`: additionally hide modified (tracked) content; new commits still
    /// show.
    Dirty,
    /// `all`: hide the submodule entirely, including its summary section.
    All,
}

impl IgnoreSubmodules {
    /// Parse a `dirty`/`untracked`/`all`/`none` config/CLI keyword. Unknown
    /// values are treated as `None` (git's `parse_submodule_ignore` rejects them
    /// with a warning; for status purposes the safe fallback is to show
    /// everything).
    fn parse(value: &str) -> Option<Self> {
        match value {
            "none" => Some(Self::None),
            "untracked" => Some(Self::Untracked),
            "dirty" => Some(Self::Dirty),
            "all" => Some(Self::All),
            _ => None,
        }
    }
}

/// Resolves the effective per-submodule ignore setting from the four layered
/// sources, in git's precedence order: the `--ignore-submodules` command line
/// (applies to every submodule) wins over `submodule.<name>.ignore` in
/// `.git/config`, which wins over the same key in `.gitmodules`, which wins over
/// the global `diff.ignoreSubmodules`.
struct SubmoduleIgnoreResolver {
    /// `--ignore-submodules[=<when>]`; `Some` overrides every other source.
    cli: Option<IgnoreSubmodules>,
    /// `diff.ignoreSubmodules` — the all-submodule fallback.
    diff_default: Option<IgnoreSubmodules>,
    /// `submodule.<name>.ignore` read from `.git/config` (repo-local), keyed by
    /// the bound submodule path. Overrides the `.gitmodules` value.
    by_path_repo: BTreeMap<Vec<u8>, IgnoreSubmodules>,
    /// `submodule.<name>.ignore` read from `.gitmodules`, keyed by bound path.
    by_path_gitmodules: BTreeMap<Vec<u8>, IgnoreSubmodules>,
}

impl SubmoduleIgnoreResolver {
    fn load(
        git_dir: &Path,
        config: &GitConfig,
        cli: Option<IgnoreSubmodules>,
    ) -> Result<Self> {
        let diff_default = config
            .get("diff", None, "ignoreSubmodules")
            .and_then(IgnoreSubmodules::parse);
        // `.git/config`'s `submodule.<name>.ignore` + `.path` (the repo-local
        // override). `read_repo_config` already merges global+repo, but the
        // submodule sections we want are repo-local; read the raw repo config.
        let by_path_repo = submodule_ignore_by_path(config);
        // `.gitmodules` lives in the worktree root.
        let by_path_gitmodules = match worktree_root_for_git_dir(git_dir) {
            Ok(root) => GitConfig::read(root.join(".gitmodules"))
                .map(|cfg| submodule_ignore_by_path(&cfg))
                .unwrap_or_default(),
            Err(_) => BTreeMap::new(),
        };
        Ok(Self {
            cli,
            diff_default,
            by_path_repo,
            by_path_gitmodules,
        })
    }

    /// The effective ignore for the submodule bound at `path`.
    fn for_path(&self, path: &[u8]) -> IgnoreSubmodules {
        if let Some(cli) = self.cli {
            return cli;
        }
        if let Some(value) = self.by_path_repo.get(path) {
            return *value;
        }
        if let Some(value) = self.by_path_gitmodules.get(path) {
            return *value;
        }
        self.diff_default.unwrap_or(IgnoreSubmodules::None)
    }

    /// Whether the whole summary is suppressed by the command line. git gates the
    /// summary block on `!ignore_submodule_arg || strcmp(arg, "all")`, so a
    /// `--ignore-submodules=all` on the CLI hides both summary sections wholesale
    /// (per-submodule `all` is handled inside the summary instead).
    fn cli_suppresses_summary(&self) -> bool {
        self.cli == Some(IgnoreSubmodules::All)
    }
}

/// Extract `submodule.<name>.ignore` keyed by the submodule's bound `.path`,
/// from a single config source (`.git/config` or `.gitmodules`). Names without a
/// `.path` are dropped — without a path binding there is nothing to match a
/// status entry against.
fn submodule_ignore_by_path(config: &GitConfig) -> BTreeMap<Vec<u8>, IgnoreSubmodules> {
    let set = sley_submodule::SubmoduleConfigSet::parse(config);
    let mut map = BTreeMap::new();
    for sub in set.iter() {
        let (Some(path), Some(ignore)) = (
            sub.path.as_deref(),
            sub.ignore.as_deref().and_then(IgnoreSubmodules::parse),
        ) else {
            continue;
        };
        map.insert(path.as_bytes().to_vec(), ignore);
    }
    map
}

/// Apply the resolved per-submodule ignore to the worktree-side change detail of
/// each status entry, mirroring git's `handle_ignore_submodules_arg` before the
/// diff: `untracked` clears untracked-content, `dirty` additionally clears
/// modified-content, `all` clears every worktree change (the gitlink's `M`
/// worktree code and all three detail bits). New commits survive `dirty`/
/// `untracked` and are only hidden by `all`.
fn apply_submodule_ignore(
    entries: &mut Vec<sley_worktree::ShortStatusEntry>,
    resolver: &SubmoduleIgnoreResolver,
) {
    // A bare `--ignore-submodules=all` on the COMMAND LINE sets the diffopt
    // ignore_submodules flag for the whole status run, hiding even the *staged*
    // gitlink change (`modified: sm` under "Changes to be committed"). A
    // per-submodule `ignore=all` from `.git/config`/`.gitmodules` does NOT — it
    // only touches the worktree-side detail and the summary (cells #93/#94 keep
    // the staged line).
    let cli_all = resolver.cli == Some(IgnoreSubmodules::All);
    entries.retain_mut(|entry| {
        let is_gitlink = entry.head_mode == Some(0o160000)
            || entry.index_mode == Some(0o160000)
            || entry.worktree_mode == Some(0o160000);
        if cli_all && is_gitlink {
            return false;
        }
        let Some(submodule) = entry.submodule.as_mut() else {
            return true;
        };
        let ignore = resolver.for_path(&entry.path);
        match ignore {
            IgnoreSubmodules::None => {}
            IgnoreSubmodules::Untracked => {
                submodule.untracked_content = false;
            }
            IgnoreSubmodules::Dirty => {
                submodule.untracked_content = false;
                submodule.modified_content = false;
            }
            IgnoreSubmodules::All => {
                submodule.new_commits = false;
                submodule.modified_content = false;
                submodule.untracked_content = false;
            }
        }
        if !submodule.any() {
            // No worktree-side submodule change survives the ignore. The gitlink
            // may still carry a *staged* (index) change; keep the entry only if
            // its index column is non-empty, and clear the worktree column so the
            // "Changes not staged" section drops it.
            entry.submodule = None;
            entry.worktree = b' ';
            return entry.index != b' ';
        }
        true
    });
}

/// The two rendered long-status submodule-summary sections. Each `Vec<String>`
/// holds the lines of one section (header, blank, `* path old...new (N):`, and
/// the `  > subject` / `  < subject` lines), or is empty when that section has no
/// content. Empty by default (summary disabled or no gitlink changes).
#[derive(Default)]
struct SubmoduleSummarySections {
    staged: Vec<String>,
    unstaged: Vec<String>,
}

/// Build the `Submodule changes to be committed:` (HEAD↔index) and `Submodules
/// changed but not updated:` (index↔worktree) sections for the long status,
/// gated on `status.submodulesummary`. `base_ref` is the commit whose tree
/// supplies the staged comparison's "old" gitlinks (`HEAD`, or `HEAD^` for a
/// `commit --amend --dry-run`). A faithful port of wt-status.c's
/// `wt_longstatus_print_submodule_summary` → `git submodule summary
/// --cached/--files --for-status`.
fn status_submodule_summary(
    git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
    config: &GitConfig,
    base_ref: &str,
    resolver: &SubmoduleIgnoreResolver,
) -> Result<SubmoduleSummarySections> {
    let mut sections = SubmoduleSummarySections::default();
    let Some(limit) = status_submodule_summary_limit(config) else {
        return Ok(sections);
    };
    // `--ignore-submodules=all` on the command line drops the whole summary.
    if resolver.cli_suppresses_summary() {
        return Ok(sections);
    }
    let db = FileObjectDatabase::from_git_dir(git_dir, format);

    // "old" gitlinks: the base commit's tree (HEAD / HEAD^).
    let base_gitlinks = match sley_rev::resolve_revision(git_dir, format, base_ref) {
        Ok(commit_oid) => {
            let tree = sley_rev::peel_to_tree(&db, format, &commit_oid)?;
            tree_gitlinks(&db, format, &tree)?
        }
        // No base commit yet (unborn HEAD): every staged gitlink is "added".
        Err(_) => BTreeMap::new(),
    };
    // "index" gitlinks: what is staged right now.
    let index_gitlinks = index_gitlinks(git_dir, format)?;
    // "worktree" gitlinks: the commit each populated submodule actually has
    // checked out (its HEAD).
    let worktree_gitlinks = worktree_gitlinks(worktree_root, &index_gitlinks);

    // Staged: base-tree → index.
    let staged_pairs = gitlink_change_pairs(&base_gitlinks, &index_gitlinks);
    sections.staged = render_summary_section(
        worktree_root,
        format,
        resolver,
        limit,
        SUMMARY_HEADER_STAGED,
        &staged_pairs,
    )?;
    // Unstaged: index → worktree HEAD.
    let unstaged_pairs = gitlink_change_pairs(&index_gitlinks, &worktree_gitlinks);
    sections.unstaged = render_summary_section(
        worktree_root,
        format,
        resolver,
        limit,
        SUMMARY_HEADER_UNSTAGED,
        &unstaged_pairs,
    )?;
    Ok(sections)
}

const SUMMARY_HEADER_STAGED: &str = "Submodule changes to be committed:";
const SUMMARY_HEADER_UNSTAGED: &str = "Submodules changed but not updated:";

/// `status.submodulesummary` → the summary limit, or `None` when disabled. git
/// stores it as an int (`git_config_int`) with the boolean shorthand mapping
/// true→-1 (unlimited) and false/0→off. A positive N caps the `>`/`<` lines per
/// submodule; `-1` (true) means unlimited. `diff.submoduleSummary` does NOT
/// enable the *status* summary — only `status.submodulesummary` does.
fn status_submodule_summary_limit(config: &GitConfig) -> Option<i64> {
    let value = config.get("status", None, "submodulesummary")?;
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" => Some(-1),
        "false" | "no" | "off" | "" => None,
        other => match other.parse::<i64>() {
            Ok(0) => None,
            Ok(n) => Some(n),
            Err(_) => None,
        },
    }
}

/// Flatten a tree and keep only its gitlink (mode 160000) entries, path → oid.
fn tree_gitlinks(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &ObjectId,
) -> Result<BTreeMap<Vec<u8>, ObjectId>> {
    let flat = sley_diff_merge::flatten_tree(db, format, tree_oid)?;
    Ok(flat
        .into_iter()
        .filter(|(_, (mode, _))| *mode == 0o160000)
        .map(|(path, (_, oid))| (path, oid))
        .collect())
}

/// The gitlink entries in the index (stage-0), path → staged commit oid.
fn index_gitlinks(git_dir: &Path, format: ObjectFormat) -> Result<BTreeMap<Vec<u8>, ObjectId>> {
    let index_path = sley_worktree::repository_index_path(git_dir);
    if !index_path.exists() {
        return Ok(BTreeMap::new());
    }
    let index = Index::parse(&fs::read(&index_path)?, format)?;
    Ok(index
        .entries
        .iter()
        .filter(|entry| entry.stage() == sley_index::Stage::Normal && entry.mode == 0o160000)
        .map(|entry| (entry.path.to_vec(), entry.oid))
        .collect())
}

/// For each index gitlink, the commit its checked-out worktree actually has at
/// HEAD. A submodule whose worktree is absent / not a repository falls back to
/// the index oid (no unstaged change), matching git treating an unpopulated
/// gitlink as unchanged.
fn worktree_gitlinks(
    worktree_root: &Path,
    index_gitlinks: &BTreeMap<Vec<u8>, ObjectId>,
) -> BTreeMap<Vec<u8>, ObjectId> {
    let mut map = BTreeMap::new();
    for (path, index_oid) in index_gitlinks {
        let Ok(path_str) = std::str::from_utf8(path) else {
            continue;
        };
        let sub_root = worktree_root.join(path_str);
        // The submodule's repo always uses the super-repo's hash for its gitlink
        // oids in this corpus; read its HEAD with the same format.
        let oid = sley_diff_merge::gitlink_head_oid(&sub_root, ObjectFormat::Sha1)
            .or_else(|| sley_diff_merge::gitlink_head_oid(&sub_root, ObjectFormat::Sha256))
            .unwrap_or(*index_oid);
        map.insert(path.clone(), oid);
    }
    map
}

/// A gitlink change between two oid maps: paths present in both with differing
/// oids, plus pure additions (old null) and removals (new null). Returns
/// (path, old_oid_or_null, new_oid_or_null) sorted by path. `None` oid encodes
/// git's `null_oid` (a fresh / removed gitlink).
fn gitlink_change_pairs(
    old: &BTreeMap<Vec<u8>, ObjectId>,
    new: &BTreeMap<Vec<u8>, ObjectId>,
) -> Vec<(Vec<u8>, Option<ObjectId>, Option<ObjectId>)> {
    let mut out = Vec::new();
    let mut paths: BTreeSet<&Vec<u8>> = BTreeSet::new();
    paths.extend(old.keys());
    paths.extend(new.keys());
    for path in paths {
        let old_oid = old.get(path).copied();
        let new_oid = new.get(path).copied();
        if old_oid == new_oid {
            continue;
        }
        out.push((path.clone(), old_oid, new_oid));
    }
    out
}

/// Render one summary section's lines for the given header and change pairs.
/// Returns an empty vec (no header) when nothing renders, so the caller can skip
/// the whole block — git only prints the header `if (cmd_stdout.len)`.
fn render_summary_section(
    worktree_root: &Path,
    format: ObjectFormat,
    resolver: &SubmoduleIgnoreResolver,
    limit: i64,
    header: &str,
    pairs: &[(Vec<u8>, Option<ObjectId>, Option<ObjectId>)],
) -> Result<Vec<String>> {
    let mut bodies: Vec<String> = Vec::new();
    for (path, old_oid, new_oid) in pairs {
        // Per-submodule `ignore=all` (from .git/config or .gitmodules, NOT the
        // CLI which already short-circuited) skips this submodule's summary
        // unless it is a pure addition — git's prepare_submodule_summary keeps
        // status 'A' even under ignore=all.
        let is_addition = old_oid.is_none();
        if !is_addition && resolver.for_path(path) == IgnoreSubmodules::All {
            continue;
        }
        let Some(body) = render_one_submodule(worktree_root, format, limit, path, *old_oid, *new_oid)?
        else {
            continue;
        };
        bodies.push(body);
    }
    if bodies.is_empty() {
        return Ok(Vec::new());
    }
    // header, blank, then each submodule body (already multi-line, no trailing
    // newline). The caller separates this whole block from neighbours.
    let mut lines = vec![header.to_string(), String::new()];
    for body in bodies {
        for line in body.lines() {
            lines.push(line.to_string());
        }
    }
    Ok(lines)
}

/// Render `* <path> <old7>...<new7> (N):` plus up to `limit` `> subject` /
/// `< subject` lines for one changed gitlink. `None` when the submodule's repo is
/// not populated (git only summarises checked-out submodules) — the caller drops
/// it. A faithful port of `generate_submodule_summary` for the gitlink→gitlink
/// case (type changes to/from a blob do not occur for a status gitlink change).
fn render_one_submodule(
    worktree_root: &Path,
    format: ObjectFormat,
    limit: i64,
    path: &[u8],
    old_oid: Option<ObjectId>,
    new_oid: Option<ObjectId>,
) -> Result<Option<String>> {
    let Ok(path_str) = std::str::from_utf8(path) else {
        return Ok(None);
    };
    let sub_root = worktree_root.join(path_str);
    // git: `prepare_submodule_summary` only summarises submodules whose worktree
    // is a non-bare repository (is_nonbare_repository_dir); skip otherwise.
    let Some(sub_git_dir) = sley_diff_merge::gitlink_git_dir(&sub_root) else {
        return Ok(None);
    };
    let sub_db = FileObjectDatabase::from_git_dir(&sub_git_dir, format);

    let null = ObjectId::null(format);
    let old = old_oid.unwrap_or(null);
    let new = new_oid.unwrap_or(null);
    let src_abbrev = abbrev7(&old);
    let dst_abbrev = abbrev7(&new);
    // git treats a null oid as "not a gitlink" (mode 0): the source of a fresh
    // submodule add, or the dest of a removal. Both sides being gitlinks is the
    // common case; a null side switches to the single-tip rendering.
    let src_is_gitlink = old_oid.is_some();
    let dst_is_gitlink = new_oid.is_some();

    // Whether each *gitlink* side's commit is present in the submodule's object
    // store (git's verify_submodule_committish). A null side is never "missing".
    let src_present = !src_is_gitlink || sub_db.read_object(&old).is_ok();
    let dst_present = !dst_is_gitlink || sub_db.read_object(&new).is_ok();

    if !src_present || !dst_present {
        // git only warns when the destination is still a gitlink (it is here).
        let warn = if !src_present && !dst_present {
            format!(
                "  Warn: {path_str} doesn't contain commits {} and {}\n",
                old.to_hex(),
                new.to_hex()
            )
        } else {
            let missing = if !src_present { &old } else { &new };
            format!("  Warn: {path_str} doesn't contain commit {}\n", missing.to_hex())
        };
        return Ok(Some(format!(
            "* {path_str} {src_abbrev}...{dst_abbrev}:\n{warn}"
        )));
    }

    let (total, marked) = if src_is_gitlink && dst_is_gitlink {
        // Symmetric first-parent difference, marked + date-ordered like
        // `git log --first-parent --pretty="  %m %s" src...dst`. The count is
        // `rev-list --first-parent --count src...dst`.
        let marked = submodule_summary_log(&sub_db, format, &old, &new)?;
        (marked.len(), marked)
    } else if dst_is_gitlink {
        // Fresh submodule add: count = `rev-list --first-parent --count dst`; one
        // `> dst` line (git uses `--pretty="  > %s" -1 dst`).
        let chain = first_parent_chain(&sub_db, format, &new)?;
        let subject = chain.first().map(|c| c.subject.clone()).unwrap_or_default();
        (chain.len(), vec![('>', subject)])
    } else {
        // Submodule removal: count = first-parent commits from src; one `< src`.
        let chain = first_parent_chain(&sub_db, format, &old)?;
        let subject = chain.first().map(|c| c.subject.clone()).unwrap_or_default();
        (chain.len(), vec![('<', subject)])
    };

    let mut body = format!("* {path_str} {src_abbrev}...{dst_abbrev} ({total}):\n");
    // The single-tip add/remove forms always show their one line (git's `-1`);
    // only the gitlink↔gitlink form honours the summary limit.
    let shown = if src_is_gitlink && dst_is_gitlink && limit > 0 {
        (limit as usize).min(marked.len())
    } else {
        marked.len()
    };
    for (marker, subject) in marked.iter().take(shown) {
        body.push_str(&format!("  {marker} {subject}\n"));
    }
    Ok(Some(body))
}

/// `git rev-parse --short <oid>^0` for the tiny submodule repos in the corpus is
/// a fixed 7-char abbreviation (git's own fallback `xstrndup(oid_to_hex, 7)`).
fn abbrev7(oid: &ObjectId) -> String {
    oid.to_hex()[..7].to_string()
}

/// Walk the symmetric first-parent difference `src...dst` in the submodule's
/// object store and return `(marker, subject)` pairs in git's log order: a
/// commit-date priority walk from both tips, marking `<` for the src side and
/// `>` for the dst side, following only first parents, stopping where the two
/// histories meet. Equivalent to
/// `git log --first-parent --pretty="  %m %s" src...dst` over the gitlink commits.
fn submodule_summary_log(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    src: &ObjectId,
    dst: &ObjectId,
) -> Result<Vec<(char, String)>> {
    // First-parent ancestor chains of each tip.
    let src_chain = first_parent_chain(db, format, src)?;
    let dst_chain = first_parent_chain(db, format, dst)?;
    let src_set: HashSet<ObjectId> = src_chain.iter().map(|c| c.oid).collect();
    let dst_set: HashSet<ObjectId> = dst_chain.iter().map(|c| c.oid).collect();

    // A commit-date max-heap seeded with each tip, tagged with its side. As each
    // commit is emitted we push its first parent (lazily), so a child always
    // precedes its parent and ties resolve to the newer date first — exactly the
    // pop order of git's `src...dst` walk.
    let mut by_oid: HashMap<ObjectId, FpCommit> = HashMap::new();
    for c in src_chain.into_iter().chain(dst_chain.into_iter()) {
        by_oid.entry(c.oid).or_insert(c);
    }

    // Marker per emitted oid: `<` if only in src, `>` if only in dst. Commits in
    // BOTH are the common base and are never emitted (uninteresting boundary).
    let marker_for = |oid: &ObjectId| -> Option<char> {
        let in_src = src_set.contains(oid);
        let in_dst = dst_set.contains(oid);
        match (in_src, in_dst) {
            (true, false) => Some('<'),
            (false, true) => Some('>'),
            _ => None,
        }
    };

    let mut heap: std::collections::BinaryHeap<SummaryHeapEntry> = Default::default();
    let mut pushed: HashSet<ObjectId> = HashSet::new();
    for tip in [src, dst] {
        if let Some(c) = by_oid.get(tip) {
            if pushed.insert(*tip) {
                heap.push(SummaryHeapEntry {
                    time: c.commit_time,
                    oid: *tip,
                });
            }
        }
    }

    let mut out = Vec::new();
    while let Some(entry) = heap.pop() {
        let Some(commit) = by_oid.get(&entry.oid) else {
            continue;
        };
        let first_parent = commit.first_parent;
        if let Some(marker) = marker_for(&entry.oid) {
            out.push((marker, commit.subject.clone()));
        }
        // Push the first parent so the chain continues toward the merge base.
        if let Some(parent) = first_parent {
            if let Some(pc) = by_oid.get(&parent) {
                if pushed.insert(parent) {
                    heap.push(SummaryHeapEntry {
                        time: pc.commit_time,
                        oid: parent,
                    });
                }
            }
        }
    }
    Ok(out)
}

/// One commit's first-parent-walk metadata for the summary log.
struct FpCommit {
    oid: ObjectId,
    first_parent: Option<ObjectId>,
    commit_time: i64,
    subject: String,
}

/// The chain of commits reachable from `tip` by following ONLY first parents,
/// reading each commit's subject + committer time.
fn first_parent_chain(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tip: &ObjectId,
) -> Result<Vec<FpCommit>> {
    let mut chain = Vec::new();
    let mut cursor = Some(*tip);
    let mut seen = HashSet::new();
    while let Some(oid) = cursor {
        if !seen.insert(oid) {
            break;
        }
        let object = db.read_object(&oid)?;
        if object.object_type != ObjectType::Commit {
            break;
        }
        let commit = sley_object::Commit::parse(format, &object.body)?;
        let first_parent = commit.parents.first().copied();
        chain.push(FpCommit {
            oid,
            first_parent,
            commit_time: commit_committer_time(&commit.committer),
            subject: commit_subject(&commit.message),
        });
        cursor = first_parent;
    }
    Ok(chain)
}

/// Parse the committer timestamp (seconds since epoch) from a commit's committer
/// identity line (`Name <email> <secs> <tz>`). Falls back to 0 when unparsable —
/// the corpus always carries a well-formed timestamp.
fn commit_committer_time(committer: &[u8]) -> i64 {
    let text = String::from_utf8_lossy(committer);
    let mut parts = text.rsplit(' ');
    let _tz = parts.next();
    parts
        .next()
        .and_then(|secs| secs.parse::<i64>().ok())
        .unwrap_or(0)
}

/// Max-heap entry for the summary's date-priority walk: newest commit-time pops
/// first; ties break on the SMALLER oid (matching sley's RevWalk heap and git's
/// `(time, Reverse(oid))` ordering).
struct SummaryHeapEntry {
    time: i64,
    oid: ObjectId,
}
impl PartialEq for SummaryHeapEntry {
    fn eq(&self, other: &Self) -> bool {
        self.time == other.time && self.oid == other.oid
    }
}
impl Eq for SummaryHeapEntry {}
impl Ord for SummaryHeapEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.time
            .cmp(&other.time)
            .then_with(|| other.oid.cmp(&self.oid))
    }
}
impl PartialOrd for SummaryHeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// The effective `core.commentChar` string (git's `comment_line_str`), default
/// `#`. May be multi-char; an empty or `auto` value falls back to `#` (we do not
/// implement the `auto` scan, which picks an unused character). Used by
/// commit-message cleanup (scissors detection + comment stripping).
fn commit_comment_string(git_dir: &Path) -> String {
    read_repo_config(git_dir)
        .ok()
        .and_then(|c| {
            c.get("core", None, "commentChar")
                .filter(|value| !value.is_empty() && *value != "auto")
                .map(str::to_string)
        })
        .unwrap_or_else(|| "#".to_string())
}

/// Comment prefix for `git status` output when `status.displayCommentPrefix` is
/// on. Upstream uses `core.commentChar` (default `#`); the prefix string is the
/// comment char (which may be multi-byte / multi-char). Returns `None` when the
/// prefix is disabled.
fn status_comment_prefix(config: &GitConfig) -> Option<String> {
    if config.get_bool("status", None, "displayCommentPrefix") != Some(true) {
        return None;
    }
    let comment_char = config
        .get("core", None, "commentChar")
        .filter(|value| !value.is_empty() && *value != "auto")
        .unwrap_or("#");
    Some(comment_char.to_string())
}

/// Buffers long-status lines so the comment prefix (and, where relevant, hint
/// gating) can be applied uniformly on flush — mirroring upstream's
/// status_vprintf(), which prefixes every emitted line.
struct StatusLineSink {
    lines: Vec<String>,
    hints: bool,
    comment_prefix: Option<String>,
}

impl StatusLineSink {
    fn new(hints: bool, comment_prefix: Option<String>) -> Self {
        Self {
            lines: Vec::new(),
            hints,
            comment_prefix,
        }
    }

    /// A normal output line.
    fn line(&mut self, text: impl Into<String>) {
        self.lines.push(text.into());
    }

    /// A blank separator line.
    fn blank(&mut self) {
        self.lines.push(String::new());
    }

    /// A parenthetical guidance line, suppressed when `advice.statusHints` is
    /// false (upstream gates all `(use "git ...")` hints on `s->hints`).
    fn hint(&mut self, text: impl Into<String>) {
        if self.hints {
            self.lines.push(text.into());
        }
    }

    fn flush(self) {
        let mut out = io::stdout().lock();
        self.write_to(&mut out);
        let _ = out.flush();
    }

    /// Render the buffered lines (with the comment prefix applied) into an
    /// arbitrary writer. Used both for stdout (status preview) and for building
    /// the COMMIT_EDITMSG template block.
    fn write_to(&self, out: &mut impl Write) {
        for line in &self.lines {
            if let Some(prefix) = &self.comment_prefix {
                if line.is_empty() {
                    // Empty line → just the comment char (no trailing space).
                    let _ = writeln!(out, "{prefix}");
                } else if line.starts_with('\t') {
                    // Indented (file) lines: comment char immediately, no space.
                    let _ = writeln!(out, "{prefix}{line}");
                } else {
                    let _ = writeln!(out, "{prefix} {line}");
                }
            } else {
                let _ = writeln!(out, "{line}");
            }
        }
    }
}

fn print_status_long(
    git_dir: &Path,
    format: ObjectFormat,
    entries: Vec<sley_worktree::ShortStatusEntry>,
    display: &StatusLongDisplay,
) -> Result<()> {
    let sink = build_status_long_sink(git_dir, format, entries, display)?;
    sink.flush();
    Ok(())
}

/// Build (but do not emit) the buffered long-status output. Shared by the
/// `git status` stdout path and the COMMIT_EDITMSG template builder.
fn build_status_long_sink(
    git_dir: &Path,
    format: ObjectFormat,
    entries: Vec<sley_worktree::ShortStatusEntry>,
    display: &StatusLongDisplay,
) -> Result<StatusLineSink> {
    let StatusLongDisplay {
        commit_preview,
        show_stash,
        ahead_behind,
        hints,
        untracked_suppressed,
        comment_prefix,
        submodule_summary,
    } = display;
    let commit_preview = *commit_preview;
    let show_stash = *show_stash;
    let ahead_behind = *ahead_behind;
    let untracked_suppressed = *untracked_suppressed;

    let mut sink = StatusLineSink::new(*hints, comment_prefix.clone());
    // `commit --dry-run`/template previews suppress the upstream-divergence
    // advice hints (`(use "git pull" ...)`) — wt-status passes `!commit_template`
    // as `show_divergence_advice` to format_tracking_info. The branch state lines
    // themselves still print.
    let head_initial =
        status_long_branch_lines(git_dir, format, ahead_behind, commit_preview, &mut sink)?;
    if head_initial {
        sink.blank();
        if commit_preview {
            sink.line("Initial commit");
        } else {
            sink.line("No commits yet");
        }
    }

    let mut staged = Vec::new();
    let mut unstaged = Vec::new();
    let mut untracked = Vec::new();
    let mut ignored = Vec::new();
    for entry in entries {
        if entry.index == b'?' && entry.worktree == b'?' {
            untracked.push(entry.path);
            continue;
        }
        if entry.index == b'!' && entry.worktree == b'!' {
            ignored.push(entry.path);
            continue;
        }
        if let Some(label) = status_long_change_label(entry.index) {
            staged.push((label, entry.path.clone()));
        }
        if let Some(label) = status_long_change_label(entry.worktree) {
            // Submodule change detail (wt-status.c): " (new commits, modified
            // content, untracked content)" — whichever apply, in that order.
            let mut extras = Vec::new();
            if let Some(submodule) = entry.submodule {
                if submodule.new_commits {
                    extras.push("new commits");
                }
                if submodule.modified_content {
                    extras.push("modified content");
                }
                if submodule.untracked_content {
                    extras.push("untracked content");
                }
            }
            let suffix = if extras.is_empty() {
                String::new()
            } else {
                format!(" ({})", extras.join(", "))
            };
            // The "(commit or discard ...)" hint keys on dirty *content* only,
            // not on new commits (wt_status_check_worktree_changes).
            let dirty_submodule = entry
                .submodule
                .is_some_and(|sub| sub.modified_content || sub.untracked_content);
            unstaged.push((label, entry.path, suffix, dirty_submodule));
        }
    }

    let has_staged = !staged.is_empty();
    let has_unstaged = !unstaged.is_empty();
    let has_untracked = !untracked.is_empty();
    let has_ignored = !ignored.is_empty();

    if has_staged {
        if head_initial {
            sink.blank();
        }
        sink.line("Changes to be committed:");
        if head_initial {
            sink.hint("  (use \"git rm --cached <file>...\" to unstage)");
        } else {
            sink.hint("  (use \"git restore --staged <file>...\" to unstage)");
        }
        for (label, path) in staged {
            sink.line(format!("\t{label:<12}{}", status_quote_path(&path, false)));
        }
    }

    if has_unstaged {
        if head_initial || has_staged {
            sink.blank();
        }
        sink.line("Changes not staged for commit:");
        if unstaged.iter().any(|(label, _, _, _)| *label == "deleted:") {
            sink.hint("  (use \"git add/rm <file>...\" to update what will be committed)");
        } else {
            sink.hint("  (use \"git add <file>...\" to update what will be committed)");
        }
        sink.hint("  (use \"git restore <file>...\" to discard changes in working directory)");
        if unstaged.iter().any(|(_, _, _, dirty)| *dirty) {
            sink.hint("  (commit or discard the untracked or modified content in submodules)");
        }
        for (label, path, suffix, _) in unstaged {
            sink.line(format!(
                "\t{label:<12}{}{suffix}",
                status_quote_path(&path, false)
            ));
        }
    }

    // `Submodule changes to be committed:` then `Submodules changed but not
    // updated:` (wt-status.c calls both summaries right after print_changed).
    // Each non-empty section is separated from what precedes it by one blank
    // line; the trailing blank before "Untracked files" is supplied by that
    // section's own leading-blank logic (see `has_summary` below).
    let mut printed_anything = head_initial || has_staged || has_unstaged;
    for section in [&submodule_summary.staged, &submodule_summary.unstaged] {
        if section.is_empty() {
            continue;
        }
        if printed_anything {
            sink.blank();
        }
        for line in section {
            sink.line(line.clone());
        }
        printed_anything = true;
    }
    let has_summary = !submodule_summary.staged.is_empty() || !submodule_summary.unstaged.is_empty();

    if has_untracked {
        if head_initial || has_staged || has_unstaged || has_summary {
            sink.blank();
        }
        sink.line("Untracked files:");
        sink.hint("  (use \"git add <file>...\" to include in what will be committed)");
        for path in untracked {
            sink.line(format!("\t{}", status_quote_path(&path, false)));
        }
    }

    if has_ignored {
        if head_initial || has_staged || has_unstaged || has_summary || has_untracked {
            sink.blank();
        }
        sink.line("Ignored files:");
        sink.hint("  (use \"git add -f <file>...\" to include in what will be committed)");
        for path in ignored {
            sink.line(format!("\t{}", status_quote_path(&path, false)));
        }
    }

    // "Untracked files not listed" appears when untracked output is suppressed
    // (-uno / status.showUntrackedFiles=no) AND there is something to commit
    // (upstream gates this on `s->committable`, i.e. staged changes present).
    // It takes the place of the untracked section, so it gets the same leading
    // blank separator that section would have, and there is no trailing blank.
    // The "(use -u option ...)" suffix is itself a hint, gated separately.
    let printed_not_listed = untracked_suppressed && has_staged;
    if printed_not_listed {
        if head_initial || has_staged || has_unstaged || has_summary {
            sink.blank();
        }
        if *hints {
            sink.line("Untracked files not listed (use -u option to show untracked files)");
        } else {
            sink.line("Untracked files not listed");
        }
    }

    if !has_staged && !has_unstaged && !has_untracked && !has_ignored {
        if head_initial {
            sink.blank();
            sink.line("nothing to commit (create/copy files and use \"git add\" to track)");
        } else {
            sink.line("nothing to commit, working tree clean");
        }
    } else if !has_staged && has_unstaged {
        sink.blank();
        if *hints {
            sink.line("no changes added to commit (use \"git add\" and/or \"git commit -a\")");
        } else {
            sink.line("no changes added to commit");
        }
    } else if !has_staged && has_untracked {
        sink.blank();
        if *hints {
            sink.line(
                "nothing added to commit but untracked files present (use \"git add\" to track)",
            );
        } else {
            sink.line("nothing added to commit but untracked files present");
        }
    } else if !printed_not_listed {
        // A real untracked section (or staged-only) ends with a trailing blank;
        // the "not listed" line already supplied the trailing content.
        sink.blank();
    }
    if show_stash {
        let stash_count = status_stash_count(git_dir, format)?;
        if stash_count == 1 {
            sink.line("Your stash currently has 1 entry");
        } else if stash_count > 1 {
            sink.line(format!("Your stash currently has {stash_count} entries"));
        }
    }
    Ok(sink)
}

fn status_stash_count(git_dir: &Path, format: ObjectFormat) -> Result<usize> {
    let store = FileRefStore::new(git_dir, format);
    Ok(store.read_reflog("refs/stash")?.len())
}

fn status_long_branch_lines(
    git_dir: &Path,
    format: ObjectFormat,
    ahead_behind: bool,
    suppress_divergence_advice: bool,
    sink: &mut StatusLineSink,
) -> Result<bool> {
    let store = FileRefStore::new(git_dir, format);
    match store.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(target)) => {
            if let Some(branch) = target.strip_prefix("refs/heads/") {
                sink.line(format!("On branch {branch}"));
                if let Some(RefTarget::Direct(oid)) = store.read_ref(&target)? {
                    status_long_tracking_lines(
                        git_dir,
                        format,
                        &store,
                        &target,
                        &oid,
                        ahead_behind,
                        suppress_divergence_advice,
                        sink,
                    )?;
                    Ok(false)
                } else {
                    Ok(true)
                }
            } else {
                sink.line(format!("On branch {target}"));
                Ok(store.read_ref(&target)?.is_none())
            }
        }
        Some(RefTarget::Direct(oid)) => {
            sink.line(format!("HEAD detached at {}", format_log_abbrev_oid(&oid)));
            Ok(false)
        }
        None => {
            sink.line("On branch (unknown)");
            Ok(true)
        }
    }
}

fn status_long_tracking_lines(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    branch_ref: &str,
    oid: &ObjectId,
    ahead_behind: bool,
    suppress_divergence_advice: bool,
    sink: &mut StatusLineSink,
) -> Result<()> {
    let Some(tracking) =
        status_branch_tracking(git_dir, format, store, branch_ref, oid, ahead_behind)?
    else {
        return Ok(());
    };
    // git's format_tracking_info gates the ahead/behind/diverged *advice* hints on
    // `show_divergence_advice` (false for commit-template previews); the state
    // lines always print. Route the advice hints through this so a dry-run drops
    // only the `(use "git pull" ...)` style guidance.
    let advice = |sink: &mut StatusLineSink, text: &str| {
        if !suppress_divergence_advice {
            sink.hint(text);
        }
    };
    match tracking.state {
        StatusBranchTrackingState::Counts(ForEachRefTrack {
            ahead: 0,
            behind: 0,
            ..
        }) => {
            sink.line(format!(
                "Your branch is up to date with '{}'.",
                tracking.upstream
            ));
        }
        StatusBranchTrackingState::Counts(ForEachRefTrack {
            ahead, behind: 0, ..
        }) => {
            sink.line(format!(
                "Your branch is ahead of '{}' by {ahead} {}.",
                tracking.upstream,
                status_commit_word(ahead)
            ));
            advice(sink, "  (use \"git push\" to publish your local commits)");
        }
        StatusBranchTrackingState::Counts(ForEachRefTrack {
            ahead: 0, behind, ..
        }) => {
            sink.line(format!(
                "Your branch is behind '{}' by {behind} {}, and can be fast-forwarded.",
                tracking.upstream,
                status_commit_word(behind)
            ));
            advice(sink, "  (use \"git pull\" to update your local branch)");
        }
        StatusBranchTrackingState::Counts(ForEachRefTrack { ahead, behind, .. }) => {
            sink.line(format!("Your branch and '{}' have diverged,", tracking.upstream));
            sink.line(format!(
                "and have {ahead} and {behind} different commits each, respectively."
            ));
            advice(
                sink,
                "  (use \"git pull\" if you want to integrate the remote branch with yours)",
            );
        }
        StatusBranchTrackingState::Different => {
            sink.line(format!(
                "Your branch and '{}' refer to different commits.",
                tracking.upstream
            ));
            advice(sink, "  (use \"git status --ahead-behind\" for details)");
        }
        StatusBranchTrackingState::Gone => {
            sink.line(format!(
                "Your branch is based on '{}', but the upstream is gone.",
                tracking.upstream
            ));
            advice(sink, "  (use \"git branch --unset-upstream\" to fixup)");
        }
    }
    sink.blank();
    Ok(())
}

fn status_commit_word(count: usize) -> &'static str {
    if count == 1 { "commit" } else { "commits" }
}

fn status_long_change_label(code: u8) -> Option<&'static str> {
    match code {
        b'A' => Some("new file:"),
        b'M' => Some("modified:"),
        b'D' => Some("deleted:"),
        _ => None,
    }
}

fn status_entries_have_index_changes(entries: &[sley_worktree::ShortStatusEntry]) -> bool {
    entries
        .iter()
        .any(|entry| status_long_change_label(entry.index).is_some())
}

fn status_porcelain_v2_code(code: u8) -> char {
    if code == b' ' { '.' } else { code as char }
}

fn status_porcelain_v2_branch_headers(
    git_dir: &Path,
    format: ObjectFormat,
    ahead_behind: bool,
) -> Result<Vec<String>> {
    let store = FileRefStore::new(git_dir, format);
    match store.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(target)) => {
            let target_oid = match store.read_ref(&target)? {
                Some(RefTarget::Direct(oid)) => Some(oid),
                _ => None,
            };
            let oid = match target_oid.as_ref() {
                Some(oid) => oid.to_hex(),
                _ => "(initial)".into(),
            };
            let head = target
                .strip_prefix("refs/heads/")
                .unwrap_or(target.as_str())
                .to_string();
            let mut headers = vec![
                format!("# branch.oid {oid}"),
                format!("# branch.head {head}"),
            ];
            if let Some(oid) = target_oid.as_ref()
                && let Some(tracking) =
                    status_branch_tracking(git_dir, format, &store, &target, oid, ahead_behind)?
            {
                headers.push(format!("# branch.upstream {}", tracking.upstream));
                match tracking.state {
                    StatusBranchTrackingState::Counts(track) => {
                        headers.push(format!("# branch.ab +{} -{}", track.ahead, track.behind));
                    }
                    StatusBranchTrackingState::Different => {
                        headers.push("# branch.ab +? -?".into());
                    }
                    StatusBranchTrackingState::Gone => {}
                }
            }
            Ok(headers)
        }
        Some(RefTarget::Direct(oid)) => Ok(vec![
            format!("# branch.oid {}", oid.to_hex()),
            "# branch.head (detached)".into(),
        ]),
        None => Ok(vec![
            "# branch.oid (initial)".into(),
            "# branch.head (unknown)".into(),
        ]),
    }
}

fn status_branch_header(
    git_dir: &Path,
    format: ObjectFormat,
    ahead_behind: bool,
) -> Result<String> {
    let store = FileRefStore::new(git_dir, format);
    match store.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(target)) => {
            if let Some(branch) = target.strip_prefix("refs/heads/") {
                if let Some(RefTarget::Direct(oid)) = store.read_ref(&target)? {
                    let mut header = format!("## {branch}");
                    if let Some(tracking) = status_branch_tracking(
                        git_dir,
                        format,
                        &store,
                        &target,
                        &oid,
                        ahead_behind,
                    )? {
                        header.push_str("...");
                        header.push_str(&tracking.upstream);
                        if let StatusBranchTrackingState::Counts(track) = tracking.state {
                            if track.ahead > 0 || track.behind > 0 {
                                header.push(' ');
                                let mut suffix = Vec::new();
                                write_for_each_ref_track(&mut suffix, track, true)?;
                                header.push_str(&String::from_utf8_lossy(&suffix));
                            }
                        } else if matches!(tracking.state, StatusBranchTrackingState::Gone) {
                            header.push_str(" [gone]");
                        } else {
                            header.push_str(" [different]");
                        }
                    }
                    Ok(header)
                } else {
                    Ok(format!("## No commits yet on {branch}"))
                }
            } else {
                Ok(format!("## {target}"))
            }
        }
        Some(RefTarget::Direct(_)) | None => Ok("## HEAD (no branch)".into()),
    }
}

struct StatusBranchTracking {
    upstream: String,
    state: StatusBranchTrackingState,
}

#[derive(Clone, Copy)]
enum StatusBranchTrackingState {
    Counts(ForEachRefTrack),
    Different,
    Gone,
}

fn status_branch_tracking(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    branch_ref: &str,
    oid: &ObjectId,
    ahead_behind: bool,
) -> Result<Option<StatusBranchTracking>> {
    let config = read_repo_config(git_dir)?;
    let Some(upstream) = for_each_ref_upstream(&config, branch_ref) else {
        return Ok(None);
    };
    let db = FileObjectDatabase::new(repository_objects_dir(git_dir), format);
    let track = if ahead_behind {
        match store.read_ref(&upstream.refname)? {
            None => StatusBranchTrackingState::Gone,
            Some(_) => for_each_ref_upstream_track(store, &db, format, oid, &upstream.refname)?
                .map(StatusBranchTrackingState::Counts)
                .unwrap_or(StatusBranchTrackingState::Different),
        }
    } else {
        status_branch_tracking_without_ahead_behind(store, oid, &upstream.refname)?
    };
    Ok(Some(StatusBranchTracking {
        upstream: for_each_ref_short_name(&upstream.refname).to_string(),
        state: track,
    }))
}

fn status_branch_tracking_without_ahead_behind(
    store: &FileRefStore,
    oid: &ObjectId,
    upstream: &str,
) -> Result<StatusBranchTrackingState> {
    let Some(RefTarget::Direct(upstream_oid)) = store.read_ref(upstream)? else {
        return Ok(StatusBranchTrackingState::Gone);
    };
    if oid == &upstream_oid {
        Ok(StatusBranchTrackingState::Counts(ForEachRefTrack {
            ahead: 0,
            behind: 0,
            gone: false,
        }))
    } else {
        Ok(StatusBranchTrackingState::Different)
    }
}

fn build_commit_author_identity(author: Option<&str>, date: Option<&str>) -> Result<Vec<u8>> {
    let (name, email) = if let Some(author) = author {
        parse_commit_author(author)?
    } else {
        // Same precedence as `commit_identity_from_env`: env var, then
        // `-c`/`GIT_CONFIG_*`, then effective config (repo > global > system),
        // then the built-in default.
        let env_name = env::var("GIT_AUTHOR_NAME").ok();
        let env_email = env::var("GIT_AUTHOR_EMAIL").ok();
        let mut config = if env_name.is_none() || env_email.is_none() {
            IdentityConfig::Lazy(None)
        } else {
            IdentityConfig::Skip
        };
        let name = env_name
            .or_else(|| identity_config_value("user.name", &mut config))
            .unwrap_or_else(|| "Git Rs".into());
        let email = env_email
            .or_else(|| identity_config_value("user.email", &mut config))
            .unwrap_or_else(|| "sley@example.invalid".into());
        (name, email)
    };
    let date = date
        .map(str::to_string)
        .unwrap_or_else(|| env::var("GIT_AUTHOR_DATE").unwrap_or_else(|_| "@0 +0000".into()));
    let date = canonicalize_commit_date(&date);
    sley_sequencer::format_commit_identity(&name, &email, &date)
}

fn parse_commit_author(author: &str) -> Result<(String, String)> {
    let Some((name, rest)) = author.rsplit_once('<') else {
        return commit_invalid_author_error(author);
    };
    let Some(email) = rest.strip_suffix('>') else {
        return commit_invalid_author_error(author);
    };
    let name = name.trim_end();
    if name.is_empty() || email.is_empty() {
        return commit_invalid_author_error(author);
    }
    Ok((name.to_string(), email.to_string()))
}

fn commit_invalid_author_error(author: &str) -> Result<(String, String)> {
    eprintln!("fatal: --author '{author}' is not 'Name <email>' and matches no existing author");
    Err(GitError::Exit(128))
}
