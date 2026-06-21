//! Extracted from the crate root (sley#8 phase 1) — code motion only.

// A glob of the crate root brings every shared helper/type into scope via
// descendant-privacy; see commands::stash for the rationale.
use crate::*;
use sley_pathspec::normalized_revwalk_pathspec;

pub(crate) fn cmd_rev_list(args: &[String]) -> Result<()> {
    let mut setup_args = Vec::new();
    let mut parents = false;
    let mut children = false;
    // `--use-mailmap`/`--mailmap` (and `--no-` forms). rev-list, unlike `log`,
    // has no `log.mailmap` default — mapping is off unless explicitly requested.
    let mut use_mailmap = false;
    let mut count = false;
    let mut min_parents = None;
    let mut max_parents = None;
    let mut abbrev_commit = false;
    let mut abbrev_len = Some(7usize);
    let mut left_right = false;
    let mut side_filter = None;
    let mut cherry_mode = RevListCherryMode::None;
    let mut timestamp = false;
    let mut quiet = false;
    let mut nul_terminated = false;
    let mut objects = false;
    let mut objects_edge = false;
    let mut object_filter = RevListObjectFilter::None;
    let mut filter_print_omitted = false;
    let mut filter_provided_objects = false;
    let mut missing_action = RevListMissingAction::Error;
    let mut boundary = false;
    let mut disk_usage = None;
    let mut object_names = true;
    let mut verify_objects = false;
    let mut read_stdin = false;
    let mut header = false;
    // `--no-commit-header` / `--commit-header`: override whether `--format=` /
    // `--pretty=format:` prints the `commit <oid>` header line. Applied after the
    // format is parsed (order-independent).
    let mut commit_header_override: Option<bool> = None;
    // `--color` / `--color=always|auto|never`: enable ANSI color atoms (`%Cred`,
    // `%C(...)`). We are never a tty, so `auto` resolves to off (matching git's
    // `want_color`).
    let mut want_color = false;
    let mut pretty = RevListPretty::Default;
    let mut preset_oneline = false;
    let mut author_patterns = Vec::new();
    let mut committer_patterns = Vec::new();
    let mut grep_patterns = Vec::new();
    let mut grep_all_match = false;
    let mut invert_grep = false;
    let mut regexp_ignore_case = false;
    let mut regexp_mode = SimpleLogRegexMode::Basic;
    let mut date_mode = DateMode::Default;
    let mut use_bitmap_index = false;
    let mut test_bitmap = false;
    let mut unpacked = false;
    let mut setup_not = false;
    // Bisection plumbing (`--bisect[-vars|-all]`): `bisect` selects the
    // weighted-midpoint output mode, `bisect_vars` prints the `bisect_*=`
    // block, `bisect_all` lists every candidate by distance. Only the literal
    // `--bisect` form injects the default `refs/bisect/*` revisions (git wires
    // it into `setup_revisions`); `--bisect-vars`/`--bisect-all` are consumed
    // by `builtin/rev-list.c` after setup and do not.
    let mut bisect = false;
    let mut bisect_inject_refs = false;
    let mut bisect_vars = false;
    let mut bisect_all = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--" => {
                setup_args.push(arg.clone());
                setup_args.extend(iter.cloned());
                break;
            }
            "--not" => {
                setup_not = !setup_not;
                setup_args.push(arg.clone());
            }
            "--full-history"
            | "--reverse"
            | "--first-parent"
            | "--no-first-parent"
            | "--topo-order"
            | "--date-order"
            | "--author-date-order"
            | "--no-walk"
            | "--no-walk=sorted"
            | "--no-walk=unsorted"
            | "--do-walk"
            | "--all"
            | "--no-all"
            | "--branches"
            | "--tags"
            | "--remotes"
            | "--ignore-missing"
            | "--no-ignore-missing" => setup_args.push(arg.clone()),
            "--default" | "-n" | "--max-count" | "--skip" | "--max-age" | "--min-age"
            | "--since" | "--after" | "--until" | "--before" | "--glob" | "--exclude"
            | "--exclude-hidden" => {
                setup_args.push(arg.clone());
                setup_args.push(
                    iter.next()
                        .ok_or_else(|| GitError::Command(format!("{arg} requires a value")))?
                        .clone(),
                );
            }
            value
                if value.starts_with("--max-count=")
                    || value.starts_with("--skip=")
                    || value.starts_with("--max-age=")
                    || value.starts_with("--min-age=")
                    || value.starts_with("--since=")
                    || value.starts_with("--after=")
                    || value.starts_with("--until=")
                    || value.starts_with("--before=")
                    || value.starts_with("--glob=")
                    || value.starts_with("--exclude=")
                    || value.starts_with("--exclude-hidden=")
                    || value.starts_with("--branches=")
                    || value.starts_with("--tags=")
                    || value.starts_with("--remotes=")
                    || (value.starts_with("-n") && value.len() > 2)
                    || (value.starts_with('-')
                        && value.len() > 1
                        && value[1..].bytes().all(|byte| byte.is_ascii_digit())) =>
            {
                setup_args.push(arg.clone());
            }
            "--parents" => parents = true,
            "--no-parents" => parents = false,
            "--use-mailmap" | "--mailmap" => use_mailmap = true,
            "--no-use-mailmap" | "--no-mailmap" => use_mailmap = false,
            "--children" => children = true,
            "--count" => count = true,
            "--no-count" => count = false,
            // History-simplification flags handled by the sley-rev setup +
            // `simplify_history`; forward them so they take effect.
            "--simplify-merges" | "--show-pulls" | "--ancestry-path" => {
                setup_args.push(arg.clone())
            }
            "--sparse" | "--dense" | "--remove-empty" | "--exclude-promisor-objects" => {}
            // No effect on the regular walk yet (pre-existing behaviour); the
            // bitmap path filters packed objects out of its result.
            "--unpacked" => unpacked = true,
            // Bisection modes. `--bisect` additionally injects the default
            // `refs/bisect/{bad,good-*}` refs as revisions when none are given,
            // matching git's setup_revisions; `--bisect-all` turns on
            // decorations for the `dist=N` annotation.
            "--bisect" => {
                bisect = true;
                bisect_inject_refs = true;
            }
            "--bisect-vars" => {
                bisect = true;
                bisect_vars = true;
            }
            "--bisect-all" => {
                bisect = true;
                bisect_all = true;
            }
            "--abbrev-commit" => abbrev_commit = true,
            "--no-abbrev-commit" => abbrev_commit = false,
            "--no-abbrev" => abbrev_len = None,
            "--left-right" => left_right = true,
            "--left-only" => side_filter = Some('<'),
            "--right-only" => side_filter = Some('>'),
            "--cherry-pick" => cherry_mode = RevListCherryMode::Pick,
            "--cherry-mark" => cherry_mode = RevListCherryMode::Mark,
            "--cherry" => {
                cherry_mode = RevListCherryMode::Mark;
                side_filter = Some('>');
                max_parents = Some(1);
            }
            "--timestamp" => timestamp = true,
            "--quiet" => quiet = true,
            "--author" => {
                let value = iter.next().ok_or_else(log_author_requires_value_error)?;
                author_patterns.push(LogFilterPattern::new(value, "header"));
            }
            value if value.starts_with("--author=") => {
                author_patterns.push(LogFilterPattern::new(&value["--author=".len()..], "header"));
            }
            "--committer" => {
                let value = iter.next().ok_or_else(log_committer_requires_value_error)?;
                committer_patterns.push(LogFilterPattern::new(value, "header"));
            }
            value if value.starts_with("--committer=") => {
                committer_patterns.push(LogFilterPattern::new(
                    &value["--committer=".len()..],
                    "header",
                ));
            }
            "--grep" => {
                let value = iter.next().ok_or_else(log_grep_requires_value_error)?;
                grep_patterns.push(LogFilterPattern::new(value, "command line"));
            }
            value if value.starts_with("--grep=") => {
                grep_patterns.push(LogFilterPattern::new(
                    &value["--grep=".len()..],
                    "command line",
                ));
            }
            "--all-match" => grep_all_match = true,
            "--invert-grep" => invert_grep = true,
            "-i" | "--regexp-ignore-case" => regexp_ignore_case = true,
            "-F" | "--fixed-strings" => regexp_mode = SimpleLogRegexMode::Fixed,
            "-E" | "--basic-regexp" | "--extended-regexp" => {
                regexp_mode = SimpleLogRegexMode::Basic
            }
            "-z" => nul_terminated = true,
            "--use-bitmap-index" => use_bitmap_index = true,
            "--test-bitmap" => test_bitmap = true,
            "--objects" => objects = true,
            "--verify-objects" => verify_objects = true,
            "--objects-edge" => {
                objects = true;
                objects_edge = true;
            }
            "--objects-edge-aggressive" => {
                objects = true;
                objects_edge = true;
            }
            "--no-filter" => object_filter = RevListObjectFilter::None,
            "--filter-print-omitted" => filter_print_omitted = true,
            // Apply the object filter to the directly-provided tip objects too, not just the
            // objects reached by the walk. For an `object:type` filter this means a provided
            // commit tip is itself dropped when it is not the requested type.
            "--filter-provided-objects" => filter_provided_objects = true,
            value if value.starts_with("--filter=") => {
                let parsed = RevListObjectFilter::parse(&value["--filter=".len()..])?;
                object_filter = object_filter.combine_with(parsed);
            }
            "--missing=print" => missing_action = RevListMissingAction::Print,
            "--missing=print-info" => missing_action = RevListMissingAction::PrintInfo,
            "--missing=allow-any" => missing_action = RevListMissingAction::AllowAny,
            "--missing=allow-promisor" => missing_action = RevListMissingAction::AllowPromisor,
            "--boundary" => boundary = true,
            "--disk-usage" => disk_usage = Some(false),
            "--disk-usage=human" => disk_usage = Some(true),
            value if value.starts_with("--disk-usage=") => {
                return Err(GitError::Command(format!(
                    "invalid rev-list disk-usage format {}",
                    &value["--disk-usage=".len()..]
                )));
            }
            "--object-names" => object_names = true,
            "--no-object-names" => object_names = false,
            "--stdin" => read_stdin = true,
            "--header" => header = true,
            "--no-commit-header" => commit_header_override = Some(false),
            "--commit-header" => commit_header_override = Some(true),
            "--color" => want_color = true,
            "--no-color" => want_color = false,
            value if value.starts_with("--color=") => {
                // `always` forces color; `auto`/`never` are off when not a tty.
                want_color = value["--color=".len()..].eq_ignore_ascii_case("always");
            }
            "--oneline" => {
                preset_oneline = true;
                abbrev_commit = true;
            }
            "--pretty=oneline" | "--format=oneline" => preset_oneline = true,
            "--pretty=short" | "--format=short" => pretty = RevListPretty::Short,
            value if value.starts_with("--format=") => {
                pretty = RevListPretty::Compiled {
                    compiled: CompiledLogFormat::compile(
                        &value["--format=".len()..],
                        LogFormatDialect::RevList,
                    )?,
                    commit_header: true,
                };
            }
            value if value.starts_with("--pretty=format:") => {
                pretty = RevListPretty::Compiled {
                    compiled: CompiledLogFormat::compile(
                        &value["--pretty=format:".len()..],
                        LogFormatDialect::RevList,
                    )?,
                    commit_header: true,
                };
            }
            value if value.starts_with("--abbrev=") => {
                let value = value
                    .strip_prefix("--abbrev=")
                    .ok_or_else(|| GitError::Command("--abbrev requires a value".into()))?;
                abbrev_len = Some(parse_rev_list_abbrev(value)?);
            }
            "--date" => {
                let value = iter.next().ok_or_else(log_date_requires_value_error)?;
                date_mode = log_date_mode(value)?;
            }
            value if value.starts_with("--date=") => {
                date_mode = log_date_mode(&value["--date=".len()..])?;
            }
            "--merges" => min_parents = Some(2),
            "--no-merges" => max_parents = Some(1),
            "--no-min-parents" => min_parents = None,
            "--no-max-parents" => max_parents = None,
            value if value.starts_with("--min-parents=") => {
                let value = value
                    .strip_prefix("--min-parents=")
                    .ok_or_else(|| GitError::Command("--min-parents requires a value".into()))?;
                min_parents = Some(parse_rev_list_parent_count(value)?);
            }
            value if value.starts_with("--max-parents=") => {
                let value = value
                    .strip_prefix("--max-parents=")
                    .ok_or_else(|| GitError::Command("--max-parents requires a value".into()))?;
                max_parents = Some(parse_rev_list_parent_count(value)?);
            }
            value if value.starts_with('-') => {
                return Err(GitError::Command(format!(
                    "unsupported rev-list option {value}"
                )));
            }
            value => setup_args.push(value.to_string()),
        }
    }
    if read_stdin {
        let mut input = String::new();
        io::stdin().read_to_string(&mut input)?;
        if setup_not {
            setup_args.push("--not".to_string());
        }
        setup_args.extend(
            input
                .lines()
                .filter(|line| !line.is_empty())
                .map(str::to_string),
        );
    }
    if parents && children {
        return Err(GitError::Command(
            "options '--parents' and '--children' cannot be used together".into(),
        ));
    }
    if preset_oneline {
        let mut compiled = presets::rev_list_oneline()?;
        if parents {
            compiled.insert_parents_after_oid();
        }
        pretty = RevListPretty::Compiled {
            compiled,
            commit_header: false,
        };
    }
    if nul_terminated && (left_right || objects_edge || header || pretty != RevListPretty::Default)
    {
        return Err(GitError::Command(
            "-z option used with unsupported option".into(),
        ));
    }
    if object_filter != RevListObjectFilter::None && !objects {
        eprintln!("fatal: object filtering requires --objects");
        return Err(GitError::Exit(128));
    }
    let author_filters = parse_log_filter_patterns(&author_patterns, regexp_mode)?;
    let committer_filters = parse_log_filter_patterns(&committer_patterns, regexp_mode)?;
    let grep_filters = parse_log_filter_patterns(&grep_patterns, regexp_mode)?;
    let git_dir = discover_git_dir(env::current_dir()?)?;
    let format = repository_object_format(&git_dir)?;
    let config = read_repo_config(&git_dir)?;
    let output_encoding = log_output_encoding(&config);
    // Mailmap engine for `--use-mailmap` custom-format atoms (the upper-case
    // `%aN`/… always map; lower-case map only under the flag).
    let mailmap = commands::utility::Mailmap::load_default(&git_dir, format)?;
    let db = FileObjectDatabase::from_git_dir(&git_dir, format);
    object_filter = object_filter.resolve(&git_dir, &db, format)?;
    let cwd = env::current_dir()?;
    let worktree_root = worktree_root_for_git_dir(&git_dir).ok();
    let exclude_object_tips = if objects {
        rev_list_extract_non_commit_excludes(&mut setup_args, &git_dir, &db, format)?
    } else {
        Vec::new()
    };
    if bisect_inject_refs {
        // git's `setup_revisions` treats `--bisect` as "also add the current
        // `refs/bisect/<bad>` as a positive and every `refs/bisect/<good>-*`
        // as a negative". They stack on top of any explicit revisions; when
        // the refs are absent nothing is added. This is what makes
        // `git rev-list --bisect` (no args) default to the bisect state.
        let default = rev_list_bisect_default_revs(&git_dir, format)?;
        if let Some(bad) = default.bad {
            setup_args.push(bad.to_hex());
        }
        for good in default.goods {
            setup_args.push(format!("^{}", good.to_hex()));
        }
    }
    let missing_tip_candidates = if objects
        || matches!(
            missing_action,
            RevListMissingAction::Print | RevListMissingAction::PrintInfo
        ) {
        rev_list_missing_tip_candidates(&setup_args)
    } else {
        Vec::new()
    };
    if !matches!(missing_action, RevListMissingAction::Error)
        && !setup_args.iter().any(|arg| arg == "--ignore-missing")
    {
        setup_args.push("--ignore-missing".to_string());
    }
    let setup = sley_rev::setup_revisions(
        &setup_args,
        &sley_rev::RevisionSetupContext {
            git_dir: &git_dir,
            worktree_root: worktree_root.as_deref(),
            cwd: &cwd,
            format,
            reader: &db,
            config: Some(&config),
        },
    )?;
    if let Some(leftover) = setup.leftovers.first() {
        return Err(GitError::Command(format!(
            "unsupported rev-list option {leftover}"
        )));
    }
    let revision_options = setup.options;
    // `--stdin` makes the empty case legal: git reads revisions from stdin and
    // produces empty output rather than the usage error a bare `rev-list` hits.
    if !revision_options.has_revisions()
        && !revision_options.ignore_missing
        && !bisect
        && !read_stdin
    {
        return Err(GitError::Command(
            "rev-list currently requires at least one revision".into(),
        ));
    }
    let ignore_missing = revision_options.ignore_missing;
    let max_count = revision_options.max_count;
    let skip_count = revision_options.skip;
    let max_age = revision_options.date_window.min_time;
    let min_age = revision_options.date_window.max_time;
    let reverse = revision_options.reverse;
    let ordering = match revision_options.order {
        sley_rev::RevisionOrder::Default => RevListOrdering::Default,
        sley_rev::RevisionOrder::Topo => RevListOrdering::Topo,
        sley_rev::RevisionOrder::Date => RevListOrdering::Date,
        sley_rev::RevisionOrder::AuthorDate => RevListOrdering::AuthorDate,
    };
    let walk_mode = match revision_options.no_walk {
        sley_rev::NoWalkMode::Walk => RevListWalkMode::Walk,
        sley_rev::NoWalkMode::Sorted => RevListWalkMode::NoWalkSorted,
        sley_rev::NoWalkMode::Unsorted => RevListWalkMode::NoWalkUnsorted,
    };
    let first_parent = revision_options.first_parent;
    let pathspecs = setup.pathspecs;
    let full_history = revision_options.full_history;
    let mut include_commits = Vec::new();
    let mut start_tag_objects = Vec::new();
    // Tips that resolve to non-commit objects (git's pending-object model):
    // a provided blob is emitted directly in --objects mode (exempt from
    // filters unless --filter-provided-objects), silently dropped otherwise;
    // any other non-commit tip is additionally accepted under
    // --use-bitmap-index, where the bitmap traversal can start from it.
    let mut provided_objects: Vec<RevListObject> = Vec::new();
    let mut bitmap_object_tips: Vec<ObjectId> = Vec::new();
    for tip in &revision_options.positives {
        let start = match rev_list_start_from_oid(&db, format, tip.oid, ignore_missing) {
            Ok(start) => start,
            Err(err) => {
                let Ok(object) = db.read_object(&tip.oid) else {
                    return Err(err);
                };
                match object.object_type {
                    ObjectType::Blob if objects => {
                        // git names a pathy tip by its path component.
                        let name = tip
                            .rev
                            .split_once(':')
                            .map(|(_, path)| path.as_bytes().to_vec())
                            .unwrap_or_default();
                        provided_objects.push(RevListObject {
                            oid: tip.oid,
                            name,
                            object_type: Some(ObjectType::Blob),
                        });
                    }
                    ObjectType::Blob | ObjectType::Tree if !use_bitmap_index => {
                        // Without --objects, git silently ignores non-commit
                        // pending objects.
                    }
                    _ if use_bitmap_index => bitmap_object_tips.push(tip.oid),
                    _ => return Err(err),
                }
                continue;
            }
        };
        if let Some(start) = start {
            include_commits.push(start.commit);
            if let Some(tag_object) = start.tag_object {
                start_tag_objects.push(RevListTagObject {
                    commit: start.commit,
                    object: tag_object,
                });
            }
        }
    }
    let mut left_right_sides = HashMap::new();
    for range in &revision_options.symmetric_ranges {
        if (left_right || side_filter.is_some() || cherry_mode != RevListCherryMode::None)
            && !range.negated
        {
            for record in rev_list_walk_commits_with_missing(
                &db,
                format,
                [range.left],
                first_parent,
                missing_action,
            )? {
                left_right_sides.entry(record.oid).or_insert('<');
            }
            for record in rev_list_walk_commits_with_missing(
                &db,
                format,
                [range.right],
                first_parent,
                missing_action,
            )? {
                left_right_sides.entry(record.oid).or_insert('>');
            }
        }
    }
    let object_filter_tip_oids = if !filter_provided_objects
        && matches!(
            object_filter,
            RevListObjectFilter::ObjectType(ObjectType::Blob | ObjectType::Tree | ObjectType::Tag)
        ) {
        // Without `--filter-provided-objects`, a directly-provided commit tip is emitted even
        // when an `object:type` filter would otherwise exclude it; with the flag it is filtered
        // like any other object (an empty exemption set).
        include_commits.iter().cloned().collect::<HashSet<_>>()
    } else {
        HashSet::new()
    };
    let exclude_tip_oids: Vec<ObjectId> = revision_options.negatives.clone();

    if verify_objects {
        let mut verify_roots = Vec::new();
        verify_roots.extend(include_commits.iter().copied());
        verify_roots.extend(start_tag_objects.iter().map(|tag| tag.object.oid));
        verify_roots.extend(provided_objects.iter().map(|object| object.oid));
        verify_roots.extend(bitmap_object_tips.iter().copied());
        if rev_list_verify_objects(&db, format, verify_roots) {
            return Err(GitError::Exit(1));
        }
    }

    if test_bitmap {
        return rev_list_test_bitmap(&git_dir, &db, format, &include_commits, &exclude_tip_oids);
    }

    if use_bitmap_index {
        // Mirror upstream's want list: tag objects stand in for the commits
        // they were peeled from (the tag itself is part of the result), plus
        // any non-commit tips collected above.
        let mut want_roots = Vec::with_capacity(include_commits.len() + bitmap_object_tips.len());
        for commit in &include_commits {
            match start_tag_objects
                .iter()
                .find(|tag_object| tag_object.commit == *commit)
            {
                Some(tag_object) => want_roots.push(tag_object.object.oid),
                None => want_roots.push(*commit),
            }
        }
        want_roots.extend(bitmap_object_tips.iter().copied());
        want_roots.extend(provided_objects.iter().map(|object| object.oid));
        // Allowlist: anything the bitmap result cannot answer (traversal
        // order, pathspec pruning, per-commit predicates, output shaping)
        // falls back to the regular walk, like upstream's try_bitmap_*
        // helpers returning -1.
        let bitmap_eligible = walk_mode == RevListWalkMode::Walk
            && ordering == RevListOrdering::Default
            && !bisect
            && pathspecs.is_empty()
            && !full_history
            && !first_parent
            && !parents
            && !children
            && !boundary
            && !left_right
            && side_filter.is_none()
            && cherry_mode == RevListCherryMode::None
            && !timestamp
            && !quiet
            && !nul_terminated
            && !objects_edge
            && !header
            && disk_usage.is_none()
            && !abbrev_commit
            && !reverse
            && pretty == RevListPretty::Default
            && skip_count == 0
            && min_parents.is_none()
            && max_parents.is_none()
            && max_age.is_none()
            && min_age.is_none()
            && author_filters.is_empty()
            && committer_filters.is_empty()
            && grep_filters.is_empty()
            && !ignore_missing
            && object_filter.bitmap_eligible()
            && if count {
                // A max-count of reachable *objects* cannot be answered from
                // a bitmap (no commit/object association); commit counting
                // clamps instead (upstream try_bitmap_count).
                !(max_count.is_some() && objects)
            } else {
                max_count.is_none()
            };
        let query = RevListBitmapQuery {
            want_roots: &want_roots,
            exclude_tips: &exclude_tip_oids,
            objects,
            count,
            max_count,
            object_filter: object_filter.clone(),
            filter_provided_objects,
            unpacked,
        };
        if bitmap_eligible
            && !want_roots.is_empty()
            && rev_list_try_bitmap(&git_dir, &db, format, &query)?
        {
            return Ok(());
        }
    }
    if !bitmap_object_tips.is_empty() {
        // The bitmap path was not usable, and the regular walk cannot start
        // from a non-commit tip.
        return Err(GitError::Command(
            "rev-list cannot start the walk from a non-commit object".into(),
        ));
    }
    if filter_provided_objects && !provided_objects.is_empty() {
        // With --filter-provided-objects the directly-provided blobs lose
        // their filter exemption.
        let mut kept = Vec::with_capacity(provided_objects.len());
        for object in provided_objects.drain(..) {
            let size = if rev_list_filter_needs_blob_size(&object_filter) {
                Some(db.read_object(&object.oid)?.body.len())
            } else {
                None
            };
            let keep = object_filter.includes_object(
                ObjectType::Blob,
                &object.oid,
                &object.name,
                size,
                0,
            )?;
            if keep {
                kept.push(object);
            }
        }
        provided_objects = kept;
    }

    let mut excluded = HashSet::new();
    for oid in &exclude_tip_oids {
        for record in
            rev_list_walk_commits_with_missing(&db, format, [*oid], first_parent, missing_action)?
        {
            excluded.insert(record.oid);
        }
    }
    // Apply `--no-commit-header` / `--commit-header` to the compiled format
    // (order-independent override of the per-format default).
    if let (Some(want_header), RevListPretty::Compiled { commit_header, .. }) =
        (commit_header_override, &mut pretty)
    {
        *commit_header = want_header;
    }
    // Commit-graph fast path: a plain commit listing (no flag that needs the parsed
    // commit object) walks via the commit-graph and reads zero commit objects. Any
    // commit-body-dependent mode falls through to the full walk below. The guard is
    // a strict allowlist — only flags whose handling needs solely oid+parents+time.
    let metadata_format = match &pretty {
        // The metadata fast-path does not emit the `commit <oid>` header that
        // `--format=` / `--pretty=format:` require (commit_header: true), so it
        // only handles header-less formats (`--no-commit-header`). Header formats
        // fall through to the full Compiled path below, which prints the header
        // and abbreviates `%h` correctly.
        RevListPretty::Compiled {
            compiled,
            commit_header: false,
        } if compiled.is_metadata_emitable()
            && compiled.uses_oid()
            && !compiled.uses_decorations() =>
        {
            Some(compiled)
        }
        _ => None,
    };
    if walk_mode == RevListWalkMode::Walk
        && matches!(ordering, RevListOrdering::Default | RevListOrdering::Date)
        && !bisect
        && (matches!(pretty, RevListPretty::Default) || metadata_format.is_some())
        && matches!(object_filter, RevListObjectFilter::None)
        && pathspecs.is_empty()
        && !full_history
        && !objects
        && !objects_edge
        && disk_usage.is_none()
        && !boundary
        && !header
        && !children
        && !left_right
        && side_filter.is_none()
        && cherry_mode == RevListCherryMode::None
        && !timestamp
        && author_filters.is_empty()
        && committer_filters.is_empty()
        && grep_filters.is_empty()
        && max_age.is_none()
        && min_age.is_none()
    {
        if count
            && skip_count == 0
            && max_count.is_none()
            && excluded.is_empty()
            && min_parents.is_none()
            && max_parents.is_none()
        {
            let total = if quiet {
                0
            } else {
                sley_rev::count_commit_metadata(
                    &git_dir,
                    format,
                    &db,
                    include_commits.clone(),
                    first_parent,
                )?
            };
            println!("{total}");
            return Ok(());
        }
        let limit = max_count.map(|max| skip_count.saturating_add(max));
        let metadata = if let Some(limit) = limit.filter(|limit| *limit > 0) {
            sley_rev::walk_commit_metadata_date_ordered_limited(
                &git_dir,
                format,
                &db,
                include_commits.clone(),
                first_parent,
                limit,
            )?
        } else {
            sley_rev::walk_commit_metadata(
                &git_dir,
                format,
                &db,
                include_commits.clone(),
                first_parent,
            )?
        };
        let mut selected = metadata
            .into_iter()
            .filter(|record| !excluded.contains(&record.oid))
            .filter(|record| {
                !(min_parents.is_some_and(|min| record.parents.len() < min)
                    || max_parents.is_some_and(|max| record.parents.len() > max))
            })
            .collect::<Vec<_>>();
        if limit.is_none() {
            selected = rev_list_metadata_date_order(selected);
        }
        if skip_count > 0 {
            selected = selected.into_iter().skip(skip_count).collect();
        }
        if let Some(max_count) = max_count {
            selected.truncate(max_count);
        }
        if reverse {
            selected.reverse();
        }
        if count {
            println!("{}", if quiet { 0 } else { selected.len() });
            return Ok(());
        }
        if quiet {
            return Ok(());
        }
        let mut stdout = io::stdout();
        // `%h`/`%t`/`%p` in a format always abbreviate to `abbrev_len` (default
        // 7), independent of `--abbrev-commit` (which only abbreviates the
        // `commit <oid>` header). The plain-oid fast path below still gates on
        // `abbrev_commit`.
        let effective_abbrev_len = abbrev_len;
        let mut line =
            metadata_format.map(|compiled| Vec::with_capacity(compiled.estimated_line_capacity()));
        for record in &selected {
            if let Some(compiled) = metadata_format {
                let line = line
                    .as_mut()
                    .expect("metadata line buffer initialized with metadata format");
                line.clear();
                emit_compiled_log_format_metadata(
                    record,
                    compiled,
                    &LogFormatContext {
                        abbrev_len: effective_abbrev_len,
                        decorations: &HashMap::new(),
                        marker: '>',
                        dialect: LogFormatDialect::RevList,
                        source: None,
                        date_mode: &date_mode,
                        source_oid: None,
                        describe: None,
                        signature: None,
                        color: want_color,
                        output_encoding: &output_encoding,
                        mailmap: &mailmap,
                        use_mailmap,
                    },
                    line,
                )?;
                stdout.write_all(line)?;
                if parents && !compiled.uses_parents() {
                    for parent in &record.parents {
                        write!(stdout, " {parent}")?;
                    }
                }
            } else {
                write!(
                    stdout,
                    "{}",
                    format_rev_list_oid(&record.oid, abbrev_commit, abbrev_len)
                )?;
                if parents {
                    for parent in &record.parents {
                        write!(stdout, " {parent}")?;
                    }
                }
            }
            stdout.write_all(if nul_terminated { b"\0" } else { b"\n" })?;
        }
        stdout.flush()?;
        return Ok(());
    }
    let mut missing_commit_objects = Vec::new();
    let commits = match walk_mode {
        RevListWalkMode::Walk => {
            let walk = rev_list_walk_commits_with_missing_details(
                &db,
                format,
                include_commits,
                first_parent,
                missing_action,
            )?;
            missing_commit_objects = walk
                .missing
                .into_iter()
                .map(|oid| RevListObject {
                    oid,
                    name: Vec::new(),
                    object_type: Some(ObjectType::Commit),
                })
                .collect();
            walk.records
        }
        RevListWalkMode::NoWalkSorted | RevListWalkMode::NoWalkUnsorted => {
            rev_list_no_walk_commits(&db, format, include_commits)?
        }
    };
    let selected_commit_oids = commits
        .iter()
        .map(|record| record.oid)
        .collect::<HashSet<_>>();
    let mut known_direct_object_oids = start_tag_objects
        .iter()
        .map(|tag| tag.object.oid)
        .collect::<HashSet<_>>();
    known_direct_object_oids.extend(provided_objects.iter().map(|object| object.oid));
    let mut recovered_missing_tips = rev_list_missing_tip_objects(
        &git_dir,
        &db,
        format,
        &missing_tip_candidates,
        &selected_commit_oids,
        &known_direct_object_oids,
        objects,
        missing_action,
    )?;
    provided_objects.append(&mut recovered_missing_tips.provided);
    let mut missing_tip_objects = recovered_missing_tips.missing;
    let mut selected = Vec::new();
    for record in &commits {
        if excluded.contains(&record.oid) {
            continue;
        }
        if min_parents.is_some_and(|min| record.parents.len() < min)
            || max_parents.is_some_and(|max| record.parents.len() > max)
        {
            continue;
        }
        let timestamp = commit_identity_timestamp_i64(&record.commit.committer)?;
        if max_age.is_some_and(|age| timestamp < age) || min_age.is_some_and(|age| timestamp > age)
        {
            continue;
        }
        if !log_author_filters_match(record, &author_filters, regexp_ignore_case)
            || !log_committer_filters_match(record, &committer_filters, regexp_ignore_case)
            || !log_grep_filters_match(
                record,
                &grep_filters,
                grep_all_match,
                invert_grep,
                regexp_ignore_case,
            )
        {
            continue;
        }
        selected.push(record);
    }
    selected = match (walk_mode, ordering) {
        (RevListWalkMode::NoWalkSorted, RevListOrdering::Default) => rev_list_date_order(selected)?,
        (RevListWalkMode::NoWalkUnsorted, RevListOrdering::Default) => selected,
        (RevListWalkMode::Walk, RevListOrdering::Default) => rev_list_date_order(selected)?,
        (_, RevListOrdering::Topo) => rev_list_topo_order(selected)?,
        (_, RevListOrdering::Date) => rev_list_date_order(selected)?,
        (_, RevListOrdering::AuthorDate) => rev_list_author_date_order(selected)?,
    };
    if bisect {
        // git runs `find_bisection` on the limited commit list (date-ordered,
        // newest-first) and replaces the output with the bisection result.
        return rev_list_emit_bisection(
            &git_dir,
            &db,
            format,
            &selected,
            bisect_vars,
            bisect_all,
            first_parent,
        );
    }
    // `--ancestry-path`: keep only commits on a path from a `^`-excluded
    // boundary (bottom) commit up to the tips. Applied before simplification,
    // matching git's `limit_to_ancestry` (which runs in `limit_list`).
    if revision_options.ancestry_path && !exclude_tip_oids.is_empty() {
        let on_path = sley_rev::ancestry_path_on_set(
            selected.iter().map(|r| (r.oid, r.parents.clone())),
            &exclude_tip_oids,
        );
        selected.retain(|r| on_path.contains(&r.oid));
    }
    // Pathspec-limited / --full-history simplification: TREESAME-prune the
    // ordered set and rewrite parents past the dropped commits. Held in an
    // owned binding so `selected` (a Vec of references) can borrow from it.
    let simplified_storage;
    if !pathspecs.is_empty() || full_history || revision_options.simplify_merges {
        let pathspec = normalized_revwalk_pathspec(
            &cwd,
            worktree_root.as_deref(),
            &pathspecs,
            effective_pathspec_flags(),
        )?;
        let ordered_owned: Vec<sley_rev::CommitRecord> =
            selected.iter().map(|r| (*r).clone()).collect();
        // The `^`-excluded boundary tips are git's BOTTOM commits: relevant for
        // topology-keep decisions even though they aren't shown.
        let bottoms: HashSet<ObjectId> = exclude_tip_oids.iter().copied().collect();
        simplified_storage = sley_rev::simplify_history_with_bottoms(
            &db,
            format,
            ordered_owned,
            &pathspec,
            sley_rev::SimplifyOptions {
                full_history,
                first_parent,
                simplify_merges: revision_options.simplify_merges,
                show_pulls: revision_options.show_pulls,
                ancestry_path: revision_options.ancestry_path,
                // git's `want_ancestry` = `rewrite_parents || children`.
                // `--ancestry-path` alone does NOT set rewrite_parents, so a bare
                // `--ancestry-path` still drops TREESAME merges.
                want_ancestry: parents || children || revision_options.simplify_merges,
            },
            &bottoms,
        )?;
        selected = simplified_storage.iter().collect();
    }
    let patchsame_oids = if cherry_mode != RevListCherryMode::None {
        rev_list_patchsame_oids(
            &db,
            format,
            &selected,
            &left_right_sides,
            &cwd,
            worktree_root.as_deref(),
            &pathspecs,
        )?
    } else {
        HashSet::new()
    };
    if cherry_mode == RevListCherryMode::Pick {
        selected.retain(|record| !patchsame_oids.contains(&record.oid));
    }
    if let Some(side) = side_filter {
        selected.retain(|record| left_right_sides.get(&record.oid).copied().unwrap_or('>') == side);
    }
    if skip_count > 0 {
        selected = selected.into_iter().skip(skip_count).collect();
    }
    if let Some(max_count) = max_count {
        selected.truncate(max_count);
    }
    if reverse {
        selected.reverse();
    }
    let decorations = match &pretty {
        RevListPretty::Compiled { compiled, .. } if compiled.uses_decorations() => {
            log_decoration_map(
                &git_dir,
                &db,
                format,
                LogDecorationMode::Short,
                &crate::DecorationFilter::default(),
            )?
        }
        _ => HashMap::new(),
    };
    let edge_oids = if objects_edge {
        rev_list_edge_oids(&selected, &excluded)
    } else {
        Vec::new()
    };
    let boundary_records = if boundary {
        rev_list_boundary_records(&db, format, &selected, &excluded)?
    } else {
        Vec::new()
    };
    let selected_tag_objects = if objects {
        rev_list_selected_tag_objects(&selected, &start_tag_objects)
    } else {
        Vec::new()
    };
    let (mut selected_objects, mut omitted_objects, mut missing_objects) = if objects {
        rev_list_objects(
            &db,
            format,
            &selected,
            &excluded,
            &exclude_object_tips,
            &object_filter,
            filter_print_omitted,
            missing_action,
        )?
    } else {
        (Vec::new(), Vec::new(), Vec::new())
    };
    if !provided_objects.is_empty() {
        // A provided object is emitted once, as the provided entry.
        let provided_oids: HashSet<ObjectId> =
            provided_objects.iter().map(|object| object.oid).collect();
        selected_objects.retain(|object| !provided_oids.contains(&object.oid));
    }
    if let Some(human_readable) = disk_usage {
        let disk_usage = rev_list_disk_usage(
            &git_dir,
            &selected,
            &boundary_records,
            &selected_tag_objects,
            &selected_objects,
            human_readable,
        )?;
        println!("{disk_usage}");
        return Ok(());
    }
    for oid in &edge_oids {
        println!("-{oid}");
    }
    if count {
        if quiet {
            if left_right && cherry_mode == RevListCherryMode::Mark {
                println!("0\t0\t0");
            } else if left_right || cherry_mode == RevListCherryMode::Mark {
                println!("0\t0");
            } else {
                println!("0");
            }
            return Ok(());
        }
        if left_right {
            let left_count = selected
                .iter()
                .filter(|record| {
                    rev_list_should_print_commit(record, &object_filter, &object_filter_tip_oids)
                })
                .filter(|record| left_right_sides.get(&record.oid).copied().unwrap_or('>') == '<')
                .filter(|record| !patchsame_oids.contains(&record.oid))
                .count();
            let right_count = selected
                .iter()
                .filter(|record| {
                    rev_list_should_print_commit(record, &object_filter, &object_filter_tip_oids)
                        && left_right_sides.get(&record.oid).copied().unwrap_or('>') != '<'
                })
                .filter(|record| !patchsame_oids.contains(&record.oid))
                .count();
            if cherry_mode == RevListCherryMode::Mark {
                let same_count = selected
                    .iter()
                    .filter(|record| {
                        rev_list_should_print_commit(
                            record,
                            &object_filter,
                            &object_filter_tip_oids,
                        ) && patchsame_oids.contains(&record.oid)
                    })
                    .count();
                println!("{left_count}\t{right_count}\t{same_count}");
            } else {
                println!("{left_count}\t{right_count}");
            }
            return Ok(());
        }
        if cherry_mode == RevListCherryMode::Mark {
            let same_count = selected
                .iter()
                .filter(|record| {
                    rev_list_should_print_commit(record, &object_filter, &object_filter_tip_oids)
                        && patchsame_oids.contains(&record.oid)
                })
                .count();
            let different_count = selected
                .iter()
                .filter(|record| {
                    rev_list_should_print_commit(record, &object_filter, &object_filter_tip_oids)
                        && !patchsame_oids.contains(&record.oid)
                })
                .count();
            println!("{different_count}\t{same_count}");
            return Ok(());
        }
        println!(
            "{}",
            selected
                .iter()
                .filter(|record| rev_list_should_print_commit(
                    record,
                    &object_filter,
                    &object_filter_tip_oids
                ))
                .count()
                + boundary_records.len()
                + selected_tag_objects.len()
                + provided_objects.len()
                + selected_objects.len()
        );
        return Ok(());
    }
    if quiet
        && !(objects
            && (filter_print_omitted
                || matches!(
                    missing_action,
                    RevListMissingAction::Print | RevListMissingAction::PrintInfo
                )))
    {
        return Ok(());
    }
    let mut child_oids = HashMap::<ObjectId, Vec<ObjectId>>::new();
    if children {
        let selected_oids = selected
            .iter()
            .map(|record| record.oid)
            .collect::<HashSet<_>>();
        for record in &selected {
            for parent in &record.parents {
                if selected_oids.contains(parent) {
                    child_oids.entry(*parent).or_default().push(record.oid);
                }
            }
        }
    }
    if !quiet {
        for record in selected {
            if !rev_list_should_print_commit(record, &object_filter, &object_filter_tip_oids) {
                continue;
            }
            let left_right_prefix = left_right_sides.get(&record.oid).copied().unwrap_or('>');
            let output_prefix = rev_list_output_prefix(
                cherry_mode,
                patchsame_oids.contains(&record.oid),
                left_right,
                left_right_prefix,
            );
            match &pretty {
                RevListPretty::Default
                | RevListPretty::Compiled {
                    commit_header: false,
                    ..
                } => {
                    let oneline = matches!(
                        pretty,
                        RevListPretty::Compiled {
                            commit_header: false,
                            ..
                        }
                    );
                    if timestamp {
                        print!(
                            "{} ",
                            commit_identity_timestamp_i64(&record.commit.committer)?
                        );
                    }
                    if let Some(prefix) = output_prefix {
                        print!("{prefix}");
                    }
                    if oneline {
                        let RevListPretty::Compiled { compiled, .. } = &pretty else {
                            unreachable!("oneline requires compiled preset");
                        };
                        let format_context = LogFormatContext {
                            abbrev_len: abbrev_commit.then_some(abbrev_len).flatten(),
                            decorations: &decorations,
                            marker: output_prefix.unwrap_or(left_right_prefix),
                            dialect: LogFormatDialect::RevList,
                            source: None,
                            date_mode: &date_mode,
                            source_oid: None,
                            describe: None,
                            signature: None,
                            color: want_color,
                            output_encoding: &output_encoding,
                            mailmap: &mailmap,
                            use_mailmap,
                        };
                        if children {
                            print!(
                                "{}",
                                format_rev_list_oid(&record.oid, abbrev_commit, abbrev_len)
                            );
                            if parents {
                                for parent in &record.parents {
                                    print!(" {parent}");
                                }
                            }
                            if let Some(children) = child_oids.get(&record.oid) {
                                for child in children {
                                    print!(" {child}");
                                }
                            }
                            print!(" {}", commit_subject(&record.commit.message));
                        } else {
                            print_log_format(record, compiled, format_context)?;
                        }
                    } else {
                        print!(
                            "{}",
                            format_rev_list_oid(&record.oid, abbrev_commit, abbrev_len)
                        );
                        if parents {
                            for parent in &record.parents {
                                print!(" {parent}");
                            }
                        }
                        if children && let Some(children) = child_oids.get(&record.oid) {
                            for child in children {
                                print!(" {child}");
                            }
                        }
                    }
                    if nul_terminated {
                        print!("\0");
                    } else {
                        println!();
                    }
                    if header && !oneline {
                        write_rev_list_header(record)?;
                    }
                }
                RevListPretty::Short => write_rev_list_short(
                    record,
                    output_prefix,
                    parents,
                    abbrev_commit,
                    abbrev_len,
                    timestamp,
                    &output_encoding,
                )?,
                RevListPretty::Compiled {
                    compiled,
                    commit_header: true,
                } => {
                    write_rev_list_commit_header_line(
                        record,
                        output_prefix,
                        parents,
                        abbrev_commit,
                        abbrev_len,
                        timestamp,
                    )?;
                    let emitted = print_log_format(
                        record,
                        compiled,
                        LogFormatContext {
                            abbrev_len,
                            decorations: &decorations,
                            marker: left_right_prefix,
                            dialect: LogFormatDialect::RevList,
                            source: None,
                            date_mode: &date_mode,
                            source_oid: None,
                            describe: None,
                            signature: None,
                            color: want_color,
                            output_encoding: &output_encoding,
                            mailmap: &mailmap,
                            use_mailmap,
                        },
                    )?;
                    if emitted > 0 {
                        println!();
                    }
                }
            }
        }
    }
    if !quiet {
        for record in boundary_records {
            write_rev_list_boundary_record(
                &record,
                RevListBoundaryOptions {
                    pretty: &pretty,
                    abbrev_commit,
                    abbrev_len,
                    timestamp,
                    parents,
                    decorations: &decorations,
                    date_mode: &date_mode,
                    output_encoding: &output_encoding,
                    mailmap: &mailmap,
                    use_mailmap,
                },
            )?;
        }
        for object in selected_tag_objects {
            write_rev_list_object_line(&object, object_names, nul_terminated)?;
        }
        for object in &provided_objects {
            write_rev_list_object_line(object, object_names, nul_terminated)?;
        }
        for object in selected_objects {
            write_rev_list_object_line(&object, object_names, nul_terminated)?;
        }
    }
    if filter_print_omitted {
        for object in omitted_objects.drain(..) {
            write_rev_list_omitted_object_line(&object, nul_terminated)?;
        }
    }
    if matches!(
        missing_action,
        RevListMissingAction::Print | RevListMissingAction::PrintInfo
    ) {
        let print_info = missing_action == RevListMissingAction::PrintInfo;
        let mut printed_missing = HashSet::new();
        for object in missing_tip_objects.drain(..) {
            if printed_missing.insert(object.oid) {
                write_rev_list_missing_object_line(&object, nul_terminated, print_info)?;
            }
        }
        for object in missing_commit_objects.drain(..) {
            if printed_missing.insert(object.oid) {
                write_rev_list_missing_object_line(&object, nul_terminated, print_info)?;
            }
        }
        for object in missing_objects.drain(..) {
            if printed_missing.insert(object.oid) {
                write_rev_list_missing_object_line(&object, nul_terminated, print_info)?;
            }
        }
    }
    io::stdout().flush()?;
    Ok(())
}

