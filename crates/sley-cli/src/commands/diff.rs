//! Extracted from the crate root (sley#8 phase 1) — code motion only.

// A glob of the crate root brings every shared helper/type into scope via
// descendant-privacy; see commands::stash for the rationale.
use crate::*;

/// Peel a single revision string to the tree it names (commit/tag/tree all work).
fn diff_peel_rev_tree(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    rev: &str,
) -> Result<ObjectId> {
    let oid = resolve_revision(git_dir, format, rev)?;
    sley_rev::peel_to_tree(db, format, &oid)
}

fn diff_split_revisions(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    path_args: Vec<String>,
) -> Result<(Vec<ObjectId>, Vec<String>)> {
    let Some(first) = path_args.first() else {
        return Ok((Vec::new(), Vec::new()));
    };
    // Range forms name exactly two trees and consume only the first token. Check
    // `...` before `..` so `A...B` is not mis-split, and require both sides so a
    // relative path like `../x` (left side empty) is never taken as a range.
    // `A...B` (symmetric): diff merge-base(A,B)..B. An omitted side defaults to
    // HEAD. It is only a range when *both* endpoints resolve as revisions —
    // otherwise the token (e.g. a relative path `../x`) falls through to pathspec
    // handling, matching git's disambiguation.
    if let Some((left, right)) = first.split_once("...") {
        let left_spec = if left.is_empty() { "HEAD" } else { left };
        let right_spec = if right.is_empty() { "HEAD" } else { right };
        if let (Ok(left_oid), Ok(right_oid)) = (
            resolve_revision(git_dir, format, left_spec),
            resolve_revision(git_dir, format, right_spec),
        ) {
            let Some(base) = sley_rev::merge_bases(git_dir, format, db, &left_oid, &right_oid)?
                .into_iter()
                .next()
            else {
                eprintln!("fatal: {first}: no merge base");
                return Err(GitError::Exit(128));
            };
            let base_tree = sley_rev::peel_to_tree(db, format, &base)?;
            let right_tree = sley_rev::peel_to_tree(db, format, &right_oid)?;
            return Ok((vec![base_tree, right_tree], path_args[1..].to_vec()));
        }
    }
    // `A..B`: diff A..B. Omitted side defaults to HEAD; only a range when both
    // endpoints resolve.
    if let Some((left, right)) = first.split_once("..") {
        let left_spec = if left.is_empty() { "HEAD" } else { left };
        let right_spec = if right.is_empty() { "HEAD" } else { right };
        if let (Ok(left_tree), Ok(right_tree)) = (
            diff_peel_rev_tree(git_dir, format, db, left_spec),
            diff_peel_rev_tree(git_dir, format, db, right_spec),
        ) {
            return Ok((vec![left_tree, right_tree], path_args[1..].to_vec()));
        }
    }
    // Otherwise peel up to two leading args that each resolve as a revision.
    let mut trees = Vec::new();
    let mut rest = Vec::new();
    let mut iter = path_args.into_iter();
    for token in iter.by_ref() {
        if trees.len() < 2
            && let Ok(tree) = diff_peel_rev_tree(git_dir, format, db, &token)
        {
            trees.push(tree);
            continue;
        }
        rest.push(token);
        break;
    }
    rest.extend(iter);
    Ok((trees, rest))
}

