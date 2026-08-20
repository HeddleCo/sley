//! Branch listing, sorting, formatting, and verbose output.
#![allow(clippy::expect_used)]

use crate::*;
use sley::plumbing::{sley_refs, sley_rev};

pub(super) struct BranchVerboseListOptions {
    pub(crate) mode: BranchListMode,
    pub(crate) patterns: Vec<String>,
    pub(crate) filters: BranchListFilters,
    pub(crate) ignore_case: bool,
    pub(crate) verbosity: usize,
    pub(crate) abbrev: Option<Option<usize>>,
    pub(crate) color: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum BranchColumnStyle {
    Column,
    Dense,
}

pub(super) struct BranchGeneralListOptions {
    pub(crate) mode: BranchListMode,
    pub(crate) patterns: Vec<String>,
    pub(crate) filters: BranchListFilters,
    pub(crate) ignore_case: bool,
    pub(crate) color: bool,
    pub(crate) column: Option<BranchColumnStyle>,
    pub(crate) sort: Option<BranchSort>,
}

#[derive(Clone, Default)]
pub(super) struct BranchListFilters {
    pub(crate) contains: Vec<String>,
    pub(crate) no_contains: Vec<String>,
    pub(crate) merged: Vec<String>,
    pub(crate) no_merged: Vec<String>,
}

impl BranchListFilters {
    pub(crate) fn is_empty(&self) -> bool {
        self.contains.is_empty()
            && self.no_contains.is_empty()
            && self.merged.is_empty()
            && self.no_merged.is_empty()
    }
}

#[derive(Clone, Copy)]
pub(super) enum BranchSort {
    Refname(bool),
    Version(bool),
    ObjectName(bool),
    ObjectType(bool),
    ObjectSize(bool),
    Date(ForEachRefDateSortField, bool),
    Upstream(bool),
    Push(bool),
    AheadBehind(ObjectId, bool),
}

pub(super) struct BranchFormatListOptions {
    pub(crate) mode: BranchListMode,
    pub(crate) patterns: Vec<String>,
    pub(crate) ignore_case: bool,
    pub(crate) color: bool,
    pub(crate) sort: Option<BranchSort>,
    pub(crate) format_spec: String,
    pub(crate) omit_empty: bool,
}
pub(super) fn run_branch_verbose_list_options(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    replace_objects: bool,
    options: BranchVerboseListOptions,
) -> Result<()> {
    if options.verbosity == 0 {
        return if options.color {
            print_branch_list_matching_colored(store, options.mode, &options.patterns)
        } else {
            print_branch_list_matching(store, options.mode, &options.patterns, options.ignore_case)
        };
    }
    print_branch_list_verbose(git_dir, format, store, replace_objects, options)
}
#[derive(Clone, Copy)]
pub(super) enum BranchListMode {
    Local,
    Remote,
    All,
}

pub(super) fn branch_refs_for_mode(
    store: &FileRefStore,
    mode: BranchListMode,
) -> Result<Vec<sley_refs::Ref>> {
    let scope = match mode {
        BranchListMode::Local => sley_refs::branch::BranchListScope::Local,
        BranchListMode::Remote => sley_refs::branch::BranchListScope::Remote,
        BranchListMode::All => sley_refs::branch::BranchListScope::All,
    };
    sley_refs::branch::list_branches(store, &sley_refs::branch::BranchListOptions::all_in(scope))
        .map(|outcome| outcome.refs)
        .map_err(sley_refs::branch::BranchOperationError::into_git_error)
}

pub(super) fn print_branch_list(store: &FileRefStore, mode: BranchListMode) -> Result<()> {
    print_branch_list_filtered(store, mode, |_, _| true)
}

pub(super) fn print_branch_list_sorted(
    store: &FileRefStore,
    mode: BranchListMode,
    descending: bool,
) -> Result<()> {
    print_branch_list_filtered_sorted_with_color(store, mode, false, descending, |_, _| true)
}

pub(super) fn print_branch_list_version_sorted(
    store: &FileRefStore,
    mode: BranchListMode,
    descending: bool,
) -> Result<()> {
    print_branch_list_matching_version_sorted(store, mode, &[], false, descending)
}

pub(super) fn print_branch_list_objectname_sorted(
    store: &FileRefStore,
    mode: BranchListMode,
    descending: bool,
) -> Result<()> {
    print_branch_list_matching_objectname_sorted(store, mode, &[], false, descending)
}

pub(super) fn print_branch_list_objecttype_sorted(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    mode: BranchListMode,
    descending: bool,
) -> Result<()> {
    print_branch_list_matching_objecttype_sorted(
        git_dir,
        format,
        store,
        mode,
        &[],
        false,
        descending,
    )
}

pub(super) fn print_branch_list_objectsize_sorted(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    mode: BranchListMode,
    descending: bool,
) -> Result<()> {
    print_branch_list_matching_objectsize_sorted(
        git_dir,
        format,
        store,
        mode,
        &[],
        false,
        descending,
    )
}

pub(super) fn print_branch_list_date_sorted(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    mode: BranchListMode,
    field: ForEachRefDateSortField,
    descending: bool,
) -> Result<()> {
    print_branch_list_matching_date_sorted(
        git_dir,
        format,
        store,
        mode,
        &[],
        false,
        (field, descending),
    )
}

pub(super) fn print_branch_list_upstream_sorted(
    git_dir: &Path,
    store: &FileRefStore,
    mode: BranchListMode,
    descending: bool,
) -> Result<()> {
    print_branch_list_matching_upstream_sorted(git_dir, store, mode, &[], false, descending)
}

pub(super) fn print_branch_list_push_sorted(
    git_dir: &Path,
    store: &FileRefStore,
    mode: BranchListMode,
    descending: bool,
) -> Result<()> {
    print_branch_list_matching_push_sorted(git_dir, store, mode, &[], false, descending)
}

pub(super) fn run_branch_general_list_options(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    replace_objects: bool,
    options: BranchGeneralListOptions,
) -> Result<()> {
    let mut refs = branch_sorted_refs(git_dir, format, store, options.mode, options.sort)?;
    if options.ignore_case
        && let BranchSort::Refname(descending) = options.sort.unwrap_or(BranchSort::Refname(false))
    {
        refs.sort_by(|left, right| {
            let left_key = left.name.to_ascii_lowercase();
            let right_key = right.name.to_ascii_lowercase();
            left_key
                .cmp(&right_key)
                .then_with(|| left.name.cmp(&right.name))
        });
        if descending {
            refs.reverse();
        }
    }
    refs = branch_filter_refs_by_reachability(
        git_dir,
        format,
        store,
        replace_objects,
        refs,
        &options.filters,
    )?;
    if let Some(style) = options.column {
        let show_detached = options.patterns.is_empty();
        let rows = collect_branch_rows(
            store,
            refs,
            store.current_branch_ref()?.as_deref(),
            options.mode,
            false,
            show_detached,
            |_, name| branch_list_patterns_match(&options.patterns, name, options.ignore_case),
        )?;
        return print_branch_columns(&rows, style);
    }
    let show_detached = options.patterns.is_empty();
    let worktree_paths = if options.color {
        Some(for_each_ref_worktree_paths(
            git_dir,
            None,
            store.current_branch_ref()?.as_deref(),
        )?)
    } else {
        None
    };
    print_branch_refs(
        store,
        refs,
        store.current_branch_ref()?.as_deref(),
        options.mode,
        options.color,
        show_detached,
        worktree_paths.as_ref(),
        |_, name| branch_list_patterns_match(&options.patterns, name, options.ignore_case),
    )
}

pub(super) fn branch_sorted_refs(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    mode: BranchListMode,
    sort: Option<BranchSort>,
) -> Result<Vec<sley_refs::Ref>> {
    let mut refs = branch_refs_for_mode(store, mode)?;
    match sort.unwrap_or(BranchSort::Refname(false)) {
        BranchSort::Refname(descending) => {
            if descending {
                refs.reverse();
            }
            Ok(refs)
        }
        BranchSort::Version(descending) => {
            refs.sort_by(|left, right| version_sort_cmp(&left.name, &right.name, &[]));
            if descending {
                refs.reverse();
            }
            Ok(refs)
        }
        BranchSort::ObjectName(descending) => {
            refs.sort_by(|left, right| {
                let left_key = branch_ref_objectname_sort_key(left);
                let right_key = branch_ref_objectname_sort_key(right);
                let object_order = if descending {
                    right_key.cmp(&left_key)
                } else {
                    left_key.cmp(&right_key)
                };
                object_order.then_with(|| left.name.cmp(&right.name))
            });
            Ok(refs)
        }
        BranchSort::ObjectType(descending) => {
            let db = FileObjectDatabase::from_git_dir(git_dir, format);
            let mut keyed = refs
                .into_iter()
                .map(|reference| {
                    let key = branch_ref_objecttype_sort_key(store, &db, &reference)?;
                    Ok::<_, GitError>((reference, key))
                })
                .collect::<Result<Vec<_>>>()?;
            keyed.sort_by(|(left, left_key), (right, right_key)| {
                let object_order = if descending {
                    right_key.cmp(left_key)
                } else {
                    left_key.cmp(right_key)
                };
                object_order.then_with(|| left.name.cmp(&right.name))
            });
            Ok(keyed.into_iter().map(|(reference, _)| reference).collect())
        }
        BranchSort::ObjectSize(descending) => {
            let db = FileObjectDatabase::from_git_dir(git_dir, format);
            let mut keyed = refs
                .into_iter()
                .map(|reference| {
                    let key = branch_ref_objectsize_sort_key(store, &db, &reference)?;
                    Ok::<_, GitError>((reference, key))
                })
                .collect::<Result<Vec<_>>>()?;
            keyed.sort_by(|(left, left_key), (right, right_key)| {
                let object_order = if descending {
                    right_key.cmp(left_key)
                } else {
                    left_key.cmp(right_key)
                };
                object_order.then_with(|| left.name.cmp(&right.name))
            });
            Ok(keyed.into_iter().map(|(reference, _)| reference).collect())
        }
        BranchSort::Date(field, descending) => {
            let db = FileObjectDatabase::from_git_dir(git_dir, format);
            let mut keyed = refs
                .into_iter()
                .map(|reference| {
                    let key = branch_ref_date_sort_key(store, &db, format, &reference, field)?;
                    Ok::<_, GitError>((reference, key))
                })
                .collect::<Result<Vec<_>>>()?;
            keyed.sort_by(|(left, left_key), (right, right_key)| {
                let date_order = if descending {
                    right_key.cmp(left_key)
                } else {
                    left_key.cmp(right_key)
                };
                date_order.then_with(|| left.name.cmp(&right.name))
            });
            Ok(keyed.into_iter().map(|(reference, _)| reference).collect())
        }
        BranchSort::Upstream(descending) => {
            let config = read_repo_config(git_dir)?;
            refs.sort_by(|left, right| {
                let left_key = branch_ref_upstream_sort_key(&config, left);
                let right_key = branch_ref_upstream_sort_key(&config, right);
                let upstream_order = if descending {
                    right_key.cmp(&left_key)
                } else {
                    left_key.cmp(&right_key)
                };
                upstream_order.then_with(|| left.name.cmp(&right.name))
            });
            Ok(refs)
        }
        BranchSort::Push(descending) => {
            let config = read_repo_config(git_dir)?;
            refs.sort_by(|left, right| {
                let left_key = branch_ref_push_sort_key(&config, left);
                let right_key = branch_ref_push_sort_key(&config, right);
                let push_order = if descending {
                    right_key.cmp(&left_key)
                } else {
                    left_key.cmp(&right_key)
                };
                push_order.then_with(|| left.name.cmp(&right.name))
            });
            Ok(refs)
        }
        BranchSort::AheadBehind(target, descending) => {
            let db = FileObjectDatabase::from_git_dir(git_dir, format);
            let mut keyed = refs
                .into_iter()
                .map(|reference| {
                    let key = branch_ref_ahead_behind_sort_key(
                        store, git_dir, &db, format, &reference, &target,
                    )?;
                    Ok::<_, GitError>((reference, key))
                })
                .collect::<Result<Vec<_>>>()?;
            keyed.sort_by(|(left, left_key), (right, right_key)| {
                let ahead_order = if descending {
                    right_key.cmp(left_key)
                } else {
                    left_key.cmp(right_key)
                };
                ahead_order.then_with(|| left.name.cmp(&right.name))
            });
            Ok(keyed.into_iter().map(|(reference, _)| reference).collect())
        }
    }
}

pub(super) fn branch_ref_ahead_behind_sort_key(
    store: &FileRefStore,
    git_dir: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    reference: &sley_refs::Ref,
    target: &ObjectId,
) -> Result<(usize, usize)> {
    let Some((oid, _)) = resolve_for_each_ref_target(store, reference)? else {
        return Ok((0, 0));
    };
    let Some(track) = for_each_ref_ahead_behind(git_dir, db, format, &oid, target)? else {
        return Ok((0, 0));
    };
    Ok((track.ahead, track.behind))
}

pub(super) fn print_branch_list_colored(
    git_dir: &Path,
    store: &FileRefStore,
    mode: BranchListMode,
) -> Result<()> {
    let current = store.current_branch_ref()?;
    let worktree_paths = for_each_ref_worktree_paths(git_dir, None, current.as_deref())?;
    print_branch_refs(
        store,
        branch_refs_for_mode(store, mode)?,
        current.as_deref(),
        mode,
        true,
        true,
        Some(&worktree_paths),
        |_, _| true,
    )
}

pub(super) fn print_branch_list_points_at(
    store: &FileRefStore,
    mode: BranchListMode,
    oid: &ObjectId,
) -> Result<()> {
    print_branch_list_points_at_matching(store, mode, oid, &[])
}

pub(super) fn print_branch_list_points_at_matching(
    store: &FileRefStore,
    mode: BranchListMode,
    oid: &ObjectId,
    patterns: &[String],
) -> Result<()> {
    print_branch_list_filtered_detached(store, mode, false, |reference, name| {
        matches!(&reference.target, RefTarget::Direct(target) if target == oid)
            && branch_list_patterns_match(patterns, name, false)
    })
}

pub(super) fn branch_filter_refs_by_reachability(
    git_dir: &Path,
    format: ObjectFormat,
    _store: &FileRefStore,
    replace_objects: bool,
    refs: Vec<sley_refs::Ref>,
    filters: &BranchListFilters,
) -> Result<Vec<sley_refs::Ref>> {
    if filters.is_empty() {
        return Ok(refs);
    }

    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let contains_oids =
        branch_resolve_filter_revs(git_dir, format, replace_objects, &filters.contains)?;
    let no_contains_oids =
        branch_resolve_filter_revs(git_dir, format, replace_objects, &filters.no_contains)?;
    let merged_oids =
        branch_resolve_filter_revs(git_dir, format, replace_objects, &filters.merged)?;
    let no_merged_oids =
        branch_resolve_filter_revs(git_dir, format, replace_objects, &filters.no_merged)?;
    let contains_targets = branch_peel_filter_oids(&db, format, &filters.contains, &contains_oids)?;
    let no_contains_targets =
        branch_peel_filter_oids(&db, format, &filters.no_contains, &no_contains_oids)?;
    let merged_targets = branch_peel_filter_oids(&db, format, &filters.merged, &merged_oids)?;
    let no_merged_targets =
        branch_peel_filter_oids(&db, format, &filters.no_merged, &no_merged_oids)?;

    let contains_target_set = contains_targets.iter().copied().collect::<HashSet<_>>();
    let no_contains_target_set = no_contains_targets.iter().copied().collect::<HashSet<_>>();
    let mut reachability = sley_rev::CommitReachability::new(git_dir, format, &db);
    let merged_reachable = reachability.reachable_oids(merged_targets, false)?;
    let no_merged_reachable = reachability.reachable_oids(no_merged_targets, false)?;
    let mut out = Vec::with_capacity(refs.len());
    for reference in refs {
        let RefTarget::Direct(tip) = &reference.target else {
            continue;
        };
        let Ok(tip) = sley_rev::peel_to_commit(&db, format, tip) else {
            continue;
        };
        let contains_match = reachability.target_match(
            &tip,
            &contains_target_set,
            &no_contains_target_set,
            false,
        )?;
        let merged_match = filters.merged.is_empty() || merged_reachable.contains(&tip);
        let no_merged_match = no_merged_reachable.contains(&tip);
        if contains_match.reached_required
            && !contains_match.reached_excluded
            && merged_match
            && !no_merged_match
        {
            out.push(reference);
        }
    }
    Ok(out)
}

pub(super) fn branch_resolve_filter_revs(
    git_dir: &Path,
    format: ObjectFormat,
    replace_objects: bool,
    revs: &[String],
) -> Result<Vec<ObjectId>> {
    revs.iter()
        .map(|rev| resolve_revision(git_dir, format, rev, replace_objects))
        .collect()
}

pub(super) fn branch_peel_filter_oids(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    revs: &[String],
    oids: &[ObjectId],
) -> Result<Vec<ObjectId>> {
    revs.iter()
        .zip(oids)
        .map(|(rev, oid)| branch_peel_filter_oid(db, format, rev, oid))
        .collect()
}

pub(super) fn branch_peel_filter_oid(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    rev: &str,
    oid: &ObjectId,
) -> Result<ObjectId> {
    match sley_rev::peel_to_commit(db, format, oid) {
        Ok(commit) => Ok(commit),
        Err(_) => {
            eprintln!("error: object {rev} must point to a commit");
            Err(GitError::Exit(128))
        }
    }
}

pub(super) fn print_branch_list_contains(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    mode: BranchListMode,
    oid: &ObjectId,
    contains: bool,
) -> Result<()> {
    if contains {
        print_branch_list_contains_filters(
            git_dir,
            format,
            store,
            mode,
            std::slice::from_ref(oid),
            &[],
        )
    } else {
        print_branch_list_contains_filters(
            git_dir,
            format,
            store,
            mode,
            &[],
            std::slice::from_ref(oid),
        )
    }
}

pub(super) fn print_branch_list_contains_filters(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    mode: BranchListMode,
    contains_oids: &[ObjectId],
    no_contains_oids: &[ObjectId],
) -> Result<()> {
    print_branch_list_contains_filters_matching(
        git_dir,
        format,
        store,
        mode,
        contains_oids,
        no_contains_oids,
        &[],
    )
}

pub(super) fn print_branch_list_contains_filters_matching(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    mode: BranchListMode,
    contains_oids: &[ObjectId],
    no_contains_oids: &[ObjectId],
    patterns: &[String],
) -> Result<()> {
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let contains_targets = contains_oids
        .iter()
        .map(|oid| sley_rev::peel_to_commit(&db, format, oid))
        .collect::<Result<Vec<_>>>()?;
    let no_contains_targets = no_contains_oids
        .iter()
        .map(|oid| sley_rev::peel_to_commit(&db, format, oid))
        .collect::<Result<Vec<_>>>()?;
    let contains_target_set = contains_targets.iter().copied().collect::<HashSet<_>>();
    let no_contains_target_set = no_contains_targets.iter().copied().collect::<HashSet<_>>();
    let mut reachability = sley_rev::CommitReachability::new(git_dir, format, &db);
    let mut included = HashSet::new();
    for reference in branch_refs_for_mode(store, mode)? {
        let RefTarget::Direct(tip) = &reference.target else {
            continue;
        };
        let Ok(tip) = sley_rev::peel_to_commit(&db, format, tip) else {
            continue;
        };
        let target_match = reachability.target_match(
            &tip,
            &contains_target_set,
            &no_contains_target_set,
            false,
        )?;
        if target_match.reached_required && !target_match.reached_excluded {
            included.insert(reference.name.clone());
        }
    }
    print_branch_list_filtered(store, mode, |reference, name| {
        included.contains(&reference.name) && branch_list_patterns_match(patterns, name, false)
    })
}

pub(super) fn print_branch_list_merged(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    mode: BranchListMode,
    oid: &ObjectId,
    merged: bool,
) -> Result<()> {
    if merged {
        print_branch_list_merged_filters(
            git_dir,
            format,
            store,
            mode,
            std::slice::from_ref(oid),
            &[],
        )
    } else {
        print_branch_list_merged_filters(
            git_dir,
            format,
            store,
            mode,
            &[],
            std::slice::from_ref(oid),
        )
    }
}

pub(super) fn print_branch_list_merged_filters(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    mode: BranchListMode,
    merged_oids: &[ObjectId],
    no_merged_oids: &[ObjectId],
) -> Result<()> {
    print_branch_list_merged_filters_matching(
        git_dir,
        format,
        store,
        mode,
        merged_oids,
        no_merged_oids,
        &[],
    )
}

pub(super) fn print_branch_list_merged_filters_matching(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    mode: BranchListMode,
    merged_oids: &[ObjectId],
    no_merged_oids: &[ObjectId],
    patterns: &[String],
) -> Result<()> {
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let merged_targets = merged_oids
        .iter()
        .map(|oid| sley_rev::peel_to_commit(&db, format, oid))
        .collect::<Result<Vec<_>>>()?;
    let no_merged_targets = no_merged_oids
        .iter()
        .map(|oid| sley_rev::peel_to_commit(&db, format, oid))
        .collect::<Result<Vec<_>>>()?;
    let mut reachability = sley_rev::CommitReachability::new(git_dir, format, &db);
    let merged_reachable = reachability.reachable_oids(merged_targets, false)?;
    let no_merged_reachable = reachability.reachable_oids(no_merged_targets, false)?;
    let mut included = HashSet::new();
    for reference in branch_refs_for_mode(store, mode)? {
        let RefTarget::Direct(tip) = &reference.target else {
            continue;
        };
        let Ok(tip) = sley_rev::peel_to_commit(&db, format, tip) else {
            continue;
        };
        let merged_match = merged_oids.is_empty() || merged_reachable.contains(&tip);
        let no_merged_match = no_merged_reachable.contains(&tip);
        if merged_match && !no_merged_match {
            included.insert(reference.name.clone());
        }
    }
    print_branch_list_filtered(store, mode, |reference, name| {
        included.contains(&reference.name) && branch_list_patterns_match(patterns, name, false)
    })
}

pub(super) fn print_branch_list_matching(
    store: &FileRefStore,
    mode: BranchListMode,
    patterns: &[String],
    ignore_case: bool,
) -> Result<()> {
    print_branch_list_filtered(store, mode, |_, name| {
        branch_list_patterns_match(patterns, name, ignore_case)
    })
}

pub(super) fn print_branch_list_matching_sorted(
    store: &FileRefStore,
    mode: BranchListMode,
    patterns: &[String],
    ignore_case: bool,
    descending: bool,
) -> Result<()> {
    print_branch_list_filtered_sorted_with_color(store, mode, false, descending, |_, name| {
        branch_list_patterns_match(patterns, name, ignore_case)
    })
}

pub(super) fn print_branch_list_matching_version_sorted(
    store: &FileRefStore,
    mode: BranchListMode,
    patterns: &[String],
    ignore_case: bool,
    descending: bool,
) -> Result<()> {
    print_branch_list_filtered_version_sorted_with_color(
        store,
        mode,
        false,
        descending,
        |_, name| branch_list_patterns_match(patterns, name, ignore_case),
    )
}

pub(super) fn print_branch_list_matching_objectname_sorted(
    store: &FileRefStore,
    mode: BranchListMode,
    patterns: &[String],
    ignore_case: bool,
    descending: bool,
) -> Result<()> {
    print_branch_list_filtered_objectname_sorted_with_color(
        store,
        mode,
        false,
        descending,
        |_, name| branch_list_patterns_match(patterns, name, ignore_case),
    )
}

pub(super) fn print_branch_list_matching_objecttype_sorted(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    mode: BranchListMode,
    patterns: &[String],
    ignore_case: bool,
    descending: bool,
) -> Result<()> {
    print_branch_list_filtered_objecttype_sorted_with_color(
        git_dir,
        format,
        store,
        mode,
        false,
        descending,
        |_, name| branch_list_patterns_match(patterns, name, ignore_case),
    )
}

pub(super) fn print_branch_list_matching_objectsize_sorted(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    mode: BranchListMode,
    patterns: &[String],
    ignore_case: bool,
    descending: bool,
) -> Result<()> {
    print_branch_list_filtered_objectsize_sorted_with_color(
        git_dir,
        format,
        store,
        mode,
        false,
        descending,
        |_, name| branch_list_patterns_match(patterns, name, ignore_case),
    )
}

pub(super) fn print_branch_list_matching_date_sorted(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    mode: BranchListMode,
    patterns: &[String],
    ignore_case: bool,
    sort: (ForEachRefDateSortField, bool),
) -> Result<()> {
    print_branch_list_filtered_date_sorted_with_color(
        git_dir,
        format,
        store,
        mode,
        false,
        sort,
        |_, name| branch_list_patterns_match(patterns, name, ignore_case),
    )
}

pub(super) fn print_branch_list_matching_upstream_sorted(
    git_dir: &Path,
    store: &FileRefStore,
    mode: BranchListMode,
    patterns: &[String],
    ignore_case: bool,
    descending: bool,
) -> Result<()> {
    print_branch_list_filtered_upstream_sorted_with_color(
        git_dir,
        store,
        mode,
        false,
        descending,
        |_, name| branch_list_patterns_match(patterns, name, ignore_case),
    )
}

pub(super) fn print_branch_list_matching_push_sorted(
    git_dir: &Path,
    store: &FileRefStore,
    mode: BranchListMode,
    patterns: &[String],
    ignore_case: bool,
    descending: bool,
) -> Result<()> {
    print_branch_list_filtered_push_sorted_with_color(
        git_dir,
        store,
        mode,
        false,
        descending,
        |_, name| branch_list_patterns_match(patterns, name, ignore_case),
    )
}

pub(super) fn branch_list_patterns_match(
    patterns: &[String],
    name: &str,
    ignore_case: bool,
) -> bool {
    patterns.is_empty()
        || patterns.iter().any(|pattern| {
            if ignore_case {
                refname_pattern_matches_case(pattern, name, true)
            } else {
                refname_pattern_matches(pattern, name)
            }
        })
}

pub(super) fn print_branch_list_matching_colored(
    store: &FileRefStore,
    mode: BranchListMode,
    patterns: &[String],
) -> Result<()> {
    print_branch_list_filtered_with_color(store, mode, true, |_, name| {
        patterns.is_empty()
            || patterns
                .iter()
                .any(|pattern| refname_pattern_matches(pattern, name))
    })
}

pub(super) fn branch_color_always_flag(value: &str) -> bool {
    value == "--color" || value == "--color=always"
}

pub(super) fn branch_color_noop_flag(value: &str) -> bool {
    matches!(value, "--no-color" | "--color=auto" | "--color=never")
}

pub(super) fn branch_ignore_case_flag(value: &str) -> bool {
    branch_ignore_case_enabled_flag(value) || value == "--no-ignore-case"
}

pub(super) fn branch_ignore_case_enabled_flag(value: &str) -> bool {
    matches!(value, "-i" | "--ignore-case")
}

pub(super) fn branch_omit_empty_value(value: &str) -> Option<bool> {
    match value {
        "--omit-empty" => Some(true),
        "--no-omit-empty" => Some(false),
        _ => None,
    }
}

pub(super) fn branch_list_noop_display_flag(value: &str) -> bool {
    branch_color_noop_flag(value)
        || branch_column_noop_flag(value)
        || matches!(
            value,
            "--abbrev"
                | "--no-abbrev"
                | "--sort=refname"
                | "--no-sort"
                | "--no-delete"
                | "--no-list"
                | "--no-show-current"
                | "--no-points-at"
                | "--omit-empty"
                | "--no-omit-empty"
                | "--no-format"
        )
        || value.starts_with("--abbrev=")
}

pub(super) fn branch_remote_or_all_mode(value: &str) -> Option<BranchListMode> {
    match value {
        "-r" | "--remotes" => Some(BranchListMode::Remote),
        "-a" | "--all" => Some(BranchListMode::All),
        _ => None,
    }
}

/// Resolve `-r` / `--remotes` / `-a` / `--all` after match guards validated `flag`.
pub(super) fn branch_remote_or_all_mode_unchecked(flag: &str) -> BranchListMode {
    branch_remote_or_all_mode(flag).expect("flag is -r/--remotes/-a/--all")
}

pub(super) fn print_branch_list_remote_or_all_flag(store: &FileRefStore, flag: &str) -> Result<()> {
    print_branch_list(store, branch_remote_or_all_mode_unchecked(flag))
}

pub(super) fn print_branch_list_colored_remote_or_all_flag(
    git_dir: &Path,
    store: &FileRefStore,
    flag: &str,
) -> Result<()> {
    print_branch_list_colored(git_dir, store, branch_remote_or_all_mode_unchecked(flag))
}

pub(super) fn branch_column_noop_flag(value: &str) -> bool {
    matches!(
        value,
        "--no-column" | "--column=auto" | "--column=never" | "--column=plain"
    )
}

pub(super) fn branch_abbrev_noop_flag(value: &str) -> bool {
    matches!(value, "--abbrev" | "--no-abbrev") || value.starts_with("--abbrev=")
}

pub(super) fn branch_version_sort_value(value: &str) -> Option<bool> {
    match value {
        "--sort=version:refname" | "--sort=v:refname" | "version:refname" | "v:refname" => {
            Some(false)
        }
        "--sort=-version:refname" | "--sort=-v:refname" | "-version:refname" | "-v:refname" => {
            Some(true)
        }
        _ => None,
    }
}

pub(super) fn branch_objectname_sort_value(value: &str) -> Option<bool> {
    match value {
        "--sort=objectname" | "objectname" => Some(false),
        "--sort=-objectname" | "-objectname" => Some(true),
        _ => None,
    }
}

pub(super) fn branch_objecttype_sort_value(value: &str) -> Option<bool> {
    match value {
        "--sort=objecttype" | "--sort=type" | "objecttype" | "type" => Some(false),
        "--sort=-objecttype" | "--sort=-type" | "-objecttype" | "-type" => Some(true),
        _ => None,
    }
}

pub(super) fn branch_objectsize_sort_value(value: &str) -> Option<bool> {
    match value {
        "--sort=objectsize" | "objectsize" => Some(false),
        "--sort=-objectsize" | "-objectsize" => Some(true),
        _ => None,
    }
}

pub(super) fn branch_date_sort_value(value: &str) -> Option<(ForEachRefDateSortField, bool)> {
    match value {
        "--sort=authordate" | "authordate" => Some((ForEachRefDateSortField::Author, false)),
        "--sort=-authordate" | "-authordate" => Some((ForEachRefDateSortField::Author, true)),
        "--sort=committerdate" | "committerdate" => {
            Some((ForEachRefDateSortField::Committer, false))
        }
        "--sort=-committerdate" | "-committerdate" => {
            Some((ForEachRefDateSortField::Committer, true))
        }
        "--sort=creatordate" | "creatordate" => Some((ForEachRefDateSortField::Creator, false)),
        "--sort=-creatordate" | "-creatordate" => Some((ForEachRefDateSortField::Creator, true)),
        _ => None,
    }
}

pub(super) fn branch_upstream_sort_value(value: &str) -> Option<bool> {
    match value {
        "--sort=upstream" | "upstream" => Some(false),
        "--sort=-upstream" | "-upstream" => Some(true),
        _ => None,
    }
}

pub(super) fn branch_push_sort_value(value: &str) -> Option<bool> {
    match value {
        "--sort=push" | "push" => Some(false),
        "--sort=-push" | "-push" => Some(true),
        _ => None,
    }
}

pub(super) fn branch_ahead_behind_sort_value(value: &str) -> Option<(&str, bool)> {
    value
        .strip_prefix("ahead-behind:")
        .map(|rev| (rev, false))
        .or_else(|| value.strip_prefix("-ahead-behind:").map(|rev| (rev, true)))
}

pub(super) fn branch_non_refname_sort_value(value: &str) -> bool {
    branch_version_sort_value(value).is_some()
        || branch_objectname_sort_value(value).is_some()
        || branch_objecttype_sort_value(value).is_some()
        || branch_objectsize_sort_value(value).is_some()
        || branch_date_sort_value(value).is_some()
        || branch_upstream_sort_value(value).is_some()
        || branch_push_sort_value(value).is_some()
}

pub(super) fn branch_contains_eq_value(value: &str) -> Option<&str> {
    value.strip_prefix("--contains=")
}

pub(super) fn branch_no_contains_eq_value(value: &str) -> Option<&str> {
    value.strip_prefix("--no-contains=")
}

pub(super) fn branch_merged_eq_value(value: &str) -> Option<&str> {
    value.strip_prefix("--merged=")
}

pub(super) fn branch_no_merged_eq_value(value: &str) -> Option<&str> {
    value.strip_prefix("--no-merged=")
}

pub(super) fn print_branch_list_format(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    replace_objects: bool,
    mode: BranchListMode,
    patterns: &[String],
    ignore_case: bool,
    format_spec: &str,
) -> Result<()> {
    print_branch_list_format_omit_empty(
        git_dir,
        format,
        store,
        replace_objects,
        BranchFormatPrintOptions {
            mode,
            patterns,
            ignore_case,
            format_spec,
            omit_empty: false,
        },
    )
}

pub(super) struct BranchFormatPrintOptions<'a> {
    pub(crate) mode: BranchListMode,
    pub(crate) patterns: &'a [String],
    pub(crate) ignore_case: bool,
    pub(crate) format_spec: &'a str,
    pub(crate) omit_empty: bool,
}