fn rev_list_verify_objects(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    roots: Vec<ObjectId>,
) -> bool {
    let mut failed = false;
    let mut seen = HashSet::new();
    let mut pending = std::collections::VecDeque::from(roots);
    while let Some(oid) = pending.pop_front() {
        if !seen.insert(oid) {
            continue;
        }
        let object = match db.read_object(&oid) {
            Ok(object) => object,
            Err(err) => {
                eprintln!("error: {oid}: object corrupt or missing: {err}");
                failed = true;
                continue;
            }
        };
        if object.object_id(format).is_ok_and(|actual| actual != oid) {
            eprintln!("error: hash mismatch {oid}");
            failed = true;
        }
        match object.object_type {
            ObjectType::Commit => {
                if let Ok(commit) = sley_object::Commit::parse_ref(format, &object.body) {
                    pending.push_back(commit.tree);
                    pending.extend(commit.parents);
                }
            }
            ObjectType::Tree => {
                if let Ok(entries) = sley_object::TreeEntries::new(format, &object.body)
                    .collect::<std::result::Result<Vec<_>, _>>()
                {
                    pending.extend(entries.into_iter().map(|entry| entry.oid));
                }
            }
            ObjectType::Tag => {
                if let Ok(tag) = sley_object::Tag::parse_ref(format, &object.body) {
                    pending.push_back(tag.object);
                }
            }
            ObjectType::Blob => {}
        }
    }
    failed
}