pub(crate) fn cmd_diff(args: &[String]) -> Result<()> {
    let commands::diff_options::DiffOptions {
        output_format,
        cached,
        quiet,
        exit_code,
        compact_summary,
        stat_count,
        stat_widths,
        mut dirstat,
        dirstat_cli_params,
        context,
        reverse,
        pickaxe,
        pickaxe_all,
        pickaxe_regex,
        find_object_values,
        raw_abbrev,
        patch_abbrev,
        patch_full_index,
        color_always,
        diff_algorithm_control,
        diff_driver_control,
        diff_hunk_control,
        diff_whitespace_control,
        diff_output_indicator_control,
        diff_patch_context_control,
        diff_patch_output_control,
        diff_rewrite_control,
        diff_submodule_output_control,
        word_diff_mode,
        word_diff_regex,
        no_index,
        mut diff_relative,
        diff_relative_explicit,
        src_prefix,
        dst_prefix,
        mut head,
        z,
        mut detect_renames,
        mut detect_copies,
        find_copies_harder,
        rename_empty,
        mut inexact_renames,
        renames_explicit,
        rename_threshold,
        copy_threshold,
        diff_filter,
        ignore_submodules_cli,
        mut path_args,
        explicit_paths,
    } = commands::diff_options::setup_diff_options(args)?;

    let name_status = output_format.contains(commands::diff_options::DiffOutputFormat::NAME_STATUS);
    let name_only = output_format.contains(commands::diff_options::DiffOutputFormat::NAME_ONLY);
    let check = output_format.contains(commands::diff_options::DiffOutputFormat::CHECK);
    let summary = output_format.contains(commands::diff_options::DiffOutputFormat::SUMMARY);
    let raw = output_format.contains(commands::diff_options::DiffOutputFormat::RAW);
    let stat = output_format.contains(commands::diff_options::DiffOutputFormat::DIFFSTAT);
    let numstat = output_format.contains(commands::diff_options::DiffOutputFormat::NUMSTAT);
    let shortstat = output_format.contains(commands::diff_options::DiffOutputFormat::SHORTSTAT);
    let patch = output_format.contains(commands::diff_options::DiffOutputFormat::PATCH);
    let no_patch = output_format.contains(commands::diff_options::DiffOutputFormat::NO_OUTPUT);
    if diff_algorithm_control && !name_status && !name_only {
        return Err(GitError::Unsupported(
            "diff algorithm controls are not supported for this output mode".into(),
        ));
    }
    if diff_driver_control && !name_status && !name_only {
        return Err(GitError::Unsupported(
            "diff driver controls are not supported for this output mode".into(),
        ));
    }
    if diff_hunk_control && !name_status && !name_only {
        return Err(GitError::Unsupported(
            "diff hunk context controls are not supported for this output mode".into(),
        ));
    }
    if diff_whitespace_control && !name_status && !name_only {
        return Err(GitError::Unsupported(
            "diff whitespace controls are not supported for this output mode".into(),
        ));
    }
    if diff_output_indicator_control && !name_status && !name_only {
        return Err(GitError::Unsupported(
            "diff output indicator controls are not supported for this output mode".into(),
        ));
    }
    if diff_patch_context_control && !name_status && !name_only {
        return Err(GitError::Unsupported(
            "diff patch context controls are not supported for this output mode".into(),
        ));
    }
    if diff_patch_output_control && !name_status && !name_only {
        return Err(GitError::Unsupported(
            "diff patch output controls are not supported for this output mode".into(),
        ));
    }
    if diff_rewrite_control && !name_status && !name_only {
        return Err(GitError::Unsupported(
            "diff rewrite controls are not supported for this output mode".into(),
        ));
    }
    if diff_submodule_output_control && !name_status && !name_only {
        return Err(GitError::Unsupported(
            "diff submodule output controls are not supported for this output mode".into(),
        ));
    }
    if reverse && !name_status && !name_only {
        return Err(GitError::Unsupported(
            "diff reverse output is not supported for this output mode".into(),
        ));
    }
    if (pickaxe.is_some() || pickaxe_all || pickaxe_regex) && !name_status && !name_only {
        return Err(GitError::Unsupported(
            "diff pickaxe controls are not supported for this output mode".into(),
        ));
    }
    if !find_object_values.is_empty() && !name_status && !name_only {
        return Err(GitError::Unsupported(
            "diff find-object output is not supported for this output mode".into(),
        ));
    }
    if check && !name_status && !name_only {
        return Err(GitError::Unsupported(
            "diff check output is not supported".into(),
        ));
    }
    if pickaxe_all && !find_object_values.is_empty() {
        return diff_find_object_pickaxe_all_conflict_error();
    }
    if pickaxe.is_some() && pickaxe_regex {
        return Err(GitError::Unsupported(
            "diff pickaxe regex matching is not supported".into(),
        ));
    }
    let cwd = env::current_dir()?;
    if no_index {
        let mut paths = path_args;
        if head {
            paths.insert(0, "HEAD".to_string());
        }
        paths.extend(explicit_paths);
        return cmd_diff_no_index(
            &cwd,
            &paths,
            DiffNoIndexParams {
                context: context.unwrap_or(3),
                color: color_always,
                word_diff_mode,
                word_diff_regex: word_diff_regex.as_deref(),
                src_prefix: &src_prefix,
                dst_prefix: &dst_prefix,
                quiet,
            },
        );
    }
    let git_dir = discover_git_dir(&cwd)?;
    let format = repository_object_format(&git_dir)?;
    let db = FileObjectDatabase::from_git_dir(&git_dir, format);
    if !diff_relative_explicit
        && let Ok(config) = read_repo_config(&git_dir)
        && config.get_bool("diff", None, "relative").unwrap_or(false)
    {
        diff_relative = commands::diff_options::DiffRelativeMode::Cwd;
    }
    if !renames_explicit
        && let Ok(config) = read_repo_config(&git_dir)
        && let Some(value) = config.get("diff", None, "renames")
    {
        match value.trim().to_ascii_lowercase().as_str() {
            "false" | "no" | "off" | "0" => {
                detect_renames = false;
                detect_copies = false;
                inexact_renames = false;
            }
            "copies" | "copy" => {
                detect_renames = true;
                detect_copies = true;
                inexact_renames = true;
            }
            "true" | "yes" | "on" | "1" | "renames" => {
                detect_renames = true;
                inexact_renames = true;
            }
            _ => {}
        }
    }
    if let Some(opts) = dirstat.as_mut() {
        // diff.dirstat config forms the base (bad parameters warn); explicit
        // --dirstat parameters apply on top (bad parameters are fatal).
        let mut base = DirstatOptions::default();
        if let Ok(config) = read_repo_config(&git_dir)
            && let Some(value) = config.get("diff", None, "dirstat")
        {
            let mut errors = String::new();
            if parse_dirstat_params(value, &mut base, &mut errors) > 0 {
                eprint!("warning: Found errors in 'diff.dirstat' config variable:\n{errors}");
            }
        }
        // Flags parsed inline (--cumulative / --dirstat-by-file) already
        // modified `opts`; merge them onto the config base.
        if opts.cumulative {
            base.cumulative = true;
        }
        if opts.mode == DirstatMode::Files {
            base.mode = DirstatMode::Files;
        }
        let mut errors = String::new();
        let mut error_count = 0usize;
        for params in &dirstat_cli_params {
            error_count += parse_dirstat_params(params, &mut base, &mut errors);
        }
        if error_count > 0 {
            eprint!("fatal: Failed to parse --dirstat/-X option parameter:\n{errors}");
            return Err(GitError::Exit(128));
        }
        *opts = base;
    }
    // Pull any leading `<rev>` / `<rev> <rev>` / `<rev>..<rev>` / `<rev>...<rev>`
    // out of the positional arguments; the remainder are pathspecs. Without this,
    // `diff A B` was treated as two paths and silently fell back to an
    // index-vs-worktree diff (wrong output, and a full-worktree rescan on big
    // repos).
    // A bare `diff HEAD` keeps its dedicated head-vs-worktree path, but
    // `diff HEAD <rev>` / `diff HEAD HEAD` means the consumed HEAD is the first of
    // several revisions — hand it back to the splitter.
    if head && !path_args.is_empty() {
        path_args.insert(0, "HEAD".to_string());
        head = false;
    }
    let (diff_trees, mut path_args) = diff_split_revisions(&git_dir, format, &db, path_args)?;
    path_args.extend(explicit_paths);
    let find_objects = resolve_diff_find_objects(&git_dir, format, &find_object_values)?;
    let no_output_mode = !raw
        && !stat
        && !compact_summary
        && !numstat
        && !shortstat
        && !summary
        && dirstat.is_none()
        && !name_status
        && !name_only;
    let output_may_show_oids = !quiet && !no_patch && !name_only && !name_status;
    let needs_raw_abbrev = output_may_show_oids && raw && raw_abbrev.is_none();
    let needs_patch_abbrev = output_may_show_oids
        && (patch || no_output_mode)
        && !patch_full_index
        && patch_abbrev.is_none();
    let repository_abbrev = if needs_raw_abbrev || needs_patch_abbrev {
        repository_abbrev(&git_dir, format)?
    } else {
        None
    };
    let raw_abbrev = match raw_abbrev {
        Some(abbrev) => abbrev.map(|width| width.min(format.hex_len())),
        // `git diff` is porcelain: raw oids abbreviate by default (unlike the
        // diff-tree plumbing), to core.abbrev or git's standard 7.
        None => Some(repository_abbrev.unwrap_or(7).min(format.hex_len())),
    };
    let patch_abbrev = if patch_full_index {
        format.hex_len()
    } else {
        patch_abbrev
            .or(repository_abbrev)
            .unwrap_or(7)
            .min(format.hex_len())
    };
    let worktree_root = if cached {
        None
    } else {
        Some(worktree_root_for_git_dir(&git_dir)?)
    };
    let pathspec = if path_args.is_empty() {
        DiffPathspec::default()
    } else {
        let worktree_root = worktree_root_for_git_dir(&git_dir)?;
        DiffPathspec::new(&cwd, &worktree_root, &path_args)?
    };
    let name_status_options = sley_diff_merge::DiffNameStatusOptions {
        detect_renames,
        detect_copies,
        find_copies_harder,
        rename_empty,
    };
    // The new-side oid is real (shown, not zeroed) when it comes from a tree or the
    // index; it is zeroed only when the new side is the worktree.
    let zero_worktree_oids = match diff_trees.len() {
        2 => false,
        1 => !cached,
        _ => !cached && !head,
    };
    let plain_index_worktree_diff = diff_trees.is_empty() && !cached && !head;
    // The new side's *content* comes from the worktree only when there is no second
    // tree and we're not diffing the index (`--cached`). A two-tree `diff A B` takes
    // its new content from tree B's blobs, never the worktree.
    let use_worktree_new = !cached && diff_trees.len() != 2;
    let rename_options = sley_diff_merge::RenameDetectionOptions {
        base: name_status_options,
        detect_inexact: true,
        rename_threshold,
        copy_threshold,
    };
    let entries = if !diff_trees.is_empty() {
        match diff_trees.as_slice() {
            // `diff <rev>`: that tree vs the worktree (or the index with --cached).
            [tree] => {
                if cached {
                    if inexact_renames {
                        sley_diff_merge::diff_name_status_tree_index_with_rename_options(
                            &git_dir,
                            format,
                            tree,
                            rename_options,
                        )?
                    } else {
                        sley_diff_merge::diff_name_status_tree_index_with_options(
                            &git_dir,
                            format,
                            tree,
                            name_status_options,
                        )?
                    }
                } else {
                    let worktree_root = worktree_root
                        .as_ref()
                        .expect("worktree root set for diff <rev>");
                    if inexact_renames {
                        sley_diff_merge::diff_name_status_tree_worktree_with_rename_options(
                            worktree_root,
                            &git_dir,
                            format,
                            tree,
                            rename_options,
                        )?
                    } else {
                        sley_diff_merge::diff_name_status_tree_worktree_with_options(
                            worktree_root,
                            &git_dir,
                            format,
                            tree,
                            name_status_options,
                        )?
                    }
                }
            }
            // `diff <rev> <rev>` / `<rev>..<rev>` / `<rev>...<rev>`: tree vs tree.
            [left, right] => {
                if inexact_renames {
                    sley_diff_merge::diff_name_status_trees_with_rename_options(
                        &db,
                        format,
                        left,
                        right,
                        rename_options,
                    )?
                } else {
                    sley_diff_merge::diff_name_status_trees_with_options(
                        &db,
                        format,
                        left,
                        right,
                        name_status_options,
                    )?
                }
            }
            _ => {
                return Err(GitError::Unsupported(
                    "diff accepts at most two revisions".into(),
                ));
            }
        }
    } else if cached {
        if inexact_renames {
            sley_diff_merge::diff_name_status_head_index_with_rename_options(
                &git_dir,
                format,
                rename_options,
            )?
        } else {
            sley_diff_merge::diff_name_status_head_index_with_options(
                &git_dir,
                format,
                name_status_options,
            )?
        }
    } else if head {
        let worktree_root = worktree_root
            .as_ref()
            .expect("worktree root set for diff HEAD");
        if inexact_renames {
            sley_diff_merge::diff_name_status_head_worktree_with_rename_options(
                worktree_root,
                &git_dir,
                format,
                rename_options,
            )?
        } else {
            sley_diff_merge::diff_name_status_head_worktree_with_options(
                worktree_root,
                &git_dir,
                format,
                name_status_options,
            )?
        }
    } else {
        let worktree_root = worktree_root.as_ref().expect("worktree root set for diff");
        if inexact_renames {
            sley_diff_merge::diff_name_status_index_worktree_with_rename_options(
                worktree_root,
                &git_dir,
                format,
                rename_options,
            )?
        } else {
            sley_diff_merge::diff_name_status_index_worktree_with_options(
                worktree_root,
                &git_dir,
                format,
                name_status_options,
            )?
        }
    };
    // Submodule-ignore handling: drop `all`-ignored gitlink entries, then for
    // worktree-involved diffs collect each staged submodule's dirt (for the
    // `-dirty` patch suffix) and append dirty-but-same-commit pairs the map
    // comparison alone cannot see.
    let submodule_config =
        submodule_diff_config(&git_dir, worktree_root.as_deref(), ignore_submodules_cli);
    let mut entries = apply_submodule_ignore_filter(entries, &submodule_config);
    let dirty_submodules = match (use_worktree_new, worktree_root.as_deref()) {
        (true, Some(root)) => {
            collect_dirty_submodules(&mut entries, &git_dir, format, root, &submodule_config)?
        }
        _ => HashSet::new(),
    };
    let entries = apply_diff_pathspec(entries, &pathspec);
    let entries = if let Some(needle) = pickaxe.as_deref() {
        apply_diff_pickaxe(
            entries,
            needle.as_bytes(),
            pickaxe_all,
            &db,
            worktree_root.as_deref(),
            use_worktree_new,
        )?
    } else if pickaxe_all || pickaxe_regex {
        sort_diff_entries_by_path(entries)
    } else {
        entries
    };
    let entries = apply_diff_find_objects(entries, &find_objects);
    let entries = if reverse {
        reverse_diff_entries(entries)
    } else {
        entries
    };
    // `--relative` rewrites the displayed paths; worktree content reads must
    // keep resolving against the original location, so the effective worktree
    // root gains the stripped prefix.
    let mut worktree_root = worktree_root;
    let entries = if matches!(diff_relative, commands::diff_options::DiffRelativeMode::Off) {
        entries
    } else {
        let prefix = diff_relative_prefix(&diff_relative, &cwd, &git_dir)?;
        if !prefix.is_empty()
            && let Some(root) = worktree_root.as_mut()
        {
            root.push(repo_path_to_path(&prefix));
        }
        apply_diff_relative(entries, &prefix)
    };
    let entries: Vec<_> = if diff_filter.all_or_none {
        if !diff_filter.includes.is_empty()
            && entries.iter().any(|entry| {
                pathspec.matches(&entry.path) && diff_filter.matches_status(entry.status.code())
            })
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
    let has_differences = !entries.is_empty();
    if !quiet && !no_patch {
        let mut stdout = io::stdout();
        let show_raw = raw && !name_only && !name_status;
        let show_numstat = numstat && !name_only && !name_status;
        let show_stat = (stat || compact_summary) && !name_only && !name_status;
        let show_shortstat = shortstat && !name_only && !name_status;
        let show_patch = !name_only && !name_status && (patch || no_output_mode);
        let show_summary = summary && !name_only && !name_status;
        if show_raw {
            // git zeroes the worktree-side oid only when it cannot be trusted:
            // a stat-clean file keeps its index oid in raw output. The
            // worktree entries carry the freshly-hashed content oid, so
            // matching it against the index entry reproduces that rule.
            let zero_all_worktree_oids = zero_worktree_oids && plain_index_worktree_diff;
            let needs_index_oids = zero_worktree_oids
                && !zero_all_worktree_oids
                && entries.iter().any(|entry| entry.new_oid.is_some());
            let index_oids: HashMap<Vec<u8>, ObjectId> = if needs_index_oids {
                let index_path = sley_worktree::repository_index_path(&git_dir);
                match fs::read(&index_path) {
                    Ok(bytes) => Index::parse(&bytes, format)?
                        .entries
                        .into_iter()
                        .map(|entry| (entry.path.to_vec(), entry.oid))
                        .collect(),
                    Err(_) => HashMap::new(),
                }
            } else {
                HashMap::new()
            };
            for entry in &entries {
                let zero_entry = zero_all_worktree_oids
                    || (zero_worktree_oids
                        && entry
                            .new_oid
                            .as_ref()
                            .is_none_or(|oid| index_oids.get(&entry.path[..]) != Some(oid)));
                write_diff_raw_entry(&mut stdout, entry, z, zero_entry, raw_abbrev, format)?;
            }
        }
        if show_numstat {
            for entry in &entries {
                write_diff_numstat_entry(
                    &mut stdout,
                    entry,
                    z,
                    &db,
                    worktree_root.as_deref(),
                    use_worktree_new,
                )?;
            }
        }
        if show_stat {
            let mut stat_widths = stat_widths;
            if let Ok(config) = read_repo_config(&git_dir) {
                stat_widths.resolve_config(&config);
            } else {
                stat_widths.resolve_config_defaults();
            }
            write_diff_stat_with_widths(
                &mut stdout,
                &entries,
                &db,
                worktree_root.as_deref(),
                use_worktree_new,
                DiffStatOptions {
                    compact_summary,
                    stat_count,
                    color: color_always,
                },
                stat_widths,
            )?;
        }
        if show_shortstat {
            write_diff_shortstat(
                &mut stdout,
                &entries,
                &db,
                worktree_root.as_deref(),
                use_worktree_new,
            )?;
        }
        if let Some(dirstat_options) = dirstat
            && !name_only
            && !name_status
        {
            write_diff_dirstat(
                &mut stdout,
                &entries,
                &db,
                worktree_root.as_deref(),
                use_worktree_new,
                dirstat_options,
            )?;
        }
        if show_summary {
            for entry in &entries {
                write_diff_summary_entry(&mut stdout, entry)?;
            }
        }
        if show_patch {
            if show_raw || show_numstat || show_stat || show_shortstat || show_summary {
                writeln!(stdout)?;
            }
            let colors = color_always.then(|| {
                commands::diff_words::DiffColors::enabled(read_repo_config(&git_dir).ok().as_ref())
            });
            let word_request = word_diff_mode.map(|mode| WordDiffRequest {
                mode,
                cli_regex: word_diff_regex.as_deref(),
            });
            // Userdiff driver resolution (`diff=<driver>` attributes +
            // `diff.<name>.*` config) for hunk headings. Attributes always come
            // from the real worktree, even when the content comparison is
            // `--cached`.
            let userdiff_attributes = worktree_root_for_git_dir(&git_dir)
                .ok()
                .map(sley_worktree::StandardAttributeMatcher::from_worktree_root)
                .transpose()?;
            let userdiff = commands::userdiff::UserdiffResolver::with_attributes(
                userdiff_attributes,
                read_repo_config(&git_dir).ok(),
            );
            for entry in &entries {
                let options = DiffPatchOptions {
                    db: &db,
                    worktree_root: worktree_root.as_deref(),
                    use_worktree_new,
                    format,
                    abbrev: patch_abbrev,
                    src_prefix: &src_prefix,
                    dst_prefix: &dst_prefix,
                    context: context.unwrap_or(3),
                    userdiff: Some(&userdiff),
                    colors: colors.as_ref(),
                    word_diff: word_request.as_ref(),
                    no_index_contents: None,
                    dirty_submodules: Some(&dirty_submodules),
                };
                write_diff_patch_entry(&mut stdout, entry, options)?;
            }
        } else if !show_summary
            && (summary || (!show_stat && !show_shortstat))
            && !show_numstat
            && !show_raw
            && dirstat.is_none()
        {
            for entry in &entries {
                if z && (name_only || name_status) {
                    if name_only {
                        stdout.write_all(&entry.path)?;
                        stdout.write_all(b"\0")?;
                    } else {
                        stdout.write_all(entry.status.label().as_bytes())?;
                        stdout.write_all(b"\0")?;
                        if let Some(old_path) = &entry.old_path {
                            stdout.write_all(old_path)?;
                            stdout.write_all(b"\0")?;
                        }
                        stdout.write_all(&entry.path)?;
                        stdout.write_all(b"\0")?;
                    }
                } else if name_only {
                    let path = status_quote_path(&entry.path, false);
                    writeln!(stdout, "{path}")?;
                } else if !name_status && summary {
                    write_diff_summary_entry(&mut stdout, entry)?;
                } else {
                    write!(stdout, "{}", entry.status.label())?;
                    if let Some(old_path) = &entry.old_path {
                        let old_path = status_quote_path(old_path, false);
                        write!(stdout, "\t{old_path}")?;
                    }
                    let path = status_quote_path(&entry.path, false);
                    writeln!(stdout, "\t{path}")?;
                }
            }
        }
    }
    if (quiet || exit_code) && has_differences {
        return Err(GitError::Exit(1));
    }
    Ok(())
}

fn apply_diff_pickaxe(
    entries: Vec<sley_diff_merge::NameStatusEntry>,
    needle: &[u8],
    pickaxe_all: bool,
    db: &FileObjectDatabase,
    worktree_root: Option<&Path>,
    use_worktree_new: bool,
) -> Result<Vec<sley_diff_merge::NameStatusEntry>> {
    if needle.is_empty() {
        return Ok(Vec::new());
    }
    let mut matches = Vec::new();
    for entry in &entries {
        if diff_entry_matches_pickaxe(entry, needle, db, worktree_root, use_worktree_new)? {
            matches.push(entry.clone());
        }
    }
    if pickaxe_all {
        if matches.is_empty() {
            Ok(Vec::new())
        } else {
            Ok(sort_diff_entries_by_path(entries))
        }
    } else {
        Ok(sort_diff_entries_by_path(matches))
    }
}

fn diff_entry_matches_pickaxe(
    entry: &sley_diff_merge::NameStatusEntry,
    needle: &[u8],
    db: &FileObjectDatabase,
    worktree_root: Option<&Path>,
    use_worktree_new: bool,
) -> Result<bool> {
    let old_content = diff_entry_old_content(entry, db)?;
    let new_content = diff_entry_new_content(entry, db, worktree_root, use_worktree_new)?;
    Ok(
        count_non_overlapping_occurrences(old_content.as_deref().unwrap_or_default(), needle)
            != count_non_overlapping_occurrences(
                new_content.as_deref().unwrap_or_default(),
                needle,
            ),
    )
}

fn resolve_diff_find_objects(
    git_dir: &Path,
    format: ObjectFormat,
    values: &[String],
) -> Result<Vec<ObjectId>> {
    values
        .iter()
        .map(|value| resolve_diff_find_object(git_dir, format, value))
        .collect()
}

fn resolve_diff_find_object(git_dir: &Path, format: ObjectFormat, value: &str) -> Result<ObjectId> {
    if value.len() == format.hex_len() && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return ObjectId::from_hex(format, value)
            .map_err(|_| diff_find_object_unable_to_resolve_error(value));
    }
    resolve_revision(git_dir, format, value)
        .map_err(|_| diff_find_object_unable_to_resolve_error(value))
}