pub(super) fn print_branch_list_format_omit_empty(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    replace_objects: bool,
    options: BranchFormatPrintOptions<'_>,
) -> Result<()> {
    print_branch_list_format_omit_empty_with_sort_color(
        git_dir,
        format,
        store,
        replace_objects,
        options,
        None,
        false,
    )
}

pub(super) fn run_branch_format_list_options(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    replace_objects: bool,
    options: BranchFormatListOptions,
) -> Result<()> {
    print_branch_list_format_omit_empty_with_sort_color(
        git_dir,
        format,
        store,
        replace_objects,
        BranchFormatPrintOptions {
            mode: options.mode,
            patterns: &options.patterns,
            ignore_case: options.ignore_case,
            format_spec: &options.format_spec,
            omit_empty: options.omit_empty,
        },
        options.sort,
        options.color,
    )
}

pub(super) fn print_branch_list_format_omit_empty_with_sort_color(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    replace_objects: bool,
    options: BranchFormatPrintOptions<'_>,
    sort: Option<BranchSort>,
    color: bool,
) -> Result<()> {
    let format_spec = ForEachRefFormat::parse(options.format_spec)?;
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let config = read_repo_config(git_dir)?;
    let head_ref = store.current_branch_ref()?;
    let objectname_abbrev = repository_abbrev(git_dir, format)?;
    let objectname_candidates = cat_file_all_object_ids(git_dir, format)?;
    let deltabase = zero_oid(format)?;
    let mailmap = commands::utility::Mailmap::load_default(git_dir, format, replace_objects)?;
    let all_refs = branch_sorted_refs(git_dir, format, store, options.mode, sort)?;
    let ref_names: std::collections::HashSet<String> = all_refs
        .iter()
        .map(|reference| reference.name.clone())
        .collect();
    let warn_ambiguous_refs = config
        .get_bool("core", None, "warnambiguousrefs")
        .unwrap_or(true);
    let mut stdout = io::stdout().lock();
    if matches!(options.mode, BranchListMode::Local | BranchListMode::All)
        && head_ref.is_none()
        && options.patterns.is_empty()
        && let Some(refname) = detached_head_branch_line(store)
        && let Some((oid, _)) = resolve_for_each_ref_target(
            store,
            &sley_refs::Ref {
                name: "HEAD".into(),
                target: store
                    .read_ref("HEAD")?
                    .unwrap_or(RefTarget::Direct(zero_oid(format)?)),
            },
        )?
    {
        print_branch_format_reference(
            &mut stdout,
            &format_spec,
            git_dir,
            format,
            store,
            &db,
            &config,
            &refname,
            oid,
            None,
            true,
            None,
            &deltabase,
            objectname_abbrev,
            &objectname_candidates,
            &mailmap,
            &ref_names,
            warn_ambiguous_refs,
            color,
            options.omit_empty,
        )?;
    }
    for reference in all_refs.iter() {
        let Some(name) = branch_pattern_name(&reference.name, options.mode) else {
            continue;
        };
        if !options.patterns.is_empty()
            && !options.patterns.iter().any(|pattern| {
                if options.ignore_case {
                    refname_pattern_matches_case(pattern, &name, true)
                } else {
                    refname_pattern_matches(pattern, &name)
                }
            })
        {
            continue;
        }
        let Some((oid, symref)) = resolve_for_each_ref_target(store, reference)? else {
            continue;
        };
        let worktree_path =
            for_each_ref_worktree_path(git_dir, None, head_ref.as_deref(), &reference.name)?;
        print_branch_format_reference(
            &mut stdout,
            &format_spec,
            git_dir,
            format,
            store,
            &db,
            &config,
            &reference.name,
            oid,
            symref,
            head_ref.as_deref() == Some(reference.name.as_str()),
            worktree_path.as_deref(),
            &deltabase,
            objectname_abbrev,
            &objectname_candidates,
            &mailmap,
            &ref_names,
            warn_ambiguous_refs,
            color,
            options.omit_empty,
        )?;
    }
    stdout.flush()?;
    Ok(())
}