/// The default bisect revisions injected by `--bisect`: the `refs/bisect/<bad>`
/// commit (positive) plus every `refs/bisect/<good>-*` commit (negative). The
/// bad/good terms come from `BISECT_TERMS` (defaulting to `bad`/`good`).
struct BisectDefaultRevs {
    bad: Option<ObjectId>,
    goods: Vec<ObjectId>,
}

fn rev_list_bisect_default_revs(git_dir: &Path, format: ObjectFormat) -> Result<BisectDefaultRevs> {
    // Same selection as `bisect next`: the single `refs/bisect/<bad>` and every
    // `refs/bisect/<good>-<oid>` ref (`bad`/`good` from BISECT_TERMS).
    let terms = sley_rev::read_bisect_terms(git_dir)?;
    let good_prefix = format!("{}-", terms.good);
    let store = FileRefStore::new(git_dir, format);
    let mut bad = None;
    let mut goods = Vec::new();
    for reference in store.list_refs()? {
        let RefTarget::Direct(oid) = reference.target else {
            continue;
        };
        let Some(name) = reference.name.strip_prefix("refs/bisect/") else {
            continue;
        };
        if name == terms.bad {
            bad = Some(oid);
        } else if name.starts_with(&good_prefix) {
            goods.push(oid);
        }
    }
    Ok(BisectDefaultRevs { bad, goods })
}

