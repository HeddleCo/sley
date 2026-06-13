//! Extracted from the crate root (sley#8 phase 1) — code motion only.

// A glob of the crate root brings every shared helper/type into scope via
// descendant-privacy; see commands::stash for the rationale.
use crate::*;

pub(crate) fn cmd_rev_list(args: &[String]) -> Result<()> {
    let mut includes = Vec::new();
    let mut excludes = Vec::new();
    let mut linear_ranges = Vec::new();
    let mut symmetric_ranges = Vec::new();
    let mut stdin_revisions = Vec::new();
    let mut default_revision = None;
    let mut max_count = None;
    let mut skip_count = 0usize;
    let mut max_age = None;
    let mut min_age = None;
    let mut reverse = false;
    let mut parents = false;
    let mut children = false;
    let mut count = false;
    let mut ordering = RevListOrdering::Default;
    let mut walk_mode = RevListWalkMode::Walk;
    let mut ref_selectors = Vec::new();
    let mut pending_ref_exclude_patterns = Vec::new();
    let mut pending_hidden_refs = None;
    let mut first_parent = false;
    let mut min_parents = None;
    let mut max_parents = None;
    let mut abbrev_commit = false;
    let mut abbrev_len = Some(7usize);
    let mut left_right = false;
    let mut side_filter = None;
    let mut timestamp = false;
    let mut quiet = false;
    let mut nul_terminated = false;
    let mut objects = false;
    let mut objects_edge = false;
    let mut object_filter = RevListObjectFilter::None;
    let mut filter_provided_objects = false;
    let mut boundary = false;
    let mut disk_usage = None;
    let mut object_names = true;
    let mut read_stdin = false;
    let mut header = false;
    let mut pretty = RevListPretty::Default;
    let mut preset_oneline = false;
    let mut ignore_missing = false;
    let mut author_patterns = Vec::new();
    let mut committer_patterns = Vec::new();
    let mut grep_patterns = Vec::new();
    let mut grep_all_match = false;
    let mut invert_grep = false;
    let mut regexp_ignore_case = false;
    let mut regexp_mode = SimpleLogRegexMode::Basic;
    let mut date_mode = DateMode::Default;
    let mut positional_only = false;
    let mut not = false;
    let mut pathspecs: Vec<String> = Vec::new();
    let mut full_history = false;
    let mut use_bitmap_index = false;
    let mut test_bitmap = false;
    let mut unpacked = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if positional_only {
            // After `--`, every remaining argument is a pathspec, never a
            // revision (git: `setup_revisions` switches to prune_data here).
            pathspecs.push(arg.to_string());
            continue;
        }
        match arg.as_str() {
            "--" => positional_only = true,
            "--full-history" => full_history = true,
            "--not" => not = !not,
            "--default" => {
                default_revision = Some(
                    iter.next()
                        .ok_or_else(|| GitError::Command("--default requires a value".into()))?,
                );
            }
            "--reverse" => reverse = true,
            "--parents" => parents = true,
            "--no-parents" => parents = false,
            "--children" => children = true,
            "--count" => count = true,
            "--no-count" => count = false,
            "--topo-order" => ordering = RevListOrdering::Topo,
            "--date-order" => ordering = RevListOrdering::Date,
            "--author-date-order" => ordering = RevListOrdering::AuthorDate,
            "--sparse"
            | "--dense"
            | "--remove-empty"
            | "--simplify-merges"
            | "--show-pulls"
            | "--exclude-promisor-objects" => {}
            // No effect on the regular walk yet (pre-existing behaviour); the
            // bitmap path filters packed objects out of its result.
            "--unpacked" => unpacked = true,
            "--exclude-hidden" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--exclude-hidden requires a value".into()))?;
                pending_hidden_refs = Some(parse_rev_list_exclude_hidden(value)?);
            }
            value if value.starts_with("--exclude-hidden=") => {
                pending_hidden_refs = Some(parse_rev_list_exclude_hidden(
                    &value["--exclude-hidden=".len()..],
                )?);
            }
            "--no-walk" | "--no-walk=sorted" => walk_mode = RevListWalkMode::NoWalkSorted,
            "--no-walk=unsorted" => walk_mode = RevListWalkMode::NoWalkUnsorted,
            "--do-walk" => walk_mode = RevListWalkMode::Walk,
            "--all" => {
                ref_selectors.push(RevListRefSelector::All {
                    not,
                    excludes: mem::take(&mut pending_ref_exclude_patterns),
                    hidden: pending_hidden_refs.take(),
                });
            }
            "--no-all" => {
                ref_selectors.retain(|selector| !matches!(selector, RevListRefSelector::All { .. }))
            }
            "--exclude" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--exclude requires a value".into()))?;
                pending_ref_exclude_patterns.push(value.to_string());
            }
            value if value.starts_with("--exclude=") => {
                pending_ref_exclude_patterns.push(value["--exclude=".len()..].to_string());
            }
            "--glob" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--glob requires a value".into()))?;
                ref_selectors.push(RevListRefSelector::Glob {
                    not,
                    pattern: value.to_string(),
                    excludes: mem::take(&mut pending_ref_exclude_patterns),
                    hidden: pending_hidden_refs.take(),
                });
            }
            value if value.starts_with("--glob=") => {
                ref_selectors.push(RevListRefSelector::Glob {
                    not,
                    pattern: value["--glob=".len()..].to_string(),
                    excludes: mem::take(&mut pending_ref_exclude_patterns),
                    hidden: pending_hidden_refs.take(),
                });
            }
            "--branches" => ref_selectors.push(RevListRefSelector::Branches {
                not,
                patterns: Vec::new(),
                include_all: true,
                excludes: mem::take(&mut pending_ref_exclude_patterns),
                hidden: {
                    if pending_hidden_refs.is_some() {
                        return rev_list_exclude_hidden_selector_error("--branches");
                    }
                    None
                },
            }),
            value if value.starts_with("--branches=") => {
                ref_selectors.push(RevListRefSelector::Branches {
                    not,
                    patterns: vec![value["--branches=".len()..].to_string()],
                    include_all: false,
                    excludes: mem::take(&mut pending_ref_exclude_patterns),
                    hidden: {
                        if pending_hidden_refs.is_some() {
                            return rev_list_exclude_hidden_selector_error("--branches");
                        }
                        None
                    },
                });
            }
            "--tags" => ref_selectors.push(RevListRefSelector::Tags {
                not,
                patterns: Vec::new(),
                include_all: true,
                excludes: mem::take(&mut pending_ref_exclude_patterns),
                hidden: {
                    if pending_hidden_refs.is_some() {
                        return rev_list_exclude_hidden_selector_error("--tags");
                    }
                    None
                },
            }),
            value if value.starts_with("--tags=") => {
                ref_selectors.push(RevListRefSelector::Tags {
                    not,
                    patterns: vec![value["--tags=".len()..].to_string()],
                    include_all: false,
                    excludes: mem::take(&mut pending_ref_exclude_patterns),
                    hidden: {
                        if pending_hidden_refs.is_some() {
                            return rev_list_exclude_hidden_selector_error("--tags");
                        }
                        None
                    },
                });
            }
            "--remotes" => ref_selectors.push(RevListRefSelector::Remotes {
                not,
                patterns: Vec::new(),
                include_all: true,
                excludes: mem::take(&mut pending_ref_exclude_patterns),
                hidden: {
                    if pending_hidden_refs.is_some() {
                        return rev_list_exclude_hidden_selector_error("--remotes");
                    }
                    None
                },
            }),
            value if value.starts_with("--remotes=") => {
                ref_selectors.push(RevListRefSelector::Remotes {
                    not,
                    patterns: vec![value["--remotes=".len()..].to_string()],
                    include_all: false,
                    excludes: mem::take(&mut pending_ref_exclude_patterns),
                    hidden: {
                        if pending_hidden_refs.is_some() {
                            return rev_list_exclude_hidden_selector_error("--remotes");
                        }
                        None
                    },
                });
            }
            "--first-parent" => first_parent = true,
            "--no-first-parent" => first_parent = false,
            "--abbrev-commit" => abbrev_commit = true,
            "--no-abbrev-commit" => abbrev_commit = false,
            "--no-abbrev" => abbrev_len = None,
            "--left-right" => left_right = true,
            "--left-only" => side_filter = Some('<'),
            "--right-only" => side_filter = Some('>'),
            "--timestamp" => timestamp = true,
            "--quiet" => quiet = true,
            "--ignore-missing" => ignore_missing = true,
            "--no-ignore-missing" => ignore_missing = false,
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
            "--objects-edge" => {
                objects = true;
                objects_edge = true;
            }
            "--objects-edge-aggressive" => {
                objects = true;
                objects_edge = true;
            }
            "--no-filter" => object_filter = RevListObjectFilter::None,
            // Apply the object filter to the directly-provided tip objects too, not just the
            // objects reached by the walk. For an `object:type` filter this means a provided
            // commit tip is itself dropped when it is not the requested type.
            "--filter-provided-objects" => filter_provided_objects = true,
            "--filter=blob:none" => object_filter = RevListObjectFilter::BlobNone,
            value if value.starts_with("--filter=tree:") => {
                object_filter = RevListObjectFilter::TreeDepth(parse_rev_list_tree_depth(
                    &value["--filter=tree:".len()..],
                )?)
            }
            value if value.starts_with("--filter=blob:limit=") => {
                object_filter = RevListObjectFilter::BlobLimit(parse_rev_list_blob_limit(
                    &value["--filter=blob:limit=".len()..],
                )?)
            }
            value if value.starts_with("--filter=object:type=") => {
                object_filter = RevListObjectFilter::ObjectType(parse_rev_list_object_type_filter(
                    &value["--filter=object:type=".len()..],
                )?)
            }
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
            "-n" | "--max-count" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command(format!("{arg} requires a value")))?;
                max_count = Some(parse_log_count(value)?);
            }
            value if value.starts_with("--max-count=") => {
                let value = value
                    .strip_prefix("--max-count=")
                    .ok_or_else(|| GitError::Command("--max-count requires a value".into()))?;
                max_count = Some(parse_log_count(value)?);
            }
            "--skip" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--skip requires a value".into()))?;
                skip_count = parse_rev_list_skip(value)?;
            }
            value if value.starts_with("--skip=") => {
                let value = value
                    .strip_prefix("--skip=")
                    .ok_or_else(|| GitError::Command("--skip requires a value".into()))?;
                skip_count = parse_rev_list_skip(value)?;
            }
            "--max-age" => {
                let value = iter.next().ok_or_else(log_max_age_requires_value_error)?;
                max_age = Some(parse_rev_list_timestamp(value)?);
            }
            value if value.starts_with("--max-age=") => {
                let value = value
                    .strip_prefix("--max-age=")
                    .ok_or_else(|| GitError::Command("--max-age requires a value".into()))?;
                max_age = Some(parse_rev_list_timestamp(value)?);
            }
            "--min-age" => {
                let value = iter.next().ok_or_else(log_min_age_requires_value_error)?;
                min_age = Some(parse_rev_list_timestamp(value)?);
            }
            value if value.starts_with("--min-age=") => {
                let value = value
                    .strip_prefix("--min-age=")
                    .ok_or_else(|| GitError::Command("--min-age requires a value".into()))?;
                min_age = Some(parse_rev_list_timestamp(value)?);
            }
            "--since" | "--after" => {
                let value = iter
                    .next()
                    .ok_or_else(|| log_date_cutoff_requires_value_error(arg))?;
                max_age = Some(log_parse_date_cutoff(value)?);
            }
            value if value.starts_with("--since=") => {
                max_age = Some(log_parse_date_cutoff(&value["--since=".len()..])?);
            }
            value if value.starts_with("--after=") => {
                max_age = Some(log_parse_date_cutoff(&value["--after=".len()..])?);
            }
            "--until" | "--before" => {
                let value = iter
                    .next()
                    .ok_or_else(|| log_date_cutoff_requires_value_error(arg))?;
                min_age = Some(log_parse_date_cutoff(value)?);
            }
            value if value.starts_with("--until=") => {
                min_age = Some(log_parse_date_cutoff(&value["--until=".len()..])?);
            }
            value if value.starts_with("--before=") => {
                min_age = Some(log_parse_date_cutoff(&value["--before=".len()..])?);
            }
            value if value.starts_with("-n") && value.len() > 2 => {
                max_count = Some(parse_log_count(&value[2..])?);
            }
            value
                if value.starts_with('-')
                    && value[1..].bytes().all(|byte| byte.is_ascii_digit()) =>
            {
                max_count = Some(parse_log_count(&value[1..])?);
            }
            value if value.starts_with('^') && value.len() > 1 => {
                if not {
                    includes.push(value[1..].to_string());
                } else {
                    excludes.push(value[1..].to_string());
                }
            }
            value if value.starts_with('-') => {
                return Err(GitError::Command(format!(
                    "unsupported rev-list option {value}"
                )));
            }
            value => add_rev_list_revision_arg(
                value,
                not,
                &mut includes,
                &mut excludes,
                &mut linear_ranges,
                &mut symmetric_ranges,
            )?,
        }
    }
    if read_stdin {
        let mut input = String::new();
        io::stdin().read_to_string(&mut input)?;
        stdin_revisions.extend(
            input
                .lines()
                .filter(|line| !line.is_empty())
                .map(str::to_string),
        );
        let mut stdin_not = false;
        for line in &stdin_revisions {
            if line == "--not" {
                stdin_not = !stdin_not;
                continue;
            }
            add_rev_list_revision_arg(
                line,
                stdin_not,
                &mut includes,
                &mut excludes,
                &mut linear_ranges,
                &mut symmetric_ranges,
            )?;
        }
    }
    if includes.is_empty()
        && excludes.is_empty()
        && linear_ranges.is_empty()
        && symmetric_ranges.is_empty()
        && ref_selectors.is_empty()
        && let Some(default_revision) = default_revision
    {
        add_rev_list_revision_arg(
            default_revision,
            false,
            &mut includes,
            &mut excludes,
            &mut linear_ranges,
            &mut symmetric_ranges,
        )?;
    }
    if includes.is_empty()
        && excludes.is_empty()
        && linear_ranges.is_empty()
        && symmetric_ranges.is_empty()
        && ref_selectors.is_empty()
    {
        return Err(GitError::Command(
            "rev-list currently requires at least one revision".into(),
        ));
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
    let hidden_refs = RevListHiddenRefs::from_config(&config);
    let db = FileObjectDatabase::from_git_dir(&git_dir, format);
    let mut include_commits = Vec::new();
    let mut start_tag_objects = Vec::new();
    // Tips that resolve to non-commit objects (git's pending-object model):
    // a provided blob is emitted directly in --objects mode (exempt from
    // filters unless --filter-provided-objects), silently dropped otherwise;
    // any other non-commit tip is additionally accepted under
    // --use-bitmap-index, where the bitmap traversal can start from it.
    let mut provided_objects: Vec<RevListObject> = Vec::new();
    let mut bitmap_object_tips: Vec<ObjectId> = Vec::new();
    for rev in includes {
        let start = match resolve_rev_list_start(&git_dir, &db, format, &rev, ignore_missing) {
            Ok(start) => start,
            Err(err) => {
                let Ok(oid) = resolve_revision(&git_dir, format, &rev) else {
                    return Err(err);
                };
                let Ok(object) = db.read_object(&oid) else {
                    return Err(err);
                };
                match object.object_type {
                    ObjectType::Blob if objects => {
                        // git names a pathy tip by its path component.
                        let name = rev
                            .split_once(':')
                            .map(|(_, path)| path.as_bytes().to_vec())
                            .unwrap_or_default();
                        provided_objects.push(RevListObject { oid, name });
                    }
                    ObjectType::Blob | ObjectType::Tree if !use_bitmap_index => {
                        // Without --objects, git silently ignores non-commit
                        // pending objects.
                    }
                    _ if use_bitmap_index => bitmap_object_tips.push(oid),
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
    let mut symmetric_excludes = Vec::new();
    for (left, right, not) in linear_ranges {
        let Some(left_oid) = resolve_rev_list_commit(&git_dir, &db, format, &left, ignore_missing)?
        else {
            continue;
        };
        let Some(right_oid) =
            resolve_rev_list_commit(&git_dir, &db, format, &right, ignore_missing)?
        else {
            continue;
        };
        if not {
            include_commits.push(left_oid);
            symmetric_excludes.push(right_oid);
        } else {
            symmetric_excludes.push(left_oid);
            include_commits.push(right_oid);
        }
    }
    let mut left_right_sides = HashMap::new();
    for (left, right, not) in symmetric_ranges {
        let Some(left_oid) = resolve_rev_list_commit(&git_dir, &db, format, &left, ignore_missing)?
        else {
            continue;
        };
        let Some(right_oid) =
            resolve_rev_list_commit(&git_dir, &db, format, &right, ignore_missing)?
        else {
            continue;
        };
        if (left_right || side_filter.is_some()) && !not {
            for record in rev_list_walk_commits(&db, format, [left_oid], first_parent)? {
                left_right_sides.entry(record.oid).or_insert('<');
            }
            for record in rev_list_walk_commits(&db, format, [right_oid], first_parent)? {
                left_right_sides.entry(record.oid).or_insert('>');
            }
        }
        let merge_bases = merge_bases(&git_dir, &db, format, &left_oid, &right_oid)?;
        if not {
            include_commits.extend(merge_bases);
            symmetric_excludes.push(left_oid);
            symmetric_excludes.push(right_oid);
        } else {
            include_commits.push(left_oid);
            include_commits.push(right_oid);
            symmetric_excludes.extend(merge_bases);
        }
    }
    if !ref_selectors.is_empty() {
        let store = FileRefStore::new(&git_dir, format);
        for reference in store.list_refs()? {
            let (include_ref, exclude_ref) =
                rev_list_ref_selection(&reference.name, &ref_selectors, &hidden_refs);
            if !include_ref && !exclude_ref {
                continue;
            }
            let RefTarget::Direct(oid) = reference.target else {
                continue;
            };
            if let Ok(Some(start)) = rev_list_start_from_oid(&db, format, oid, true) {
                if include_ref {
                    include_commits.push(start.commit);
                    if let Some(tag_object) = start.tag_object {
                        start_tag_objects.push(RevListTagObject {
                            commit: start.commit,
                            object: tag_object,
                        });
                    }
                }
                if exclude_ref {
                    symmetric_excludes.push(start.commit);
                }
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
    let mut exclude_tip_oids: Vec<ObjectId> = symmetric_excludes;
    for rev in excludes {
        let Some(oid) = resolve_rev_list_commit(&git_dir, &db, format, &rev, ignore_missing)?
        else {
            continue;
        };
        exclude_tip_oids.push(oid);
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
            && pathspecs.is_empty()
            && !full_history
            && !first_parent
            && !parents
            && !children
            && !boundary
            && !left_right
            && side_filter.is_none()
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
            && !matches!(object_filter, RevListObjectFilter::TreeDepth(depth) if depth > 0)
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
            object_filter,
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
            let keep = match object_filter {
                RevListObjectFilter::None => true,
                // tree:0 prunes blobs along with trees; deeper limits keep them.
                RevListObjectFilter::TreeDepth(depth) => depth > 0,
                RevListObjectFilter::BlobNone => false,
                RevListObjectFilter::BlobLimit(limit) => {
                    db.read_object(&object.oid)?.body.len() < limit
                }
                RevListObjectFilter::ObjectType(wanted) => wanted == ObjectType::Blob,
            };
            if keep {
                kept.push(object);
            }
        }
        provided_objects = kept;
    }

    let mut excluded = HashSet::new();
    for oid in exclude_tip_oids {
        for record in rev_list_walk_commits(&db, format, [oid], first_parent)? {
            excluded.insert(record.oid);
        }
    }
    // Commit-graph fast path: a plain commit listing (no flag that needs the parsed
    // commit object) walks via the commit-graph and reads zero commit objects. Any
    // commit-body-dependent mode falls through to the full walk below. The guard is
    // a strict allowlist — only flags whose handling needs solely oid+parents+time.
    let metadata_format = match &pretty {
        RevListPretty::Compiled { compiled, .. }
            if compiled.is_metadata_emitable()
                && compiled.uses_oid()
                && !compiled.uses_decorations() =>
        {
            Some(compiled)
        }
        _ => None,
    };
    if walk_mode == RevListWalkMode::Walk
        && matches!(ordering, RevListOrdering::Default | RevListOrdering::Date)
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
        && !timestamp
        && author_filters.is_empty()
        && committer_filters.is_empty()
        && grep_filters.is_empty()
        && max_age.is_none()
        && min_age.is_none()
    {
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
        let effective_abbrev_len = abbrev_commit.then_some(abbrev_len).flatten();
        for record in &selected {
            if let Some(compiled) = metadata_format {
                let mut line = Vec::with_capacity(compiled.estimated_line_capacity());
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
                        color: false,
                        output_encoding: "UTF-8",
                    },
                    &mut line,
                )?;
                stdout.write_all(&line)?;
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
    let commits = match walk_mode {
        RevListWalkMode::Walk => rev_list_walk_commits(&db, format, include_commits, first_parent)?,
        RevListWalkMode::NoWalkSorted | RevListWalkMode::NoWalkUnsorted => {
            rev_list_no_walk_commits(&db, format, include_commits)?
        }
    };
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
        if side_filter.is_some_and(|side| left_right_sides.get(&record.oid).copied() != Some(side))
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
    // Pathspec-limited / --full-history simplification: TREESAME-prune the
    // ordered set and rewrite parents past the dropped commits. Held in an
    // owned binding so `selected` (a Vec of references) can borrow from it.
    let simplified_storage;
    if !pathspecs.is_empty() || full_history {
        let pathspec = sley_rev::Pathspec::parse(
            pathspecs.iter().map(|p| p.as_bytes()),
            sley_rev::PathspecMatchMagic::default(),
        )
        .map_err(|err| GitError::Command(format!("bad pathspec: {err:?}")))?;
        let ordered_owned: Vec<sley_rev::CommitRecord> =
            selected.iter().map(|r| (*r).clone()).collect();
        simplified_storage = sley_rev::simplify_history(
            &db,
            format,
            ordered_owned,
            &pathspec,
            sley_rev::SimplifyOptions {
                full_history,
                first_parent,
            },
        )?;
        selected = simplified_storage.iter().collect();
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
            log_decoration_map(&git_dir, &db, format, LogDecorationMode::Short)?
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
    let mut selected_objects = if objects {
        rev_list_objects(&db, format, &selected, &excluded, object_filter)?
    } else {
        Vec::new()
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
            if left_right {
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
                    rev_list_should_print_commit(record, object_filter, &object_filter_tip_oids)
                })
                .filter(|record| left_right_sides.get(&record.oid).copied().unwrap_or('>') == '<')
                .count();
            let right_count = selected
                .iter()
                .filter(|record| {
                    rev_list_should_print_commit(record, object_filter, &object_filter_tip_oids)
                        && left_right_sides.get(&record.oid).copied().unwrap_or('>') != '<'
                })
                .count();
            println!("{left_count}\t{right_count}");
            return Ok(());
        }
        println!(
            "{}",
            selected
                .iter()
                .filter(|record| rev_list_should_print_commit(
                    record,
                    object_filter,
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
    if quiet {
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
    for record in selected {
        if !rev_list_should_print_commit(record, object_filter, &object_filter_tip_oids) {
            continue;
        }
        let left_right_prefix = left_right_sides.get(&record.oid).copied().unwrap_or('>');
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
                if left_right {
                    print!("{left_right_prefix}");
                }
                if oneline {
                    let RevListPretty::Compiled { compiled, .. } = &pretty else {
                        unreachable!("oneline requires compiled preset");
                    };
                    let format_context = LogFormatContext {
                        abbrev_len: abbrev_commit.then_some(abbrev_len).flatten(),
                        decorations: &decorations,
                        marker: left_right_prefix,
                        dialect: LogFormatDialect::RevList,
                        source: None,
                        date_mode: &date_mode,
                        source_oid: None,
                        describe: None,
                        color: false,
                        output_encoding: "UTF-8",
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
                left_right.then_some(left_right_prefix),
                parents,
                abbrev_commit,
                abbrev_len,
                timestamp,
            )?,
            RevListPretty::Compiled {
                compiled,
                commit_header: true,
            } => {
                write_rev_list_commit_header_line(
                    record,
                    left_right.then_some(left_right_prefix),
                    parents,
                    abbrev_commit,
                    abbrev_len,
                    timestamp,
                )?;
                print_log_format(
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
                        color: false,
                        output_encoding: "UTF-8",
                    },
                )?;
                println!();
            }
        }
    }
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
    io::stdout().flush()?;
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
    writeln!(stdout, "    {}", commit_subject(&record.commit.message))?;
    writeln!(stdout)?;
    Ok(())
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
                        color: false,
                        output_encoding: "UTF-8",
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
            print_log_format(
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
                    color: false,
                    output_encoding: "UTF-8",
                },
            )?;
            println!();
            Ok(())
        }
    }
}

#[derive(Clone)]
struct RevListObject {
    oid: ObjectId,
    name: Vec<u8>,
}

struct RevListStart {
    commit: ObjectId,
    tag_object: Option<RevListObject>,
}

struct RevListTagObject {
    commit: ObjectId,
    object: RevListObject,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RevListObjectFilter {
    None,
    BlobNone,
    BlobLimit(usize),
    ObjectType(ObjectType),
    TreeDepth(usize),
}

fn rev_list_should_print_commit(
    record: &sley_rev::CommitRecord,
    filter: RevListObjectFilter,
    tip_oids: &HashSet<ObjectId>,
) -> bool {
    !matches!(
        filter,
        RevListObjectFilter::ObjectType(ObjectType::Blob | ObjectType::Tree | ObjectType::Tag)
    ) || tip_oids.contains(&record.oid)
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
    filter: RevListObjectFilter,
) -> Result<Vec<RevListObject>> {
    if matches!(
        filter,
        RevListObjectFilter::TreeDepth(0)
            | RevListObjectFilter::ObjectType(ObjectType::Commit | ObjectType::Tag)
    ) {
        return Ok(Vec::new());
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
    let mut objects = Vec::new();
    let walk = RevListObjectWalk { db, format, filter };
    for record in records {
        rev_list_collect_tree_objects(
            &walk,
            &record.commit.tree,
            Vec::new(),
            &mut seen,
            &mut objects,
            rev_list_tree_depth_limit(filter),
        )?;
    }
    Ok(objects)
}

fn rev_list_tree_depth_limit(filter: RevListObjectFilter) -> Option<usize> {
    match filter {
        RevListObjectFilter::TreeDepth(depth) => Some(depth),
        RevListObjectFilter::ObjectType(ObjectType::Commit | ObjectType::Tag) => Some(0),
        _ => None,
    }
}

struct RevListObjectWalk<'a> {
    db: &'a FileObjectDatabase,
    format: ObjectFormat,
    filter: RevListObjectFilter,
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
    objects: &mut Vec<RevListObject>,
    tree_depth: Option<usize>,
) -> Result<()> {
    if !seen.insert(*tree_oid) {
        return Ok(());
    }
    if rev_list_object_filter_includes_object(walk.filter, ObjectType::Tree) {
        objects.push(RevListObject {
            oid: *tree_oid,
            name: path.clone(),
        });
    }
    if tree_depth == Some(1) {
        return Ok(());
    }
    let object = walk.db.read_object(tree_oid)?;
    if object.object_type != ObjectType::Tree {
        return Err(GitError::InvalidObject(format!(
            "expected tree {tree_oid}, found {}",
            object.object_type.as_str()
        )));
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
                objects,
                tree_depth.map(|depth| depth.saturating_sub(1)),
            )?;
        } else {
            if !seen.insert(entry.oid) {
                continue;
            }
            if !rev_list_object_filter_includes_object(walk.filter, entry_type) {
                continue;
            }
            if walk.filter == RevListObjectFilter::BlobNone {
                continue;
            }
            if let RevListObjectFilter::BlobLimit(limit) = walk.filter {
                let object = walk.db.read_object(&entry.oid)?;
                if object.body.len() >= limit {
                    continue;
                }
            }
            objects.push(RevListObject {
                oid: entry.oid,
                name: entry_path,
            });
        }
    }
    Ok(())
}

fn rev_list_object_filter_includes_object(
    filter: RevListObjectFilter,
    object_type: ObjectType,
) -> bool {
    match filter {
        RevListObjectFilter::ObjectType(filter_type) => filter_type == object_type,
        _ => true,
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

fn resolve_rev_list_commit(
    git_dir: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    rev: &str,
    ignore_missing: bool,
) -> Result<Option<ObjectId>> {
    let oid = match resolve_revision(git_dir, format, rev) {
        Ok(oid) => oid,
        Err(_) if ignore_missing => return Ok(None),
        Err(err) => return Err(err),
    };
    match sley_rev::peel_to_commit(db, format, &oid) {
        Ok(oid) => Ok(Some(oid)),
        Err(_) if ignore_missing => Ok(None),
        Err(err) => Err(err),
    }
}

fn resolve_rev_list_start(
    git_dir: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    rev: &str,
    ignore_missing: bool,
) -> Result<Option<RevListStart>> {
    let oid = match resolve_revision(git_dir, format, rev) {
        Ok(oid) => oid,
        Err(_) if ignore_missing => return Ok(None),
        Err(err) => return Err(err),
    };
    rev_list_start_from_oid(db, format, oid, ignore_missing)
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

fn parse_rev_list_skip(value: &str) -> Result<usize> {
    value
        .parse::<usize>()
        .map_err(|_| GitError::Command(format!("invalid skip count {value}")))
}

fn parse_rev_list_abbrev(value: &str) -> Result<usize> {
    value
        .parse::<usize>()
        .map(|width| width.max(4))
        .map_err(|_| GitError::Command(format!("invalid abbrev length {value}")))
}

fn parse_rev_list_timestamp(value: &str) -> Result<i64> {
    value
        .parse::<i64>()
        .map_err(|_| GitError::Command(format!("invalid timestamp {value}")))
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
        // Excluded by the eligibility allowlist.
        RevListObjectFilter::TreeDepth(_) => unreachable!("non-zero tree depth falls back"),
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