pub(super) fn print_branch_format_reference(
    stdout: &mut impl Write,
    format_spec: &ForEachRefFormat,
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    db: &FileObjectDatabase,
    config: &GitConfig,
    refname: &str,
    oid: ObjectId,
    symref: Option<String>,
    is_head: bool,
    worktree_path: Option<&str>,
    deltabase: &ObjectId,
    objectname_abbrev: Option<usize>,
    objectname_candidates: &[ObjectId],
    mailmap: &commands::utility::Mailmap,
    ref_names: &std::collections::HashSet<String>,
    warn_ambiguous_refs: bool,
    color: bool,
    omit_empty: bool,
) -> Result<()> {
    let upstream = for_each_ref_upstream(config, refname);
    let push = for_each_ref_push(config, refname);
    let upstream_track = upstream
        .as_ref()
        .map(|upstream| {
            for_each_ref_upstream_track(store, git_dir, db, format, &oid, &upstream.refname)
        })
        .transpose()?
        .flatten();
    let push_track = push
        .as_ref()
        .and_then(|push| push.refname.as_deref())
        .map(|push_ref| for_each_ref_upstream_track(store, git_dir, db, format, &oid, push_ref))
        .transpose()?
        .flatten();
    let object = db.read_object(&oid)?;
    let object_disk_size = for_each_ref_loose_object_disk_size(git_dir, &oid)?;
    let contents = for_each_ref_contents(format, &object)?;
    let context = ForEachRefFormatContext {
        git_dir,
        db,
        format,
        refname,
        oid: &oid,
        deltabase,
        object_type: object.object_type,
        object_body: &object.body,
        object_size: object.body.len(),
        object_disk_size,
        color,
        quote: ForEachRefQuoteMode::None,
        objectname_abbrev,
        objectname_candidates,
        worktree_path,
        is_head,
        symref: symref.as_deref(),
        upstream,
        push,
        upstream_track,
        push_track,
        contents,
        peeled_object: None,
        signature: None,
        peeled_signature: None,
        mailmap,
        ref_names,
        warn_ambiguous_refs,
    };
    let mut line = Vec::new();
    print_for_each_ref_format(&mut line, format_spec, &context)?;
    if omit_empty && line.is_empty() {
        return Ok(());
    }
    stdout.write_all(&line)?;
    stdout.write_all(b"\n")?;
    Ok(())
}