fn apply_diff_find_objects(
    entries: Vec<sley_diff_merge::NameStatusEntry>,
    targets: &[ObjectId],
) -> Vec<sley_diff_merge::NameStatusEntry> {
    if targets.is_empty() {
        return entries;
    }
    sort_diff_entries_by_path(
        entries
            .into_iter()
            .filter(|entry| {
                targets.iter().any(|target| {
                    entry.old_oid.as_ref() == Some(target) || entry.new_oid.as_ref() == Some(target)
                })
            })
            .collect(),
    )
}

fn count_non_overlapping_occurrences(haystack: &[u8], needle: &[u8]) -> usize {
    if needle.is_empty() {
        return 0;
    }
    let mut count = 0;
    let mut offset = 0;
    while offset + needle.len() <= haystack.len() {
        if &haystack[offset..offset + needle.len()] == needle {
            count += 1;
            offset += needle.len();
        } else {
            offset += 1;
        }
    }
    count
}

fn sort_diff_entries_by_path(
    mut entries: Vec<sley_diff_merge::NameStatusEntry>,
) -> Vec<sley_diff_merge::NameStatusEntry> {
    entries.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.old_path.cmp(&right.old_path))
            .then_with(|| left.status.code().cmp(&right.status.code()))
    });
    entries
}