/// Run `find_bisection` over the (date-ordered, newest-first) interesting set
/// and emit the plumbing output for `--bisect` / `--bisect-vars` /
/// `--bisect-all` (upstream `builtin/rev-list.c`'s bisect path +
/// `show_bisect_vars`).
fn rev_list_emit_bisection(
    git_dir: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    selected: &[&sley_rev::CommitRecord],
    bisect_vars: bool,
    bisect_all: bool,
    first_parent: bool,
) -> Result<()> {
    let result = sley_rev::bisect::find_bisection(selected, bisect_all, first_parent);
    let mut stdout = io::stdout();

    if bisect_vars {
        // `show_bisect_vars`: nothing when the set is empty.
        let Some(&(rev, _)) = result.picks.first() else {
            return Ok(());
        };
        let all = result.all;
        let reaches = result.reaches;
        // cnt = max(all - reaches, reaches); test count is cnt - 1.
        let cnt = (all - reaches).max(reaches);
        writeln!(stdout, "bisect_rev='{}'", rev.to_hex())?;
        writeln!(stdout, "bisect_nr={}", cnt - 1)?;
        writeln!(stdout, "bisect_good={}", all - reaches - 1)?;
        writeln!(stdout, "bisect_bad={}", reaches - 1)?;
        writeln!(stdout, "bisect_all={}", all)?;
        writeln!(
            stdout,
            "bisect_steps={}",
            sley_rev::bisect::estimate_bisect_steps(all)
        )?;
        stdout.flush()?;
        return Ok(());
    }

    if bisect_all {
        // Every candidate, newest-first by distance, decorated with `dist=N`
        // alongside its ref decorations (upstream `best_bisection_sorted` +
        // `revs.show_decorations`).
        let decorations = log_decoration_map(
            git_dir,
            db,
            format,
            LogDecorationMode::Short,
            &crate::DecorationFilter::default(),
        )?;
        for (oid, distance) in &result.picks {
            let mut labels: Vec<String> = decorations.get(oid).cloned().unwrap_or_default();
            labels.push(format!("dist={distance}"));
            writeln!(stdout, "{} ({})", oid.to_hex(), labels.join(", "))?;
        }
        stdout.flush()?;
        return Ok(());
    }

    // Plain `--bisect`: the single midpoint commit (or nothing).
    if let Some(&(rev, _)) = result.picks.first() {
        writeln!(stdout, "{}", rev.to_hex())?;
    }
    stdout.flush()?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RevListPretty {
    Default,
    Short,
    /// `commit_header` is true for `--format=` / `--pretty=format:` (git prints a
    /// leading `commit <oid>` line); false for `--oneline` presets.
    Compiled {
        compiled: CompiledLogFormat,
        commit_header: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RevListCherryMode {
    None,
    Pick,
    Mark,
}

fn write_rev_list_header(record: &sley_rev::CommitRecord) -> Result<()> {
    let mut stdout = io::stdout();
    writeln!(stdout, "tree {}", record.commit.tree)?;
    for parent in &record.parents {
        writeln!(stdout, "parent {parent}")?;
    }
    stdout.write_all(b"author ")?;
    stdout.write_all(&record.commit.author)?;
    stdout.write_all(b"\ncommitter ")?;
    stdout.write_all(&record.commit.committer)?;
    stdout.write_all(b"\n\n")?;
    for line in String::from_utf8_lossy(&record.commit.message).lines() {
        stdout.write_all(b"    ")?;
        stdout.write_all(line.as_bytes())?;
        stdout.write_all(b"\n")?;
    }
    stdout.write_all(&[0])?;
    Ok(())
}

fn write_rev_list_short(
    record: &sley_rev::CommitRecord,
    left_right_prefix: Option<char>,
    parents: bool,
    abbrev_commit: bool,
    abbrev_len: Option<usize>,
    timestamp: bool,
    output_encoding: &str,
) -> Result<()> {
    let mut stdout = io::stdout();
    write_rev_list_commit_header_line(
        record,
        left_right_prefix,
        parents,
        abbrev_commit,
        abbrev_len,
        timestamp,
    )?;
    writeln!(
        stdout,
        "Author: {}",
        commit_author_identity(&record.commit.author)
    )?;
    writeln!(stdout)?;
    stdout.write_all(b"    ")?;
    let message = commit_message_for_commit_encoding(&record.commit, output_encoding);
    stdout.write_all(commit_subject_bytes(&message))?;
    stdout.write_all(b"\n")?;
    writeln!(stdout)?;
    Ok(())
}

fn rev_list_output_prefix(
    cherry_mode: RevListCherryMode,
    patchsame: bool,
    left_right: bool,
    left_right_prefix: char,
) -> Option<char> {
    if cherry_mode == RevListCherryMode::Mark {
        if patchsame {
            Some('=')
        } else if left_right {
            Some(left_right_prefix)
        } else {
            Some('+')
        }
    } else {
        left_right.then_some(left_right_prefix)
    }
}

fn write_rev_list_commit_header_line(
    record: &sley_rev::CommitRecord,
    left_right_prefix: Option<char>,
    parents: bool,
    abbrev_commit: bool,
    abbrev_len: Option<usize>,
    timestamp: bool,
) -> Result<()> {
    let mut stdout = io::stdout();
    if timestamp {
        write!(
            stdout,
            "{} ",
            commit_identity_timestamp_i64(&record.commit.committer)?
        )?;
    }
    stdout.write_all(b"commit ")?;
    if let Some(prefix) = left_right_prefix {
        write!(stdout, "{prefix}")?;
    }
    write!(
        stdout,
        "{}",
        format_rev_list_oid(&record.oid, abbrev_commit, abbrev_len)
    )?;
    if parents {
        for parent in &record.parents {
            write!(stdout, " {parent}")?;
        }
    }
    writeln!(stdout)?;
    Ok(())
}

fn rev_list_patchsame_oids(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    selected: &[&sley_rev::CommitRecord],
    left_right_sides: &HashMap<ObjectId, char>,
    cwd: &Path,
    worktree_root: Option<&Path>,
    pathspecs: &[String],
) -> Result<HashSet<ObjectId>> {
    let left_count = selected
        .iter()
        .filter(|record| left_right_sides.get(&record.oid).copied().unwrap_or('>') == '<')
        .count();
    let right_count = selected.len().saturating_sub(left_count);
    if left_count == 0 || right_count == 0 {
        return Ok(HashSet::new());
    }

    let diff_pathspec = if pathspecs.is_empty() {
        None
    } else {
        let Some(root) = worktree_root else {
            return Ok(HashSet::new());
        };
        Some(DiffPathspec::new(cwd, root, pathspecs)?)
    };

    let left_first = left_count < right_count;
    let mut ids = HashMap::<Vec<u8>, Vec<ObjectId>>::new();
    for record in selected {
        let on_left = left_right_sides.get(&record.oid).copied().unwrap_or('>') == '<';
        if left_first != on_left {
            continue;
        }
        if let Some(id) = rev_list_commit_patch_id(db, format, record, diff_pathspec.as_ref())? {
            ids.entry(id).or_default().push(record.oid);
        }
    }

    let mut patchsame = HashSet::new();
    for record in selected {
        let on_left = left_right_sides.get(&record.oid).copied().unwrap_or('>') == '<';
        if left_first == on_left {
            continue;
        }
        let Some(id) = rev_list_commit_patch_id(db, format, record, diff_pathspec.as_ref())?
        else {
            continue;
        };
        let Some(matches) = ids.get(&id) else {
            continue;
        };
        patchsame.insert(record.oid);
        patchsame.extend(matches.iter().copied());
    }

    Ok(patchsame)
}

fn rev_list_commit_patch_id(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    record: &sley_rev::CommitRecord,
    diff_pathspec: Option<&DiffPathspec>,
) -> Result<Option<Vec<u8>>> {
    if record.parents.len() > 1 {
        return Ok(None);
    }
    let parent_tree = match record.parents.first() {
        Some(parent) => commands::merge_rebase::commit_tree_oid(db, format, parent)?,
        None => ObjectId::empty_tree(format),
    };
    let diff = match diff_pathspec {
        Some(pathspec) => rev_list_render_tree_to_tree_patch(
            db,
            format,
            &parent_tree,
            &record.commit.tree,
            pathspec,
        )?,
        None => {
            render_tree_to_tree_patch(db, format, &parent_tree, &record.commit.tree)
                .unwrap_or_default()
        }
    };
    Ok(commands::patch_id::patch_id_for_diff(&diff, format))
}

fn rev_list_render_tree_to_tree_patch(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    old_tree: &ObjectId,
    new_tree: &ObjectId,
    pathspec: &DiffPathspec,
) -> Result<Vec<u8>> {
    let entries = sley_diff_merge::diff_name_status_trees_with_options(
        db,
        format,
        old_tree,
        new_tree,
        sley_diff_merge::DiffNameStatusOptions::default(),
    )?;
    let entries = apply_diff_pathspec(entries, pathspec);
    let mut out = Vec::new();
    for entry in &entries {
        write_diff_patch_entry(
            &mut out,
            entry,
            DiffPatchOptions {
                db,
                worktree_root: None,
                use_worktree_new: false,
                format,
                abbrev: 7,
                src_prefix: "a/",
                dst_prefix: "b/",
                context: 3,
                userdiff: None,
                colors: None,
                word_diff: None,
                no_index_contents: None,
                submodule_format: commands::diff_options::SubmoduleDiffFormat::Short,
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
            },
        )?;
    }
    Ok(out)
}

fn rev_list_edge_oids(
    records: &[&sley_rev::CommitRecord],
    excluded: &HashSet<ObjectId>,
) -> Vec<ObjectId> {
    let mut seen = HashSet::new();
    let mut edge_oids = Vec::new();
    for record in records {
        for parent in &record.parents {
            if excluded.contains(parent) && seen.insert(*parent) {
                edge_oids.push(*parent);
            }
        }
    }
    edge_oids
}

fn rev_list_boundary_records(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    records: &[&sley_rev::CommitRecord],
    excluded: &HashSet<ObjectId>,
) -> Result<Vec<sley_rev::CommitRecord>> {
    let mut out = Vec::new();
    for oid in rev_list_edge_oids(records, excluded) {
        out.push(read_rev_list_commit_record(db, format, oid)?);
    }
    Ok(out)
}

struct RevListBoundaryOptions<'a> {
    pretty: &'a RevListPretty,
    abbrev_commit: bool,
    abbrev_len: Option<usize>,
    timestamp: bool,
    parents: bool,
    decorations: &'a HashMap<ObjectId, Vec<String>>,
    date_mode: &'a DateMode,
    output_encoding: &'a str,
    mailmap: &'a commands::utility::Mailmap,
    use_mailmap: bool,
}

fn write_rev_list_boundary_record(
    record: &sley_rev::CommitRecord,
    options: RevListBoundaryOptions<'_>,
) -> Result<()> {
    let RevListBoundaryOptions {
        pretty,
        abbrev_commit,
        abbrev_len,
        timestamp,
        parents,
        decorations,
        date_mode,
        output_encoding,
        mailmap,
        use_mailmap,
    } = options;
    match pretty {
        RevListPretty::Default
        | RevListPretty::Compiled {
            commit_header: false,
            ..
        } => {
            let oneline = matches!(
                pretty,
                RevListPretty::Compiled {
                    commit_header: false,
                    ..
                }
            );
            if timestamp {
                print!(
                    "{} ",
                    commit_identity_timestamp_i64(&record.commit.committer)?
                );
            }
            if oneline {
                let RevListPretty::Compiled { compiled, .. } = pretty else {
                    unreachable!("oneline requires compiled preset");
                };
                print!("-");
                print_log_format(
                    record,
                    compiled,
                    LogFormatContext {
                        abbrev_len: abbrev_commit.then_some(abbrev_len).flatten(),
                        decorations,
                        marker: '-',
                        dialect: LogFormatDialect::RevList,
                        source: None,
                        date_mode,
                        source_oid: None,
                        describe: None,
                        signature: None,
                        color: false,
                        output_encoding,
                        mailmap,
                        use_mailmap,
                    },
                )?;
            } else {
                print!(
                    "-{}",
                    format_rev_list_oid(&record.oid, abbrev_commit, abbrev_len)
                );
                if parents {
                    for parent in &record.parents {
                        print!(" {parent}");
                    }
                }
            }
            println!();
            Ok(())
        }
        RevListPretty::Short => write_rev_list_short(
            record,
            Some('-'),
            parents,
            abbrev_commit,
            abbrev_len,
            timestamp,
            output_encoding,
        ),
        RevListPretty::Compiled {
            compiled,
            commit_header: true,
        } => {
            write_rev_list_commit_header_line(
                record,
                Some('-'),
                parents,
                abbrev_commit,
                abbrev_len,
                timestamp,
            )?;
            let emitted = print_log_format(
                record,
                compiled,
                LogFormatContext {
                    abbrev_len,
                    decorations,
                    marker: '-',
                    dialect: LogFormatDialect::RevList,
                    source: None,
                    date_mode,
                    source_oid: None,
                    describe: None,
                    signature: None,
                    color: false,
                    output_encoding,
                    mailmap,
                    use_mailmap,
                },
            )?;
            if emitted > 0 {
                println!();
            }
            Ok(())
        }
    }
}

#[derive(Clone)]
struct RevListObject {
    oid: ObjectId,
    name: Vec<u8>,
    object_type: Option<ObjectType>,
}

struct RevListStart {
    commit: ObjectId,
    tag_object: Option<RevListObject>,
}

struct RevListTagObject {
    commit: ObjectId,
    object: RevListObject,
}

struct RevListRecoveredMissingTips {
    provided: Vec<RevListObject>,
    missing: Vec<RevListObject>,
}

fn rev_list_missing_tip_candidates(args: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--" => break,
            "--default" | "-n" | "--max-count" | "--skip" | "--max-age" | "--min-age"
            | "--since" | "--after" | "--until" | "--before" | "--glob" | "--exclude"
            | "--exclude-hidden" => {
                let _ = iter.next();
            }
            value if value.starts_with('-') || value.starts_with('^') || value.contains("..") => {}
            value => out.push(value.to_string()),
        }
    }
    out
}