pub(super) fn branch_pattern_name(name: &str, mode: BranchListMode) -> Option<String> {
    if matches!(mode, BranchListMode::Local | BranchListMode::All)
        && let Some(name) = name.strip_prefix("refs/heads/")
    {
        return Some(name.to_string());
    }
    if matches!(mode, BranchListMode::Remote | BranchListMode::All)
        && let Some(name) = name.strip_prefix("refs/remotes/")
    {
        return Some(name.to_string());
    }
    None
}

pub(super) fn print_branch_list_filtered(
    store: &FileRefStore,
    mode: BranchListMode,
    mut include: impl FnMut(&sley_refs::Ref, &str) -> bool,
) -> Result<()> {
    print_branch_list_filtered_with_color(store, mode, false, |reference, name| {
        include(reference, name)
    })
}

pub(super) fn print_branch_list_filtered_detached(
    store: &FileRefStore,
    mode: BranchListMode,
    show_detached: bool,
    mut include: impl FnMut(&sley_refs::Ref, &str) -> bool,
) -> Result<()> {
    print_branch_list_filtered_sorted_with_color_detached(
        store,
        mode,
        false,
        false,
        show_detached,
        |reference, name| include(reference, name),
    )
}

pub(super) fn print_branch_list_filtered_with_color(
    store: &FileRefStore,
    mode: BranchListMode,
    color: bool,
    include: impl FnMut(&sley_refs::Ref, &str) -> bool,
) -> Result<()> {
    print_branch_list_filtered_sorted_with_color(store, mode, color, false, include)
}