fn diff_find_object_unable_to_resolve_error(value: &str) -> GitError {
    eprintln!("error: unable to resolve '{value}'");
    GitError::Exit(129)
}

fn diff_find_object_pickaxe_all_conflict_error() -> Result<()> {
    eprintln!(
        "fatal: options '--pickaxe-all' and '--find-object' cannot be used together, use '--pickaxe-all' with '-G' and '-S'"
    );
    Err(GitError::Exit(128))
}

fn diff_relative_prefix(
    mode: &commands::diff_options::DiffRelativeMode,
    cwd: &Path,
    git_dir: &Path,
) -> Result<Vec<u8>> {
    match mode {
        commands::diff_options::DiffRelativeMode::Off => Ok(Vec::new()),
        commands::diff_options::DiffRelativeMode::Cwd => Ok(worktree_prefix(cwd, git_dir)?
            .trim_end_matches('/')
            .as_bytes()
            .to_vec()),
        commands::diff_options::DiffRelativeMode::Prefix(prefix) => {
            Ok(diff_relative_prefix_arg(prefix).into_bytes())
        }
    }
}

fn diff_relative_prefix_arg(prefix: &str) -> String {
    if prefix.starts_with('/') {
        prefix.to_string()
    } else {
        prefix.trim_end_matches('/').to_string()
    }
}