fn rev_list_extract_non_commit_excludes(
    args: &mut Vec<String>,
    git_dir: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
) -> Result<Vec<RevListObject>> {
    let mut kept = Vec::with_capacity(args.len());
    let mut excluded = Vec::new();
    for arg in args.drain(..) {
        let Some(rev) = arg.strip_prefix('^').filter(|rev| !rev.is_empty()) else {
            kept.push(arg);
            continue;
        };
        let oid = match sley_rev::resolve_revision_with_reader(git_dir, format, db, rev) {
            Ok(oid) => oid,
            Err(_) => {
                kept.push(arg);
                continue;
            }
        };
        let object = match db.read_object(&oid) {
            Ok(object) => object,
            Err(_) => {
                kept.push(arg);
                continue;
            }
        };
        if matches!(object.object_type, ObjectType::Commit | ObjectType::Tag) {
            kept.push(arg);
            continue;
        }
        let name = rev
            .split_once(':')
            .map(|(_, path)| path.as_bytes().to_vec())
            .unwrap_or_default();
        excluded.push(RevListObject {
            oid,
            name,
            object_type: Some(object.object_type),
        });
    }
    *args = kept;
    Ok(excluded)
}

fn rev_list_missing_tip_objects(
    git_dir: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    candidates: &[String],
    selected_commit_oids: &HashSet<ObjectId>,
    known_direct_object_oids: &HashSet<ObjectId>,
    objects: bool,
    missing_action: RevListMissingAction,
) -> Result<RevListRecoveredMissingTips> {
    let mut provided = Vec::new();
    let mut missing = Vec::new();
    let mut provided_seen = HashSet::new();
    let mut missing_seen = HashSet::new();
    for candidate in candidates {
        let Ok(oid) = sley_rev::resolve_revision_with_reader(git_dir, format, db, candidate) else {
            continue;
        };
        if selected_commit_oids.contains(&oid) || known_direct_object_oids.contains(&oid) {
            continue;
        }
        match db.read_object(&oid) {
            Ok(object) if objects && object.object_type == ObjectType::Tag => {
                let tag = Tag::parse(format, &object.body)?;
                if provided_seen.insert(oid) {
                    provided.push(RevListObject {
                        oid,
                        name: tag.name,
                        object_type: Some(ObjectType::Tag),
                    });
                }
                if matches!(
                    missing_action,
                    RevListMissingAction::Print | RevListMissingAction::PrintInfo
                ) && db.read_object(&tag.object).is_err()
                    && missing_seen.insert(tag.object)
                {
                    missing.push(RevListObject {
                        oid: tag.object,
                        name: Vec::new(),
                        object_type: Some(tag.object_type),
                    });
                }
            }
            Ok(_) => {}
            Err(_)
                if matches!(
                    missing_action,
                    RevListMissingAction::Print | RevListMissingAction::PrintInfo
                ) && missing_seen.insert(oid) =>
            {
                missing.push(RevListObject {
                    oid,
                    name: Vec::new(),
                    object_type: None,
                });
            }
            Err(_) => {}
        }
    }
    Ok(RevListRecoveredMissingTips { provided, missing })
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RevListObjectFilter {
    None,
    BlobNone,
    BlobLimit(usize),
    ObjectType(ObjectType),
    TreeDepth(usize),
    SparseOid(String),
    Sparse(Vec<Vec<u8>>),
    Combine(Vec<RevListObjectFilter>),
}

impl RevListObjectFilter {
    fn parse(spec: &str) -> Result<Self> {
        if spec == "blob:none" {
            return Ok(Self::BlobNone);
        }
        if let Some(value) = spec.strip_prefix("tree:") {
            return Ok(Self::TreeDepth(parse_rev_list_tree_depth(value)?));
        }
        if let Some(value) = spec.strip_prefix("blob:limit=") {
            return Ok(Self::BlobLimit(parse_rev_list_blob_limit(value)?));
        }
        if let Some(value) = spec.strip_prefix("object:type=") {
            return Ok(Self::ObjectType(parse_rev_list_object_type_filter(value)?));
        }
        if let Some(value) = spec.strip_prefix("sparse:oid=") {
            return Ok(Self::SparseOid(value.to_string()));
        }
        if spec.starts_with("sparse:path=") {
            eprintln!("fatal: sparse:path filters support has been dropped");
            return Err(GitError::Exit(128));
        }
        if let Some(value) = spec.strip_prefix("combine:") {
            if value.is_empty() {
                eprintln!("fatal: expected something after combine:");
                return Err(GitError::Exit(128));
            }
            let mut filters = Vec::new();
            for raw in value.split('+') {
                let decoded = rev_list_decode_sub_filter(raw)?;
                filters.push(Self::parse(&decoded)?);
            }
            return Ok(Self::Combine(filters));
        }
        eprintln!("fatal: invalid filter-spec '{spec}'");
        Err(GitError::Exit(128))
    }

    fn combine_with(self, other: Self) -> Self {
        match (self, other) {
            (Self::None, filter) | (filter, Self::None) => filter,
            (Self::Combine(mut filters), Self::Combine(mut more)) => {
                filters.append(&mut more);
                Self::Combine(filters)
            }
            (Self::Combine(mut filters), filter) | (filter, Self::Combine(mut filters)) => {
                filters.push(filter);
                Self::Combine(filters)
            }
            (left, right) => Self::Combine(vec![left, right]),
        }
    }

    fn resolve(
        self,
        git_dir: &Path,
        db: &FileObjectDatabase,
        format: ObjectFormat,
    ) -> Result<Self> {
        match self {
            Self::SparseOid(value) => {
                let oid = if let Some((rev, path)) = value.split_once(':') {
                    sley_rev::resolve_rev_path(git_dir, format, db, rev, path)?
                } else {
                    sley_rev::resolve_revision_with_reader(git_dir, format, db, &value)?
                };
                let object = db.read_object(&oid)?;
                if object.object_type != ObjectType::Blob {
                    eprintln!("fatal: expected blob for sparse:oid filter");
                    return Err(GitError::Exit(128));
                }
                Ok(Self::Sparse(
                    object
                        .body
                        .split(|byte| *byte == b'\n')
                        .filter(|line| !line.is_empty())
                        .map(|line| line.to_vec())
                        .collect(),
                ))
            }
            Self::Combine(filters) => filters
                .into_iter()
                .map(|filter| filter.resolve(git_dir, db, format))
                .collect::<Result<Vec<_>>>()
                .map(Self::Combine),
            filter => Ok(filter),
        }
    }

    fn tree_depth_limit(&self) -> Option<usize> {
        match self {
            Self::TreeDepth(depth) => Some(*depth),
            Self::ObjectType(ObjectType::Commit | ObjectType::Tag) => Some(0),
            Self::Combine(filters) => filters.iter().filter_map(Self::tree_depth_limit).max(),
            _ => None,
        }
    }

    fn bitmap_eligible(&self) -> bool {
        match self {
            Self::None | Self::BlobNone | Self::BlobLimit(_) | Self::ObjectType(_) => true,
            Self::TreeDepth(depth) => *depth == 0,
            Self::Combine(filters) => filters.iter().all(Self::bitmap_eligible),
            Self::SparseOid(_) | Self::Sparse(_) => false,
        }
    }

    fn includes_object(
        &self,
        object_type: ObjectType,
        oid: &ObjectId,
        path: &[u8],
        size: Option<usize>,
        depth: usize,
    ) -> Result<bool> {
        match self {
            Self::None => Ok(true),
            Self::BlobNone => Ok(object_type != ObjectType::Blob),
            Self::BlobLimit(limit) => {
                Ok(object_type != ObjectType::Blob || size.unwrap_or(0) < *limit)
            }
            Self::ObjectType(wanted) => Ok(object_type == *wanted),
            Self::TreeDepth(limit) => Ok(object_type == ObjectType::Commit || depth < *limit),
            Self::Sparse(patterns) => Ok(object_type != ObjectType::Blob
                && object_type != ObjectType::Tree
                || rev_list_sparse_patterns_include(patterns, path, object_type)),
            Self::Combine(filters) => {
                for filter in filters {
                    if !filter.includes_object(object_type, oid, path, size, depth)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            Self::SparseOid(_) => unreachable!("sparse:oid filter must be resolved before use"),
        }
    }
}

fn rev_list_should_print_commit(
    record: &sley_rev::CommitRecord,
    filter: &RevListObjectFilter,
    tip_oids: &HashSet<ObjectId>,
) -> bool {
    filter
        .includes_object(ObjectType::Commit, &record.oid, &[], None, 0)
        .unwrap_or(true)
        || tip_oids.contains(&record.oid)
}

fn rev_list_decode_sub_filter(raw: &str) -> Result<String> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut idx = 0usize;
    while idx < bytes.len() {
        match bytes[idx] {
            b'@' | b'`' | b'~' => {
                eprintln!(
                    "fatal: must escape char in sub-filter-spec: '{}'",
                    bytes[idx] as char
                );
                return Err(GitError::Exit(128));
            }
            b'%' => {
                let Some(high) = bytes
                    .get(idx + 1)
                    .and_then(|byte| (*byte as char).to_digit(16))
                else {
                    eprintln!("fatal: invalid filter-spec");
                    return Err(GitError::Exit(128));
                };
                let Some(low) = bytes
                    .get(idx + 2)
                    .and_then(|byte| (*byte as char).to_digit(16))
                else {
                    eprintln!("fatal: invalid filter-spec");
                    return Err(GitError::Exit(128));
                };
                out.push((high * 16 + low) as u8);
                idx += 3;
            }
            byte => {
                out.push(byte);
                idx += 1;
            }
        }
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

fn rev_list_sparse_patterns_include(
    patterns: &[Vec<u8>],
    path: &[u8],
    object_type: ObjectType,
) -> bool {
    if path.is_empty() {
        return object_type == ObjectType::Tree;
    }
    patterns.iter().any(|pattern| {
        if pattern.ends_with(b"/") {
            let dir = &pattern[..pattern.len() - 1];
            path == dir || path.starts_with(pattern)
        } else {
            path == pattern
        }
    })
}

fn rev_list_selected_tag_objects(
    selected: &[&sley_rev::CommitRecord],
    tag_objects: &[RevListTagObject],
) -> Vec<RevListObject> {
    let selected_oids = selected
        .iter()
        .map(|record| record.oid)
        .collect::<HashSet<_>>();
    let mut seen = HashSet::new();
    tag_objects
        .iter()
        .filter(|tag_object| selected_oids.contains(&tag_object.commit))
        .filter(|tag_object| seen.insert(tag_object.object.oid))
        .map(|tag_object| tag_object.object.clone())
        .collect()
}

fn rev_list_objects(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    records: &[&sley_rev::CommitRecord],
    excluded: &HashSet<ObjectId>,
    exclude_objects: &[RevListObject],
    filter: &RevListObjectFilter,
    collect_omitted: bool,
    missing_action: RevListMissingAction,
) -> Result<(Vec<RevListObject>, Vec<RevListObject>, Vec<RevListObject>)> {
    if !collect_omitted && filter.tree_depth_limit() == Some(0) {
        return Ok((Vec::new(), Vec::new(), Vec::new()));
    }
    let mut seen = HashSet::new();
    for oid in excluded {
        let object = db.read_object(oid)?;
        if object.object_type != ObjectType::Commit {
            continue;
        }
        let commit = Commit::parse_ref(format, &object.body)?;
        rev_list_mark_tree_objects(db, format, &commit.tree, &mut seen)?;
    }
    for object in exclude_objects {
        match object.object_type {
            Some(ObjectType::Tree) => {
                rev_list_mark_tree_objects(db, format, &object.oid, &mut seen)?
            }
            _ => {
                seen.insert(object.oid);
            }
        }
    }
    let mut state = RevListObjectState::default();
    let walk = RevListObjectWalk {
        db,
        format,
        filter,
        collect_omitted,
        missing_action,
    };
    for record in records {
        rev_list_collect_tree_objects(
            &walk,
            &record.commit.tree,
            Vec::new(),
            &mut seen,
            &mut state,
            filter.tree_depth_limit(),
            0,
        )?;
    }
    Ok((state.objects, state.omitted, state.missing))
}

struct RevListObjectWalk<'a> {
    db: &'a FileObjectDatabase,
    format: ObjectFormat,
    filter: &'a RevListObjectFilter,
    collect_omitted: bool,
    missing_action: RevListMissingAction,
}

#[derive(Default)]
struct RevListObjectState {
    objects: Vec<RevListObject>,
    omitted: Vec<RevListObject>,
    missing: Vec<RevListObject>,
    emitted_oids: HashSet<ObjectId>,
    omitted_oids: HashSet<ObjectId>,
    missing_oids: HashSet<ObjectId>,
}

fn rev_list_mark_tree_objects(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    seen: &mut HashSet<ObjectId>,
) -> Result<()> {
    if !seen.insert(*tree_oid) {
        return Ok(());
    }
    let object = db.read_object(tree_oid)?;
    if object.object_type != ObjectType::Tree {
        return Err(GitError::InvalidObject(format!(
            "expected tree {tree_oid}, found {}",
            object.object_type.as_str()
        )));
    }
    for entry in TreeEntries::new(format, &object.body) {
        let entry = entry?;
        if tree_entry_object_type(entry.mode) == ObjectType::Tree {
            rev_list_mark_tree_objects(db, format, &entry.oid, seen)?;
        } else {
            seen.insert(entry.oid);
        }
    }
    Ok(())
}

fn rev_list_collect_tree_objects(
    walk: &RevListObjectWalk<'_>,
    tree_oid: &ObjectId,
    path: Vec<u8>,
    seen: &mut HashSet<ObjectId>,
    state: &mut RevListObjectState,
    tree_depth: Option<usize>,
    depth: usize,
) -> Result<()> {
    if seen.contains(tree_oid) {
        return Ok(());
    }
    let object = match walk.db.read_object(tree_oid) {
        Ok(object) => object,
        Err(err) => {
            return rev_list_handle_missing_object(
                walk,
                tree_oid,
                &path,
                Some(ObjectType::Tree),
                state,
                err,
            );
        }
    };
    seen.insert(*tree_oid);
    if object.object_type != ObjectType::Tree {
        return Err(GitError::InvalidObject(format!(
            "expected tree {tree_oid}, found {}",
            object.object_type.as_str()
        )));
    }
    rev_list_record_filter_decision(walk, ObjectType::Tree, tree_oid, &path, None, depth, state)?;
    if tree_depth == Some(1) {
        if walk.collect_omitted {
            rev_list_collect_omitted_tree_contents(walk, tree_oid, &path, state)?;
        } else if env::var_os("GIT_TRACE").is_some() {
            eprintln!(
                "Skipping contents of tree {}...",
                String::from_utf8_lossy(&path)
            );
        }
        return Ok(());
    }
    for entry in TreeEntries::new(walk.format, &object.body) {
        let entry = entry?;
        let entry_path = rev_list_join_object_path(&path, entry.name);
        let entry_type = tree_entry_object_type(entry.mode);
        if entry_type == ObjectType::Tree {
            rev_list_collect_tree_objects(
                walk,
                &entry.oid,
                entry_path,
                seen,
                state,
                tree_depth.map(|depth| depth.saturating_sub(1)),
                depth + 1,
            )?;
        } else {
            if !seen.insert(entry.oid) {
                continue;
            }
            let size = if entry_type == ObjectType::Blob
                && (rev_list_filter_needs_blob_size(walk.filter)
                    || !matches!(
                        walk.missing_action,
                        RevListMissingAction::AllowAny | RevListMissingAction::AllowPromisor
                    )) {
                let object = match walk.db.read_object(&entry.oid) {
                    Ok(object) => object,
                    Err(err) => {
                        rev_list_handle_missing_object(
                            walk,
                            &entry.oid,
                            &entry_path,
                            Some(entry_type),
                            state,
                            err,
                        )?;
                        continue;
                    }
                };
                Some(object.body.len())
            } else {
                None
            };
            if matches!(
                walk.missing_action,
                RevListMissingAction::AllowAny | RevListMissingAction::AllowPromisor
            ) && size.is_none()
                && !walk.db.contains(&entry.oid)?
            {
                rev_list_handle_missing_object(
                    walk,
                    &entry.oid,
                    &entry_path,
                    Some(entry_type),
                    state,
                    GitError::object_not_found(entry.oid),
                )?;
                continue;
            }
            rev_list_record_filter_decision(
                walk,
                entry_type,
                &entry.oid,
                &entry_path,
                size,
                depth + 1,
                state,
            )?;
        }
    }
    Ok(())
}

fn rev_list_filter_needs_blob_size(filter: &RevListObjectFilter) -> bool {
    match filter {
        RevListObjectFilter::BlobLimit(_) => true,
        RevListObjectFilter::Combine(filters) => {
            filters.iter().any(rev_list_filter_needs_blob_size)
        }
        _ => false,
    }
}

fn rev_list_record_filter_decision(
    walk: &RevListObjectWalk<'_>,
    object_type: ObjectType,
    oid: &ObjectId,
    path: &[u8],
    size: Option<usize>,
    depth: usize,
    state: &mut RevListObjectState,
) -> Result<()> {
    if walk
        .filter
        .includes_object(object_type, oid, path, size, depth)?
    {
        if state.emitted_oids.insert(*oid) {
            state.objects.push(RevListObject {
                oid: *oid,
                name: path.to_vec(),
                object_type: Some(object_type),
            });
        }
        state.omitted_oids.remove(oid);
        state.omitted.retain(|object| object.oid != *oid);
    } else if walk.collect_omitted
        && !state.emitted_oids.contains(oid)
        && state.omitted_oids.insert(*oid)
    {
        state.omitted.push(RevListObject {
            oid: *oid,
            name: Vec::new(),
            object_type: Some(object_type),
        });
    }
    Ok(())
}

fn rev_list_collect_omitted_tree_contents(
    walk: &RevListObjectWalk<'_>,
    tree_oid: &ObjectId,
    path: &[u8],
    state: &mut RevListObjectState,
) -> Result<()> {
    let object = match walk.db.read_object(tree_oid) {
        Ok(object) => object,
        Err(err) => {
            return rev_list_handle_missing_object(
                walk,
                tree_oid,
                path,
                Some(ObjectType::Tree),
                state,
                err,
            );
        }
    };
    if object.object_type != ObjectType::Tree {
        return Ok(());
    }
    for entry in TreeEntries::new(walk.format, &object.body) {
        let entry = entry?;
        let entry_path = rev_list_join_object_path(path, entry.name);
        let entry_type = tree_entry_object_type(entry.mode);
        rev_list_record_filter_decision(
            walk,
            entry_type,
            &entry.oid,
            &entry_path,
            None,
            usize::MAX,
            state,
        )?;
        if entry_type == ObjectType::Tree {
            rev_list_collect_omitted_tree_contents(walk, &entry.oid, &entry_path, state)?;
        }
    }
    Ok(())
}

fn rev_list_handle_missing_object(
    walk: &RevListObjectWalk<'_>,
    oid: &ObjectId,
    path: &[u8],
    object_type: Option<ObjectType>,
    state: &mut RevListObjectState,
    err: GitError,
) -> Result<()> {
    match walk.missing_action {
        RevListMissingAction::Error => Err(err),
        RevListMissingAction::AllowAny | RevListMissingAction::AllowPromisor => Ok(()),
        RevListMissingAction::Print | RevListMissingAction::PrintInfo => {
            if state.missing_oids.insert(*oid) {
                state.missing.push(RevListObject {
                    oid: *oid,
                    name: path.to_vec(),
                    object_type,
                });
            }
            Ok(())
        }
    }
}

fn rev_list_join_object_path(prefix: &[u8], name: &[u8]) -> Vec<u8> {
    if prefix.is_empty() {
        return name.to_vec();
    }
    let mut path = Vec::with_capacity(prefix.len() + 1 + name.len());
    path.extend_from_slice(prefix);
    path.push(b'/');
    path.extend_from_slice(name);
    path
}

fn write_rev_list_object_line(
    object: &RevListObject,
    object_names: bool,
    nul_terminated: bool,
) -> Result<()> {
    let mut stdout = io::stdout();
    write!(stdout, "{}", object.oid)?;
    if nul_terminated {
        stdout.write_all(&[0])?;
        if object_names && !object.name.is_empty() {
            stdout.write_all(b"path=")?;
            stdout.write_all(&object.name)?;
            stdout.write_all(&[0])?;
        }
        return Ok(());
    }
    if object_names {
        stdout.write_all(b" ")?;
        stdout.write_all(&object.name)?;
    }
    stdout.write_all(b"\n")?;
    Ok(())
}

fn write_rev_list_omitted_object_line(object: &RevListObject, nul_terminated: bool) -> Result<()> {
    let mut stdout = io::stdout();
    write!(stdout, "~{}", object.oid)?;
    stdout.write_all(if nul_terminated { b"\0" } else { b"\n" })?;
    Ok(())
}

fn write_rev_list_missing_object_line(
    object: &RevListObject,
    nul_terminated: bool,
    print_info: bool,
) -> Result<()> {
    let mut stdout = io::stdout();
    if nul_terminated {
        if print_info {
            write!(stdout, "{}", object.oid)?;
            stdout.write_all(b"\0missing=yes\0")?;
            if !object.name.is_empty() {
                stdout.write_all(b"path=")?;
                stdout.write_all(&object.name)?;
                stdout.write_all(b"\0")?;
            }
            if let Some(object_type) = object.object_type {
                write!(stdout, "type={}", object_type.as_str())?;
                stdout.write_all(b"\0")?;
            }
        } else {
            write!(stdout, "?{}", object.oid)?;
            stdout.write_all(b"\0")?;
        }
        return Ok(());
    }
    write!(stdout, "?{}", object.oid)?;
    if print_info {
        if !object.name.is_empty() {
            stdout.write_all(b" path=")?;
            stdout.write_all(&rev_list_quote_missing_path(&object.name))?;
        }
        if let Some(object_type) = object.object_type {
            write!(stdout, " type={}", object_type.as_str())?;
        }
    }
    stdout.write_all(b"\n")?;
    Ok(())
}

fn rev_list_quote_missing_path(path: &[u8]) -> Vec<u8> {
    let needs_quote = path.iter().any(|byte| {
        byte.is_ascii_whitespace() || matches!(*byte, b'"' | b'\\') || !byte.is_ascii_graphic()
    });
    if !needs_quote {
        return path.to_vec();
    }
    let mut out = Vec::with_capacity(path.len() + 2);
    out.push(b'"');
    for byte in path {
        match *byte {
            b'"' => out.extend_from_slice(br#"\""#),
            b'\\' => out.extend_from_slice(br#"\\"#),
            b'\n' => out.extend_from_slice(br#"\n"#),
            b'\t' => out.extend_from_slice(br#"\t"#),
            other => out.push(other),
        }
    }
    out.push(b'"');
    out
}

fn rev_list_disk_usage(
    git_dir: &Path,
    records: &[&sley_rev::CommitRecord],
    boundary_records: &[sley_rev::CommitRecord],
    tag_objects: &[RevListObject],
    objects: &[RevListObject],
    human_readable: bool,
) -> Result<String> {
    let mut seen = HashSet::new();
    let mut size = 0u64;
    for oid in records
        .iter()
        .map(|record| &record.oid)
        .chain(boundary_records.iter().map(|record| &record.oid))
        .chain(tag_objects.iter().map(|object| &object.oid))
        .chain(objects.iter().map(|object| &object.oid))
    {
        if seen.insert(oid)
            && let Some(object_size) = for_each_ref_loose_object_disk_size(git_dir, oid)?
        {
            size += object_size;
        }
    }
    if human_readable {
        Ok(count_objects_human_bytes(size))
    } else {
        Ok(size.to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RevListWalkMode {
    Walk,
    NoWalkSorted,
    NoWalkUnsorted,
}

fn rev_list_start_from_oid(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    oid: ObjectId,
    ignore_missing: bool,
) -> Result<Option<RevListStart>> {
    let object = match db.read_object(&oid) {
        Ok(object) => object,
        Err(_) if ignore_missing => return Ok(None),
        Err(err) => return Err(err),
    };
    let tag_object = if object.object_type == ObjectType::Tag {
        let tag = match Tag::parse(format, &object.body) {
            Ok(tag) => tag,
            Err(_) if ignore_missing => return Ok(None),
            Err(err) => return Err(err),
        };
        Some(RevListObject {
            oid,
            name: tag.name,
            object_type: Some(ObjectType::Tag),
        })
    } else {
        None
    };
    match sley_rev::peel_to_commit(db, format, &oid) {
        Ok(commit) => Ok(Some(RevListStart { commit, tag_object })),
        Err(_) if ignore_missing => Ok(None),
        Err(err) => Err(err),
    }
}

fn parse_rev_list_parent_count(value: &str) -> Result<usize> {
    value
        .parse::<usize>()
        .map_err(|_| GitError::Command(format!("invalid parent count {value}")))
}

fn parse_rev_list_abbrev(value: &str) -> Result<usize> {
    value
        .parse::<usize>()
        .map(|width| width.max(4))
        .map_err(|_| GitError::Command(format!("invalid abbrev length {value}")))
}

fn format_rev_list_oid(oid: &ObjectId, abbrev_commit: bool, abbrev_len: Option<usize>) -> String {
    let hex = oid.to_hex();
    if abbrev_commit && let Some(width) = abbrev_len {
        return hex[..width.min(hex.len())].to_string();
    }
    hex
}

// ===== --use-bitmap-index / --test-bitmap =====

/// The slice of a rev-list invocation the bitmap engine can answer; built only
/// after the eligibility allowlist passed.
struct RevListBitmapQuery<'a> {
    want_roots: &'a [ObjectId],
    exclude_tips: &'a [ObjectId],
    objects: bool,
    count: bool,
    max_count: Option<usize>,
    object_filter: RevListObjectFilter,
    filter_provided_objects: bool,
    unpacked: bool,
}

/// Sorted set-bit positions of `words & mask`.
fn rev_list_bitmap_and_positions(words: &[u64], mask: &[u64]) -> Vec<u32> {
    let mut positions = Vec::new();
    for (word_index, (word, mask_word)) in words.iter().zip(mask).enumerate() {
        let mut remaining = word & mask_word;
        while remaining != 0 {
            let bit = remaining.trailing_zeros();
            positions.push(word_index as u32 * 64 + bit);
            remaining &= remaining - 1;
        }
    }
    positions
}

fn rev_list_bitmap_and_count(words: &[u64], mask: &[u64]) -> usize {
    words
        .iter()
        .zip(mask)
        .map(|(word, mask_word)| (word & mask_word).count_ones() as usize)
        .sum()
}

/// Attempts to answer the query from the repository's pack bitmap. Returns
/// `Ok(false)` when no usable bitmap exists (the caller falls back to the
/// regular walk), `Ok(true)` after printing the result.
fn rev_list_try_bitmap(
    git_dir: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    query: &RevListBitmapQuery<'_>,
) -> Result<bool> {
    let objects_dir = sley_odb::repository_objects_dir(git_dir);
    let Some(bitmap) = sley_odb::load_pack_bitmap(&objects_dir, format)? else {
        return Ok(false);
    };

    // Upstream's classic-path guard: with haves, at least one must be in the
    // bitmapped pack or there is nothing to optimise here.
    if !query.exclude_tips.is_empty()
        && !query
            .exclude_tips
            .iter()
            .any(|oid| bitmap.pack_position(oid).is_some())
    {
        return Ok(false);
    }

    let mut result =
        sley_odb::bitmap_reachable(&bitmap, db, format, query.want_roots, query.objects)?;
    if !query.exclude_tips.is_empty() {
        let haves =
            sley_odb::bitmap_reachable(&bitmap, db, format, query.exclude_tips, query.objects)?;
        result.subtract(&haves);
    }
    rev_list_bitmap_apply_filter(&bitmap, db, &mut result, query)?;

    if query.unpacked {
        // Upstream filter_packed_objects_from_bitmap: everything in the
        // bitmapped pack is packed by definition; extended objects are kept
        // only when no pack (bitmapped or otherwise) holds them.
        result.words.iter_mut().for_each(|word| *word = 0);
        let packed = sley_odb::packed_object_ids(&objects_dir, format)?;
        result.extended.retain(|(oid, _)| !packed.contains(oid));
    }

    let commit_mask = bitmap.type_words(ObjectType::Commit);
    if query.count {
        let mut commit_count = rev_list_bitmap_and_count(&result.words, commit_mask)
            + result
                .extended
                .iter()
                .filter(|(_, object_type)| *object_type == ObjectType::Commit)
                .count();
        if let Some(max_count) = query.max_count {
            commit_count = commit_count.min(max_count);
        }
        let mut total = commit_count;
        if query.objects {
            for object_type in [ObjectType::Tree, ObjectType::Blob, ObjectType::Tag] {
                total += rev_list_bitmap_and_count(&result.words, bitmap.type_words(object_type))
                    + result
                        .extended
                        .iter()
                        .filter(|(_, extended_type)| *extended_type == object_type)
                        .count();
            }
        }
        println!("{total}");
        return Ok(true);
    }

    // Traversal output: per-type in pack order (commits, then trees, blobs,
    // tags when --objects), then the extended objects — bare oids throughout,
    // mirroring upstream's show_object_fast.
    let mut stdout = io::stdout();
    for position in rev_list_bitmap_and_positions(&result.words, commit_mask) {
        if let Some(oid) = bitmap.oid_at(position) {
            writeln!(stdout, "{oid}")?;
        }
    }
    if query.objects {
        for object_type in [ObjectType::Tree, ObjectType::Blob, ObjectType::Tag] {
            for position in
                rev_list_bitmap_and_positions(&result.words, bitmap.type_words(object_type))
            {
                if let Some(oid) = bitmap.oid_at(position) {
                    writeln!(stdout, "{oid}")?;
                }
            }
        }
    }
    for (oid, object_type) in &result.extended {
        if *object_type != ObjectType::Commit && !query.objects {
            continue;
        }
        writeln!(stdout, "{oid}")?;
    }
    stdout.flush()?;
    Ok(true)
}

/// Applies the object filter to a bitmap walk result, mirroring upstream's
/// `filter_bitmap`: bits are cleared per type bitmap, objects the caller named
/// directly (the want tips) are exempt unless `--filter-provided-objects`, and
/// the extended (non-pack) objects are filtered individually.
fn rev_list_bitmap_apply_filter(
    bitmap: &sley_odb::LoadedPackBitmap,
    db: &FileObjectDatabase,
    result: &mut sley_odb::BitmapWalkResult,
    query: &RevListBitmapQuery<'_>,
) -> Result<()> {
    if query.object_filter == RevListObjectFilter::None {
        return Ok(());
    }

    // Tip exemptions (upstream find_tip_objects).
    let word_count = result.words.len();
    let mut tip_words = vec![0u64; word_count];
    let mut tip_extended: HashSet<ObjectId> = HashSet::new();
    if !query.filter_provided_objects {
        for root in query.want_roots {
            match bitmap.pack_position(root) {
                Some(position) => {
                    let word = (position / 64) as usize;
                    if word < word_count {
                        tip_words[word] |= 1u64 << (position % 64);
                    }
                }
                None => {
                    tip_extended.insert(*root);
                }
            }
        }
    }

    let exclude_type = |result: &mut sley_odb::BitmapWalkResult, object_type: ObjectType| {
        for (word, (type_word, tip_word)) in result
            .words
            .iter_mut()
            .zip(bitmap.type_words(object_type).iter().zip(&tip_words))
        {
            *word &= !(type_word & !tip_word);
        }
        result.extended.retain(|(oid, extended_type)| {
            *extended_type != object_type || tip_extended.contains(oid)
        });
    };

    match query.object_filter {
        RevListObjectFilter::None => {}
        RevListObjectFilter::BlobNone => exclude_type(result, ObjectType::Blob),
        RevListObjectFilter::BlobLimit(limit) => {
            let blob_mask = bitmap.type_words(ObjectType::Blob);
            for position in rev_list_bitmap_and_positions(&result.words, blob_mask) {
                let word = (position / 64) as usize;
                let bit = 1u64 << (position % 64);
                if tip_words[word] & bit != 0 {
                    continue;
                }
                let Some(oid) = bitmap.oid_at(position) else {
                    continue;
                };
                if db.read_object(oid)?.body.len() >= limit {
                    result.words[word] &= !bit;
                }
            }
            let mut keep = Vec::with_capacity(result.extended.len());
            for (oid, extended_type) in result.extended.drain(..) {
                let keep_object = extended_type != ObjectType::Blob
                    || tip_extended.contains(&oid)
                    || db.read_object(&oid)?.body.len() < limit;
                if keep_object {
                    keep.push((oid, extended_type));
                }
            }
            result.extended = keep;
        }
        RevListObjectFilter::TreeDepth(0) => {
            exclude_type(result, ObjectType::Tree);
            exclude_type(result, ObjectType::Blob);
        }
        RevListObjectFilter::ObjectType(wanted) => {
            for object_type in [
                ObjectType::Commit,
                ObjectType::Tree,
                ObjectType::Blob,
                ObjectType::Tag,
            ] {
                if object_type != wanted {
                    exclude_type(result, object_type);
                }
            }
        }
        RevListObjectFilter::Combine(ref filters) => {
            for filter in filters {
                let subquery = RevListBitmapQuery {
                    want_roots: query.want_roots,
                    exclude_tips: query.exclude_tips,
                    objects: query.objects,
                    count: query.count,
                    max_count: query.max_count,
                    object_filter: filter.clone(),
                    filter_provided_objects: query.filter_provided_objects,
                    unpacked: query.unpacked,
                };
                rev_list_bitmap_apply_filter(bitmap, db, result, &subquery)?;
            }
        }
        // Excluded by the eligibility allowlist.
        RevListObjectFilter::TreeDepth(_)
        | RevListObjectFilter::SparseOid(_)
        | RevListObjectFilter::Sparse(_) => unreachable!("unsupported filters fall back"),
    }
    Ok(())
}

/// `git rev-list --test-bitmap <commit>`: verify the stored bitmap for the
/// commit against a real reachability walk (upstream `test_bitmap_walk`).
fn rev_list_test_bitmap(
    git_dir: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    include_commits: &[ObjectId],
    exclude_tips: &[ObjectId],
) -> Result<()> {
    let objects_dir = sley_odb::repository_objects_dir(git_dir);
    let Some(bitmap) = sley_odb::load_pack_bitmap(&objects_dir, format)? else {
        eprintln!("fatal: failed to load bitmap indexes");
        return Err(GitError::Exit(128));
    };
    if include_commits.len() != 1 || !exclude_tips.is_empty() {
        eprintln!("fatal: you must specify exactly one commit to test");
        return Err(GitError::Exit(128));
    }
    let tip = include_commits[0];
    eprintln!(
        "Bitmap v1 test ({} entries loaded)",
        bitmap.bitmapped_commits().count()
    );
    let Some(stored) = bitmap.bitmap_for_commit(&tip) else {
        eprintln!("fatal: commit '{tip}' doesn't have an indexed bitmap");
        return Err(GitError::Exit(128));
    };
    let stored = std::sync::Arc::clone(stored);
    eprintln!("Found bitmap for '{tip}'. {} bits", bitmap.object_count());
    let walked = sley_odb::bitmap_reachable(&bitmap, db, format, &[tip], true)?;
    if walked.extended.is_empty() && walked.words == *stored {
        eprintln!("OK!");
        Ok(())
    } else {
        eprintln!("fatal: mismatch in bitmap results");
        Err(GitError::Exit(128))
    }
}