pub(super) fn print_branch_list_filtered_sorted_with_color(
    store: &FileRefStore,
    mode: BranchListMode,
    color: bool,
    descending: bool,
    include: impl FnMut(&sley_refs::Ref, &str) -> bool,
) -> Result<()> {
    print_branch_list_filtered_sorted_with_color_detached(
        store, mode, color, descending, true, include,
    )
}

pub(super) fn print_branch_list_filtered_sorted_with_color_detached(
    store: &FileRefStore,
    mode: BranchListMode,
    color: bool,
    descending: bool,
    show_detached: bool,
    include: impl FnMut(&sley_refs::Ref, &str) -> bool,
) -> Result<()> {
    let current = store.current_branch_ref()?;
    let mut refs = branch_refs_for_mode(store, mode)?;
    if descending {
        refs.reverse();
    }
    print_branch_refs(
        store,
        refs,
        current.as_deref(),
        mode,
        color,
        show_detached,
        None,
        include,
    )
}

pub(super) fn print_branch_list_filtered_version_sorted_with_color(
    store: &FileRefStore,
    mode: BranchListMode,
    color: bool,
    descending: bool,
    include: impl FnMut(&sley_refs::Ref, &str) -> bool,
) -> Result<()> {
    let current = store.current_branch_ref()?;
    let mut refs = branch_refs_for_mode(store, mode)?;
    refs.sort_by(|left, right| version_sort_cmp(&left.name, &right.name, &[]));
    if descending {
        refs.reverse();
    }
    print_branch_refs(
        store,
        refs,
        current.as_deref(),
        mode,
        color,
        true,
        None,
        include,
    )
}