fn apply_diff_relative(
    entries: Vec<sley_diff_merge::NameStatusEntry>,
    prefix: &[u8],
) -> Vec<sley_diff_merge::NameStatusEntry> {
    let mut filtered = Vec::new();
    for entry in entries {
        if let Some(old_path) = &entry.old_path {
            let old_display = diff_relative_display_path(old_path, prefix);
            let new_display = diff_relative_display_path(&entry.path, prefix);
            if matches!(entry.status, sley_diff_merge::NameStatus::Copied(_)) {
                match (old_display, new_display) {
                    (Some(old_path), Some(path)) => {
                        filtered.push(sley_diff_merge::NameStatusEntry {
                            path: BString::from(path),
                            old_path: Some(BString::from(old_path)),
                            ..entry
                        })
                    }
                    (None, Some(path)) => filtered.push(sley_diff_merge::NameStatusEntry {
                        status: sley_diff_merge::NameStatus::Added,
                        path: BString::from(path),
                        old_path: None,
                        old_mode: None,
                        new_mode: entry.new_mode,
                        old_oid: None,
                        new_oid: entry.new_oid,
                    }),
                    (Some(_), None) | (None, None) => {}
                }
            } else {
                match (old_display, new_display) {
                    (Some(old_path), Some(path)) => {
                        filtered.push(sley_diff_merge::NameStatusEntry {
                            path: BString::from(path),
                            old_path: Some(BString::from(old_path)),
                            ..entry
                        });
                    }
                    (Some(path), None) => filtered.push(sley_diff_merge::NameStatusEntry {
                        status: sley_diff_merge::NameStatus::Deleted,
                        path: BString::from(path),
                        old_path: None,
                        old_mode: entry.old_mode,
                        new_mode: None,
                        old_oid: entry.old_oid,
                        new_oid: None,
                    }),
                    (None, Some(path)) => filtered.push(sley_diff_merge::NameStatusEntry {
                        status: sley_diff_merge::NameStatus::Added,
                        path: BString::from(path),
                        old_path: None,
                        old_mode: None,
                        new_mode: entry.new_mode,
                        old_oid: None,
                        new_oid: entry.new_oid,
                    }),
                    (None, None) => {}
                }
            }
        } else if let Some(path) = diff_relative_display_path(&entry.path, prefix) {
            filtered.push(sley_diff_merge::NameStatusEntry {
                path: BString::from(path),
                ..entry
            });
        }
    }
    filtered.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.old_path.cmp(&right.old_path))
            .then_with(|| left.status.code().cmp(&right.status.code()))
    });
    filtered
}