pub(super) fn print_branch_list_filtered_objectname_sorted_with_color(
    store: &FileRefStore,
    mode: BranchListMode,
    color: bool,
    descending: bool,
    include: impl FnMut(&sley_refs::Ref, &str) -> bool,
) -> Result<()> {
    let current = store.current_branch_ref()?;
    let mut refs = branch_refs_for_mode(store, mode)?;
    refs.sort_by(|left, right| {
        let left_key = branch_ref_objectname_sort_key(left);
        let right_key = branch_ref_objectname_sort_key(right);
        let object_order = if descending {
            right_key.cmp(&left_key)
        } else {
            left_key.cmp(&right_key)
        };
        object_order.then_with(|| left.name.cmp(&right.name))
    });
    print_branch_refs(
        store,
        refs,
        current.as_deref(),
        mode,
        color,
        true,
        None,
        include,
    )
}

pub(super) fn branch_ref_objectname_sort_key(reference: &sley_refs::Ref) -> String {
    match &reference.target {
        RefTarget::Direct(oid) => oid.to_hex(),
        RefTarget::Symbolic(target) => target.clone(),
    }
}

pub(super) fn print_branch_list_filtered_objecttype_sorted_with_color(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    mode: BranchListMode,
    color: bool,
    descending: bool,
    include: impl FnMut(&sley_refs::Ref, &str) -> bool,
) -> Result<()> {
    let current = store.current_branch_ref()?;
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let mut keyed = Vec::new();
    for reference in branch_refs_for_mode(store, mode)? {
        let key = branch_ref_objecttype_sort_key(store, &db, &reference)?;
        keyed.push((reference, key));
    }
    keyed.sort_by(|(left, left_key), (right, right_key)| {
        let object_order = if descending {
            right_key.cmp(left_key)
        } else {
            left_key.cmp(right_key)
        };
        object_order.then_with(|| left.name.cmp(&right.name))
    });
    let refs = keyed
        .into_iter()
        .map(|(reference, _)| reference)
        .collect::<Vec<_>>();
    print_branch_refs(
        store,
        refs,
        current.as_deref(),
        mode,
        color,
        true,
        None,
        include,
    )
}

pub(super) fn branch_ref_objecttype_sort_key(
    store: &FileRefStore,
    db: &FileObjectDatabase,
    reference: &sley_refs::Ref,
) -> Result<String> {
    let Some((oid, _)) = resolve_for_each_ref_target(store, reference)? else {
        return Ok(String::new());
    };
    Ok(db.read_object(&oid)?.object_type.as_str().to_string())
}

pub(super) fn print_branch_list_filtered_objectsize_sorted_with_color(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    mode: BranchListMode,
    color: bool,
    descending: bool,
    include: impl FnMut(&sley_refs::Ref, &str) -> bool,
) -> Result<()> {
    let current = store.current_branch_ref()?;
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let mut keyed = Vec::new();
    for reference in branch_refs_for_mode(store, mode)? {
        let key = branch_ref_objectsize_sort_key(store, &db, &reference)?;
        keyed.push((reference, key));
    }
    keyed.sort_by(|(left, left_key), (right, right_key)| {
        let object_order = if descending {
            right_key.cmp(left_key)
        } else {
            left_key.cmp(right_key)
        };
        object_order.then_with(|| left.name.cmp(&right.name))
    });
    let refs = keyed
        .into_iter()
        .map(|(reference, _)| reference)
        .collect::<Vec<_>>();
    print_branch_refs(
        store,
        refs,
        current.as_deref(),
        mode,
        color,
        true,
        None,
        include,
    )
}

pub(super) fn branch_ref_objectsize_sort_key(
    store: &FileRefStore,
    db: &FileObjectDatabase,
    reference: &sley_refs::Ref,
) -> Result<usize> {
    let Some((oid, _)) = resolve_for_each_ref_target(store, reference)? else {
        return Ok(0);
    };
    Ok(db.read_object(&oid)?.body.len())
}

pub(super) fn print_branch_list_filtered_date_sorted_with_color(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    mode: BranchListMode,
    color: bool,
    sort: (ForEachRefDateSortField, bool),
    include: impl FnMut(&sley_refs::Ref, &str) -> bool,
) -> Result<()> {
    let current = store.current_branch_ref()?;
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let (field, descending) = sort;
    let mut keyed = Vec::new();
    for reference in branch_refs_for_mode(store, mode)? {
        let key = branch_ref_date_sort_key(store, &db, format, &reference, field)?;
        keyed.push((reference, key));
    }
    keyed.sort_by(|(left, left_key), (right, right_key)| {
        let date_order = if descending {
            right_key.cmp(left_key)
        } else {
            left_key.cmp(right_key)
        };
        date_order.then_with(|| left.name.cmp(&right.name))
    });
    let refs = keyed
        .into_iter()
        .map(|(reference, _)| reference)
        .collect::<Vec<_>>();
    print_branch_refs(
        store,
        refs,
        current.as_deref(),
        mode,
        color,
        true,
        None,
        include,
    )
}

pub(super) fn branch_ref_date_sort_key(
    store: &FileRefStore,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    reference: &sley_refs::Ref,
    field: ForEachRefDateSortField,
) -> Result<i128> {
    let Some((oid, _)) = resolve_for_each_ref_target(store, reference)? else {
        return Ok(0);
    };
    let object = db.read_object(&oid)?;
    let contents = for_each_ref_contents(format, &object)?;
    Ok(for_each_ref_sort_date_key(contents, field))
}

pub(super) fn print_branch_list_filtered_upstream_sorted_with_color(
    git_dir: &Path,
    store: &FileRefStore,
    mode: BranchListMode,
    color: bool,
    descending: bool,
    include: impl FnMut(&sley_refs::Ref, &str) -> bool,
) -> Result<()> {
    let current = store.current_branch_ref()?;
    let config = read_repo_config(git_dir)?;
    let mut refs = branch_refs_for_mode(store, mode)?;
    refs.sort_by(|left, right| {
        let left_key = branch_ref_upstream_sort_key(&config, left);
        let right_key = branch_ref_upstream_sort_key(&config, right);
        let upstream_order = if descending {
            right_key.cmp(&left_key)
        } else {
            left_key.cmp(&right_key)
        };
        upstream_order.then_with(|| left.name.cmp(&right.name))
    });
    print_branch_refs(
        store,
        refs,
        current.as_deref(),
        mode,
        color,
        true,
        None,
        include,
    )
}

pub(super) fn branch_ref_upstream_sort_key(
    config: &GitConfig,
    reference: &sley_refs::Ref,
) -> String {
    for_each_ref_upstream(config, &reference.name)
        .map(|upstream| upstream.refname)
        .unwrap_or_default()
}

pub(super) fn print_branch_list_filtered_push_sorted_with_color(
    git_dir: &Path,
    store: &FileRefStore,
    mode: BranchListMode,
    color: bool,
    descending: bool,
    include: impl FnMut(&sley_refs::Ref, &str) -> bool,
) -> Result<()> {
    let current = store.current_branch_ref()?;
    let config = read_repo_config(git_dir)?;
    let mut refs = branch_refs_for_mode(store, mode)?;
    refs.sort_by(|left, right| {
        let left_key = branch_ref_push_sort_key(&config, left);
        let right_key = branch_ref_push_sort_key(&config, right);
        let push_order = if descending {
            right_key.cmp(&left_key)
        } else {
            left_key.cmp(&right_key)
        };
        push_order.then_with(|| left.name.cmp(&right.name))
    });
    print_branch_refs(
        store,
        refs,
        current.as_deref(),
        mode,
        color,
        true,
        None,
        include,
    )
}

pub(super) fn branch_ref_push_sort_key(config: &GitConfig, reference: &sley_refs::Ref) -> String {
    for_each_ref_push(config, &reference.name)
        .and_then(|push| push.refname)
        .unwrap_or_default()
}

/// The `* (no branch, ...)` / `* (HEAD detached at ...)` first line `git
/// branch` prints when HEAD is detached, with the in-progress-operation
/// variants (bisect / rebase) taking precedence -- mirroring upstream
/// `wt_status_get_state` + `get_head_description`.
pub(super) fn detached_head_branch_line(store: &FileRefStore) -> Option<String> {
    let git_dir = store.git_dir();
    let RefTarget::Direct(oid) = store.read_ref("HEAD").ok()?? else {
        return None;
    };
    if let Ok(start) = fs::read_to_string(git_dir.join("BISECT_START")) {
        let start = start.trim();
        if !start.is_empty() {
            return Some(format!("(no branch, bisect started on {start})"));
        }
    }
    for dir in ["rebase-merge", "rebase-apply"] {
        if let Ok(head_name) = fs::read_to_string(git_dir.join(dir).join("head-name")) {
            let head_name = head_name.trim();
            let branch = if matches!(head_name, "detached HEAD" | "HEAD") {
                format!("detached HEAD {}", format_log_abbrev_oid(&oid))
            } else {
                head_name
                    .strip_prefix("refs/heads/")
                    .unwrap_or(head_name)
                    .to_string()
            };
            return Some(format!("(no branch, rebasing {branch})"));
        }
    }
    Some(
        detached_head_description(store)
            .unwrap_or_else(|| format!("(HEAD detached at {})", format_log_abbrev_oid(&oid))),
    )
}

pub(super) fn detached_head_description(store: &FileRefStore) -> Option<String> {
    let entries = store.read_reflog("HEAD").ok()?;
    let (idx, checkout) = entries.iter().enumerate().rev().find_map(|(idx, entry)| {
        let message = std::str::from_utf8(&entry.message).ok()?;
        let destination = message
            .strip_prefix("checkout: moving from ")?
            .rsplit_once(" to ")?
            .1;
        Some((idx, (entry, destination)))
    })?;
    let label = detached_checkout_label(checkout.1, &checkout.0.new_oid);
    let moved_after_checkout = entries[idx + 1..]
        .iter()
        .any(|entry| entry.old_oid != entry.new_oid);
    if moved_after_checkout {
        Some(format!("(HEAD detached from {label})"))
    } else {
        Some(format!("(HEAD detached at {label})"))
    }
}

pub(super) fn detached_checkout_label(destination: &str, oid: &ObjectId) -> String {
    if destination == "HEAD"
        || destination.starts_with("HEAD^")
        || destination.starts_with("HEAD~")
        || destination == oid.to_hex()
        || oid.to_hex().starts_with(destination)
    {
        format_log_abbrev_oid(oid)
    } else {
        destination.to_string()
    }
}

pub(super) fn print_branch_refs(
    store: &FileRefStore,
    refs: Vec<sley_refs::Ref>,
    current: Option<&str>,
    mode: BranchListMode,
    color: bool,
    show_detached: bool,
    worktree_paths: Option<&HashMap<String, String>>,
    mut include: impl FnMut(&sley_refs::Ref, &str) -> bool,
) -> Result<()> {
    let colors = if color {
        Some(branch_list_colors_from_current_repo(store)?)
    } else {
        None
    };
    if matches!(mode, BranchListMode::Local | BranchListMode::All)
        && current.is_none()
        && show_detached
        && let Some(line) = detached_head_branch_line(store)
    {
        if let Some(colors) = &colors {
            println!("* {}", colors.paint(BranchColorSlot::Current, &line));
        } else {
            println!("* {line}");
        }
    }
    let ref_names = matches!(mode, BranchListMode::Remote | BranchListMode::All).then(|| {
        refs.iter()
            .map(|reference| reference.name.clone())
            .collect::<HashSet<_>>()
    });
    for reference in refs {
        if matches!(mode, BranchListMode::Local | BranchListMode::All)
            && let Some(name) = reference.name.strip_prefix("refs/heads/")
        {
            if !include(&reference, name) {
                continue;
            }
            let linked_worktree = worktree_paths
                .and_then(|paths| paths.get(&reference.name))
                .is_some();
            let marker = if Some(reference.name.as_str()) == current {
                '*'
            } else if linked_worktree {
                '+'
            } else {
                ' '
            };
            let target = local_symbolic_branch_target(&reference);
            if let Some(colors) = &colors {
                let slot = match marker {
                    '*' => BranchColorSlot::Current,
                    '+' => BranchColorSlot::Worktree,
                    _ => BranchColorSlot::Local,
                };
                print!("{marker} {}", colors.paint(slot, name));
                if let Some(target) = target {
                    print!(" -> {target}");
                }
                println!();
            } else if let Some(target) = target {
                println!("{marker} {name} -> {target}");
            } else {
                println!("{marker} {name}");
            }
            continue;
        }
        if matches!(mode, BranchListMode::Remote | BranchListMode::All)
            && let Some(name) = reference.name.strip_prefix("refs/remotes/")
        {
            if ref_names
                .as_ref()
                .is_some_and(|ref_names| remote_symbolic_ref_is_dangling(&reference, ref_names))
            {
                continue;
            }
            let display = remote_branch_display(&reference, name, mode);
            if !include(&reference, name) {
                continue;
            }
            if let Some(colors) = &colors {
                println!("  {}", colors.paint(BranchColorSlot::Remote, &display));
            } else {
                println!("  {display}");
            }
        }
    }
    Ok(())
}

pub(super) fn collect_branch_rows(
    store: &FileRefStore,
    refs: Vec<sley_refs::Ref>,
    current: Option<&str>,
    mode: BranchListMode,
    color: bool,
    show_detached: bool,
    mut include: impl FnMut(&sley_refs::Ref, &str) -> bool,
) -> Result<Vec<String>> {
    let mut rows = Vec::new();
    let colors = if color {
        Some(branch_list_colors_from_current_repo(store)?)
    } else {
        None
    };
    if matches!(mode, BranchListMode::Local | BranchListMode::All)
        && current.is_none()
        && show_detached
        && let Some(line) = detached_head_branch_line(store)
    {
        if let Some(colors) = &colors {
            rows.push(format!(
                "* {}",
                colors.paint(BranchColorSlot::Current, &line)
            ));
        } else {
            rows.push(format!("* {line}"));
        }
    }
    let ref_names = matches!(mode, BranchListMode::Remote | BranchListMode::All).then(|| {
        refs.iter()
            .map(|reference| reference.name.clone())
            .collect::<HashSet<_>>()
    });
    for reference in refs {
        if matches!(mode, BranchListMode::Local | BranchListMode::All)
            && let Some(name) = reference.name.strip_prefix("refs/heads/")
        {
            if !include(&reference, name) {
                continue;
            }
            let marker = if Some(reference.name.as_str()) == current {
                '*'
            } else {
                ' '
            };
            let target = local_symbolic_branch_target(&reference);
            if let Some(colors) = &colors {
                let slot = if marker == '*' {
                    BranchColorSlot::Current
                } else {
                    BranchColorSlot::Local
                };
                let mut row = format!("{marker} {}", colors.paint(slot, name));
                if let Some(target) = target {
                    row.push_str(&format!(" -> {target}"));
                }
                rows.push(row);
            } else if let Some(target) = target {
                rows.push(format!("{marker} {name} -> {target}"));
            } else {
                rows.push(format!("{marker} {name}"));
            }
            continue;
        }
        if matches!(mode, BranchListMode::Remote | BranchListMode::All)
            && let Some(name) = reference.name.strip_prefix("refs/remotes/")
        {
            if ref_names
                .as_ref()
                .is_some_and(|ref_names| remote_symbolic_ref_is_dangling(&reference, ref_names))
            {
                continue;
            }
            let display = remote_branch_display(&reference, name, mode);
            if !include(&reference, name) {
                continue;
            }
            if let Some(colors) = &colors {
                rows.push(format!(
                    "  {}",
                    colors.paint(BranchColorSlot::Remote, &display)
                ));
            } else {
                rows.push(format!("  {display}"));
            }
        }
    }
    Ok(rows)
}

pub(super) fn local_symbolic_branch_target(reference: &sley_refs::Ref) -> Option<String> {
    let RefTarget::Symbolic(target) = &reference.target else {
        return None;
    };
    target
        .strip_prefix("refs/heads/")
        .or_else(|| target.strip_prefix("refs/remotes/"))
        .map(str::to_string)
}

pub(super) fn remote_symbolic_ref_is_dangling(
    reference: &sley_refs::Ref,
    ref_names: &HashSet<String>,
) -> bool {
    match &reference.target {
        RefTarget::Symbolic(target) => !ref_names.contains(target.as_str()),
        RefTarget::Direct(_) => false,
    }
}

pub(super) fn remote_branch_display(
    reference: &sley_refs::Ref,
    name: &str,
    mode: BranchListMode,
) -> String {
    let display = if matches!(mode, BranchListMode::All) {
        format!("remotes/{name}")
    } else {
        name.to_string()
    };
    let RefTarget::Symbolic(target) = &reference.target else {
        return display;
    };
    let Some(target_name) = target.strip_prefix("refs/remotes/") else {
        return display;
    };
    format!("{display} -> {target_name}")
}

#[derive(Clone, Copy)]
pub(super) enum BranchColorSlot {
    Current,
    Local,
    Remote,
    Worktree,
}

pub(super) struct BranchListColors {
    current: String,
    local: String,
    remote: String,
    worktree: String,
    reset: String,
}

impl BranchListColors {
    fn from_config(config: &GitConfig) -> Self {
        Self {
            current: branch_color(config, "current", "green"),
            local: branch_color(config, "local", "normal"),
            remote: branch_color(config, "remote", "red"),
            worktree: branch_color(config, "worktree", "cyan"),
            reset: git_color_spec_to_ansi("reset", true),
        }
    }