fn diff_relative_display_path(path: &[u8], prefix: &[u8]) -> Option<Vec<u8>> {
    if prefix.is_empty() {
        return Some(path.to_vec());
    }
    if path == prefix {
        return Some(Vec::new());
    }
    // git matches the prefix as a plain string (`--relative=sub` turns
    // `subdir/file2` into `dir/file2`), swallowing one separating slash when
    // the prefix happens to end on a path-component boundary.
    path.strip_prefix(prefix)
        .map(|rest| rest.strip_prefix(b"/").unwrap_or(rest).to_vec())
}

/// Parameters for `git diff --no-index`.
struct DiffNoIndexParams<'a> {
    context: usize,
    color: bool,
    word_diff_mode: Option<commands::diff_words::WordDiffMode>,
    word_diff_regex: Option<&'a str>,
    src_prefix: &'a str,
    dst_prefix: &'a str,
    quiet: bool,
}

/// `git diff --no-index <path> <path>`: compare two files outside (or beside)
/// the object database. Attributes and `diff.*` config still apply when the
/// command runs inside a repository. Exits 1 when the files differ.
fn cmd_diff_no_index(cwd: &Path, paths: &[String], params: DiffNoIndexParams<'_>) -> Result<()> {
    if paths.len() != 2 {
        eprintln!("usage: git diff --no-index [<options>] <path> <path>");
        return Err(GitError::Exit(129));
    }
    let read_side = |spec: &str| -> Result<(Vec<u8>, u32)> {
        let path = Path::new(spec);
        let content = fs::read(path).map_err(|_| {
            eprintln!("error: Could not access '{spec}'");
            GitError::Exit(1)
        })?;
        let mode = {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let permissions = fs::metadata(path).map(|meta| meta.permissions().mode());
                if permissions.is_ok_and(|bits| bits & 0o111 != 0) {
                    0o100755
                } else {
                    0o100644
                }
            }
            #[cfg(not(unix))]
            {
                0o100644
            }
        };
        Ok((content, mode))
    };
    let (old_content, old_mode) = read_side(&paths[0])?;
    let (new_content, new_mode) = read_side(&paths[1])?;
    if old_content == new_content && old_mode == new_mode {
        return Ok(());
    }
    // Repository context is optional: when present, .gitattributes drivers,
    // diff.<name>.* config, and color overrides all apply.
    let git_dir = discover_git_dir(cwd).ok();
    let config = git_dir
        .as_deref()
        .and_then(|dir| read_repo_config(dir).ok());
    let worktree_root = git_dir
        .as_deref()
        .and_then(|dir| worktree_root_for_git_dir(dir).ok());
    let colors = params
        .color
        .then(|| commands::diff_words::DiffColors::enabled(config.as_ref()));
    let word_request = params.word_diff_mode.map(|mode| WordDiffRequest {
        mode,
        cli_regex: params.word_diff_regex,
    });
    let entry = sley_diff_merge::NameStatusEntry {
        status: sley_diff_merge::NameStatus::Modified,
        path: BString::from(paths[1].as_bytes()),
        old_path: Some(BString::from(paths[0].as_bytes())),
        old_mode: Some(old_mode),
        new_mode: Some(new_mode),
        old_oid: None,
        new_oid: None,
    };
    // A throwaway object database handle: content reads are overridden, so it
    // is never consulted.
    let scratch_git_dir = git_dir.clone().unwrap_or_else(|| cwd.to_path_buf());
    let db = FileObjectDatabase::from_git_dir(&scratch_git_dir, ObjectFormat::Sha1);
    if !params.quiet {
        let mut stdout = io::stdout();
        let userdiff_attributes = worktree_root
            .map(sley_worktree::StandardAttributeMatcher::from_worktree_root)
            .transpose()?;
        let userdiff = commands::userdiff::UserdiffResolver::with_attributes(
            userdiff_attributes,
            config.clone(),
        );
        let options = DiffPatchOptions {
            db: &db,
            worktree_root: None,
            use_worktree_new: false,
            format: ObjectFormat::Sha1,
            abbrev: 7,
            src_prefix: params.src_prefix,
            dst_prefix: params.dst_prefix,
            context: params.context,
            userdiff: Some(&userdiff),
            colors: colors.as_ref(),
            word_diff: word_request.as_ref(),
            no_index_contents: Some((Some(&old_content), Some(&new_content))),
            dirty_submodules: None,
        };
        write_diff_patch_entry(&mut stdout, &entry, options)?;
    }
    Err(GitError::Exit(1))
}