    fn paint(&self, slot: BranchColorSlot, text: &str) -> String {
        let color = match slot {
            BranchColorSlot::Current => &self.current,
            BranchColorSlot::Local => &self.local,
            BranchColorSlot::Remote => &self.remote,
            BranchColorSlot::Worktree => &self.worktree,
        };
        format!("{color}{text}{}", self.reset)
    }
}

pub(super) fn branch_list_colors_from_current_repo(
    store: &FileRefStore,
) -> Result<BranchListColors> {
    let config = read_repo_config(store.git_dir())?;
    Ok(BranchListColors::from_config(&config))
}

pub(super) fn branch_color(config: &GitConfig, key: &str, default: &str) -> String {
    git_color_spec_to_ansi(
        config.get("color", Some("branch"), key).unwrap_or(default),
        true,
    )
}

pub(super) fn print_branch_columns(rows: &[String], style: BranchColumnStyle) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let width = env::var("COLUMNS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|width| *width > 0)
        .unwrap_or(80);
    let max_len = rows.iter().map(|row| row.len()).max().unwrap_or(0);
    if max_len.saturating_mul(2) >= width {
        for row in rows {
            println!("{row}");
        }
        return Ok(());
    }
    let cell_width = max_len + 1;
    let requested_cols = (width / cell_width).max(1).min(rows.len());
    let row_count = rows.len().div_ceil(requested_cols);
    let col_count = rows.len().div_ceil(row_count);
    let mut col_widths = vec![cell_width; col_count];
    if style == BranchColumnStyle::Dense {
        for (col, width) in col_widths.iter_mut().enumerate() {
            let mut col_len = 0usize;
            for row in 0..row_count {
                let idx = col * row_count + row;
                if let Some(value) = rows.get(idx) {
                    col_len = col_len.max(value.len());
                }
            }
            *width = col_len + 1;
        }
    }
    for row in 0..row_count {
        let mut line = String::new();
        for (col, width) in col_widths.iter().enumerate() {
            let idx = col * row_count + row;
            let Some(value) = rows.get(idx) else {
                continue;
            };
            if col + 1 == col_count {
                line.push_str(value);
            } else {
                line.push_str(&format!("{value:<width$}"));
            }
        }
        println!("{}", line.trim_end());
    }
    Ok(())
}

pub(super) fn print_branch_list_verbose(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    replace_objects: bool,
    options: BranchVerboseListOptions,
) -> Result<()> {
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let config = read_repo_config(git_dir)?;
    let current = store.current_branch_ref()?;
    let objectname_abbrev = options
        .abbrev
        .map(|abbrev| abbrev.map(|width| width.min(format.hex_len())))
        .unwrap_or(repository_abbrev(git_dir, format)?);
    let objectname_candidates = cat_file_all_object_ids(git_dir, format)?;
    let worktree_paths = for_each_ref_worktree_paths(git_dir, None, current.as_deref())?;
    let mut rows = Vec::new();
    if matches!(options.mode, BranchListMode::Local | BranchListMode::All)
        && current.is_none()
        && options.patterns.is_empty()
        && let Some(display) = detached_head_branch_line(store)
        && let Some((oid, _)) = resolve_for_each_ref_target(
            store,
            &sley_refs::Ref {
                name: "HEAD".into(),
                target: store
                    .read_ref("HEAD")?
                    .unwrap_or(RefTarget::Direct(zero_oid(format)?)),
            },
        )?
    {
        rows.push(BranchVerboseRow {
            display,
            oid: for_each_ref_abbrev_oid(&oid, objectname_abbrev, &objectname_candidates),
            subject: branch_verbose_subject(&db, format, &oid)?,
            is_head: true,
            worktree_path: None,
            upstream: None,
            upstream_track: None,
        });
    }
    let refs = branch_filter_refs_by_reachability(
        git_dir,
        format,
        store,
        replace_objects,
        branch_refs_for_mode(store, options.mode)?,
        &options.filters,
    )?;
    for reference in refs {
        let Some((display, pattern_name)) =
            branch_verbose_display_name(&reference.name, options.mode)
        else {
            continue;
        };
        if !branch_list_patterns_match(&options.patterns, &pattern_name, options.ignore_case) {
            continue;
        }
        let Some((oid, _)) = resolve_for_each_ref_target(store, &reference)? else {
            continue;
        };
        let subject = branch_verbose_subject(&db, format, &oid)?;
        let upstream = for_each_ref_upstream(&config, &reference.name);
        let upstream_track = upstream
            .as_ref()
            .map(|upstream| {
                for_each_ref_upstream_track(store, git_dir, &db, format, &oid, &upstream.refname)
            })
            .transpose()?
            .flatten();
        rows.push(BranchVerboseRow {
            display,
            oid: for_each_ref_abbrev_oid(&oid, objectname_abbrev, &objectname_candidates),
            subject,
            is_head: current.as_deref() == Some(reference.name.as_str()),
            worktree_path: worktree_paths.get(&reference.name).cloned(),
            upstream,
            upstream_track,
        });
    }
    let width = rows.iter().map(|row| row.display.len()).max().unwrap_or(0);
    let colors = if options.color {
        Some(branch_list_colors_from_current_repo(store)?)
    } else {
        None
    };
    for row in rows {
        let marker = if row.is_head {
            '*'
        } else if row.worktree_path.is_some() {
            '+'
        } else {
            ' '
        };
        let mut tracking =
            branch_verbose_tracking(row.upstream.as_ref(), row.upstream_track, options.verbosity);
        if options.verbosity >= 2
            && !row.is_head
            && let Some(worktree_path) = &row.worktree_path
        {
            tracking.push_str(&format!(" ({worktree_path})"));
        }
        let display = format!("{:<width$}", row.display, width = width);
        let display = if let Some(colors) = &colors {
            let slot = if row.is_head {
                BranchColorSlot::Current
            } else if row.worktree_path.is_some() {
                BranchColorSlot::Worktree
            } else if row.display.starts_with("remotes/") {
                BranchColorSlot::Remote
            } else {
                BranchColorSlot::Local
            };
            colors.paint(slot, &display)
        } else {
            display
        };
        println!("{marker} {display} {}{} {}", row.oid, tracking, row.subject);
    }
    Ok(())
}

pub(super) struct BranchVerboseRow {
    display: String,
    oid: String,
    subject: String,
    is_head: bool,
    worktree_path: Option<String>,
    upstream: Option<ForEachRefUpstream>,
    upstream_track: Option<ForEachRefTrack>,
}

pub(super) fn branch_verbose_display_name(
    name: &str,
    mode: BranchListMode,
) -> Option<(String, String)> {
    if matches!(mode, BranchListMode::Local | BranchListMode::All)
        && let Some(name) = name.strip_prefix("refs/heads/")
    {
        return Some((name.to_string(), name.to_string()));
    }
    if matches!(mode, BranchListMode::Remote | BranchListMode::All)
        && let Some(name) = name.strip_prefix("refs/remotes/")
    {
        let display = if matches!(mode, BranchListMode::All) {
            format!("remotes/{name}")
        } else {
            name.to_string()
        };
        return Some((display, name.to_string()));
    }
    None
}

pub(super) fn branch_verbose_subject(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    oid: &ObjectId,
) -> Result<String> {
    let object = db.read_object(oid)?;
    if object.object_type != ObjectType::Commit {
        return Ok(String::new());
    }
    let commit = Commit::parse(format, &object.body)?;
    Ok(commit_subject(&commit.message))
}

pub(super) fn branch_verbose_tracking(
    upstream: Option<&ForEachRefUpstream>,
    track: Option<ForEachRefTrack>,
    verbosity: usize,
) -> String {
    match (verbosity, upstream, track) {
        (0, _, _) => String::new(),
        (1, _, Some(track)) if track.gone || track.ahead > 0 || track.behind > 0 => {
            let mut out = Vec::new();
            write_for_each_ref_track(&mut out, track, true).expect("write to vec");
            format!(" {}", String::from_utf8_lossy(&out))
        }
        (1, _, _) => String::new(),
        (_, Some(upstream), Some(track)) if track.gone || track.ahead > 0 || track.behind > 0 => {
            let mut out = Vec::new();
            write_for_each_ref_track(&mut out, track, false).expect("write to vec");
            format!(
                " [{}: {}]",
                for_each_ref_short_name(&upstream.refname),
                String::from_utf8_lossy(&out)
            )
        }
        (_, Some(upstream), _) => format!(" [{}]", for_each_ref_short_name(&upstream.refname)),
        (_, None, _) => String::new(),
    }
}
