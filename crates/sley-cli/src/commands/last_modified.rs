//! `git last-modified`: report the commit that last touched each selected path.

use crate::*;
use sley::plumbing::{sley_diff_merge, sley_rev};

#[derive(Clone)]
struct LastModifiedOptions {
    max_depth: i64,
    show_trees: bool,
    nul: bool,
    max_count: Option<usize>,
    revs: Vec<String>,
    pathspecs: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct TreeSide {
    mode: u32,
    oid: ObjectId,
}

#[derive(Clone)]
struct PathEntry {
    path: Vec<u8>,
    mode: u32,
    oid: ObjectId,
}

struct LastModifiedState {
    paths: Vec<PathEntry>,
    path_index: HashMap<Vec<u8>, usize>,
    remaining: HashSet<usize>,
    active: HashMap<ObjectId, HashSet<usize>>,
    emitted: Vec<LastModifiedOutput>,
}

struct LastModifiedOutput {
    commit: ObjectId,
    boundary: bool,
    path: Vec<u8>,
}

struct CommitInfo {
    tree: ObjectId,
    parents: Vec<ObjectId>,
}

#[derive(Clone, Eq, PartialEq)]
struct LastModifiedQueueEntry {
    timestamp: i64,
    hex: String,
    oid: ObjectId,
}

impl Ord for LastModifiedQueueEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.timestamp
            .cmp(&other.timestamp)
            .then_with(|| other.hex.cmp(&self.hex))
    }
}

impl PartialOrd for LastModifiedQueueEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

pub(crate) fn cmd_last_modified(
    cli_session: &crate::session::CliSession,
    args: &[String],
) -> Result<()> {
    let options = parse_last_modified_args(args)?;
    let repo = RepositoryContext::from_session(cli_session)?;
    let db = repo.objects();
    let format = repo.format();
    let options = disambiguate_last_modified_positionals(options, &repo);

    let (start_rev, excluded_revs) = resolve_last_modified_revs(&options)?;
    let start_oid = repo.resolve_revision(&start_rev)?;
    let start_commit = peel_last_modified_commit(&repo, &start_rev, &start_oid)?;
    let exclude_tips = excluded_revs
        .iter()
        .map(|rev| {
            let oid = repo.resolve_revision(rev)?;
            peel_last_modified_commit(&repo, rev, &oid)
        })
        .collect::<Result<Vec<_>>>()?;
    let excluded = excluded_ancestors(db, format, &exclude_tips)?;

    let pathspec = sley_pathspec::normalized_revwalk_pathspec(
        repo.cwd(),
        Some(repo.worktree_root()?),
        &options.pathspecs,
        effective_pathspec_flags(),
    )?;
    let start_info = read_commit_info(db, format, &start_commit)?;
    let paths = collect_last_modified_paths(
        db,
        format,
        &start_info.tree,
        &pathspec,
        options.max_depth,
        options.show_trees,
    )?;

    let mut state = LastModifiedState {
        path_index: paths
            .iter()
            .enumerate()
            .map(|(idx, entry)| (entry.path.clone(), idx))
            .collect(),
        remaining: (0..paths.len()).collect(),
        paths,
        active: HashMap::new(),
        emitted: Vec::new(),
    };
    state
        .active
        .insert(start_commit, (0..state.paths.len()).collect());

    run_last_modified_walk(db, format, &options, &excluded, &mut state, start_commit)?;
    write_last_modified_outputs(&state.emitted, options.nul)
}

fn parse_last_modified_args(args: &[String]) -> Result<LastModifiedOptions> {
    let mut options = LastModifiedOptions {
        max_depth: 0,
        show_trees: false,
        nul: false,
        max_count: None,
        revs: Vec::new(),
        pathspecs: Vec::new(),
    };
    let mut after_dd = false;
    let mut i = 0usize;
    while i < args.len() {
        let arg = &args[i];
        if after_dd {
            options.pathspecs.push(arg.clone());
            i += 1;
            continue;
        }
        match arg.as_str() {
            "--" => {
                after_dd = true;
                i += 1;
            }
            "-r" | "--recursive" => {
                options.max_depth = -1;
                i += 1;
            }
            "-t" | "--show-trees" => {
                options.show_trees = true;
                i += 1;
            }
            "-z" => {
                options.nul = true;
                i += 1;
            }
            "--max-depth" => {
                let Some(value) = args.get(i + 1) else {
                    eprintln!("error: option `max-depth' requires a value");
                    return Err(GitError::Exit(129));
                };
                options.max_depth = parse_last_modified_depth(value)?;
                i += 2;
            }
            value if value.starts_with("--max-depth=") => {
                options.max_depth = parse_last_modified_depth(&value["--max-depth=".len()..])?;
                i += 1;
            }
            value
                if value.starts_with('-')
                    && value.len() > 1
                    && value[1..].bytes().all(|byte| byte.is_ascii_digit()) =>
            {
                options.max_count = Some(value[1..].parse::<usize>().map_err(|_| {
                    GitError::Command(format!("invalid max-count {}", &value[1..]))
                })?);
                i += 1;
            }
            value if value.starts_with('-') => {
                eprintln!("error: unknown last-modified argument: {value}");
                print_last_modified_usage();
                return Err(GitError::Exit(129));
            }
            value => {
                if looks_like_revision_arg(value) && options.pathspecs.is_empty() {
                    options.revs.push(value.to_string());
                } else {
                    options.pathspecs.push(value.to_string());
                }
                i += 1;
            }
        }
    }
    Ok(options)
}

fn disambiguate_last_modified_positionals(
    mut options: LastModifiedOptions,
    repo: &RepositoryContext,
) -> LastModifiedOptions {
    if options.revs.is_empty()
        && let Some(first) = options.pathspecs.first()
        && repo.resolve_revision(first).is_ok()
    {
        let rev = options.pathspecs.remove(0);
        options.revs.push(rev);
    }
    options
}

fn parse_last_modified_depth(value: &str) -> Result<i64> {
    value.parse::<i64>().map_err(|_| {
        eprintln!("error: option `max-depth' expects a numerical value");
        GitError::Exit(129)
    })
}

fn looks_like_revision_arg(value: &str) -> bool {
    value.contains("..")
        || value.contains("^{")
        || value == "HEAD"
        || value.starts_with("HEAD^")
        || value.starts_with("HEAD~")
        || value.starts_with('@')
        || value.starts_with("refs/")
}

fn resolve_last_modified_revs(options: &LastModifiedOptions) -> Result<(String, Vec<String>)> {
    let mut start = None;
    let mut excluded = Vec::new();
    for rev in &options.revs {
        if let Some((left, right)) = rev.split_once("..") {
            if !left.is_empty() {
                excluded.push(left.to_string());
            }
            if !right.is_empty() {
                if start.replace(right.to_string()).is_some() {
                    return Err(last_modified_two_commits());
                }
            }
        } else if let Some(stripped) = rev.strip_prefix('^') {
            excluded.push(stripped.to_string());
        } else if start.replace(rev.clone()).is_some() {
            return Err(last_modified_two_commits());
        }
    }
    Ok((start.unwrap_or_else(|| "HEAD".to_string()), excluded))
}

fn peel_last_modified_commit(
    repo: &RepositoryContext,
    rev: &str,
    oid: &ObjectId,
) -> Result<ObjectId> {
    match sley_rev::peel_to_commit(repo.objects(), repo.format(), oid) {
        Ok(commit) => Ok(commit),
        Err(_) => {
            let object = repo.objects().read_object(oid)?;
            eprintln!(
                "error: revision argument '{rev}' is a {}, not a commit-ish",
                object.object_type.as_str()
            );
            Err(GitError::Exit(1))
        }
    }
}

fn last_modified_two_commits() -> GitError {
    eprintln!("error: last-modified can only operate on one commit at a time");
    GitError::Exit(1)
}

fn print_last_modified_usage() {
    eprintln!("usage: git last-modified [--recursive] [--show-trees] [--max-depth=<depth>] [-z]");
    eprintln!("                         [<revision-range>] [[--] <pathspec>...]");
}

fn run_last_modified_walk(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    options: &LastModifiedOptions,
    excluded: &HashSet<ObjectId>,
    state: &mut LastModifiedState,
    start_commit: ObjectId,
) -> Result<()> {
    let mut queue = std::collections::BinaryHeap::new();
    let mut queued = HashSet::from([start_commit]);
    let mut date_cache = HashMap::new();
    push_last_modified_commit(&mut queue, db, format, &mut date_cache, start_commit)?;
    let mut popped = 0usize;

    while let Some(entry) = queue.pop() {
        let commit_oid = entry.oid;
        queued.remove(&commit_oid);
        let active = state.active.remove(&commit_oid).unwrap_or_default();
        let active: HashSet<usize> = active
            .into_iter()
            .filter(|idx| state.remaining.contains(idx))
            .collect();
        if active.is_empty() {
            continue;
        }

        popped += 1;
        if options.max_count.is_some_and(|max| popped > max) || excluded.contains(&commit_oid) {
            mark_boundary_paths(db, format, state, &commit_oid, &active)?;
            continue;
        }

        let info = read_commit_info(db, format, &commit_oid)?;
        if info.parents.is_empty() {
            mark_active_paths(state, &commit_oid, false, &active);
            continue;
        }

        let mut current_active = active;
        for parent in &info.parents {
            if current_active.is_empty() {
                break;
            }
            let parent_info = read_commit_info(db, format, parent)?;
            let changed = changed_paths_between_trees(db, format, &parent_info.tree, &info.tree)?;
            let changed_indices = changed
                .into_iter()
                .filter_map(|path| state.path_index.get(&path).copied())
                .filter(|idx| current_active.contains(idx))
                .collect::<HashSet<_>>();
            let mut passed = HashSet::new();
            for idx in &current_active {
                if !changed_indices.contains(idx) {
                    passed.insert(*idx);
                }
            }
            if !passed.is_empty() {
                let parent_active = state.active.entry(*parent).or_default();
                parent_active.extend(passed.iter().copied());
                if queued.insert(*parent) {
                    push_last_modified_commit(&mut queue, db, format, &mut date_cache, *parent)?;
                }
                for idx in passed {
                    current_active.remove(&idx);
                }
            }
        }

        if !current_active.is_empty() {
            mark_active_paths(state, &commit_oid, false, &current_active);
        }
    }
    Ok(())
}

fn mark_boundary_paths(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    state: &mut LastModifiedState,
    commit_oid: &ObjectId,
    active: &HashSet<usize>,
) -> Result<()> {
    let info = read_commit_info(db, format, commit_oid)?;
    let tree_paths = collect_all_tree_entries(db, format, &info.tree, true)?;
    let oid_by_path = tree_paths
        .into_iter()
        .map(|entry| (entry.path, (entry.mode, entry.oid)))
        .collect::<HashMap<_, _>>();
    let mut to_mark = HashSet::new();
    for idx in active {
        let path = &state.paths[*idx];
        if oid_by_path
            .get(&path.path)
            .is_some_and(|(mode, oid)| *mode == path.mode && *oid == path.oid)
        {
            to_mark.insert(*idx);
        }
    }
    mark_active_paths(state, commit_oid, true, &to_mark);
    Ok(())
}

fn mark_active_paths(
    state: &mut LastModifiedState,
    commit_oid: &ObjectId,
    boundary: bool,
    active: &HashSet<usize>,
) {
    let mut active = active.iter().copied().collect::<Vec<_>>();
    active.sort_unstable();
    for idx in active {
        if state.remaining.remove(&idx) {
            state.emitted.push(LastModifiedOutput {
                commit: *commit_oid,
                boundary,
                path: state.paths[idx].path.clone(),
            });
        }
    }
}

fn push_last_modified_commit(
    queue: &mut std::collections::BinaryHeap<LastModifiedQueueEntry>,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    date_cache: &mut HashMap<ObjectId, i64>,
    oid: ObjectId,
) -> Result<()> {
    let timestamp = match date_cache.get(&oid).copied() {
        Some(timestamp) => timestamp,
        None => {
            let object = db.read_object(&oid)?;
            let commit = Commit::parse(format, &object.body)?;
            let timestamp = for_each_ref_identity_timestamp(&commit.committer).unwrap_or(0);
            date_cache.insert(oid, timestamp);
            timestamp
        }
    };
    queue.push(LastModifiedQueueEntry {
        timestamp,
        hex: oid.to_hex(),
        oid,
    });
    Ok(())
}

fn excluded_ancestors(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tips: &[ObjectId],
) -> Result<HashSet<ObjectId>> {
    let mut seen = HashSet::new();
    let mut stack = tips.to_vec();
    while let Some(oid) = stack.pop() {
        if !seen.insert(oid) {
            continue;
        }
        let info = read_commit_info(db, format, &oid)?;
        stack.extend(info.parents);
    }
    Ok(seen)
}

fn read_commit_info(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    oid: &ObjectId,
) -> Result<CommitInfo> {
    let object = db.read_object(oid)?;
    if object.object_type != ObjectType::Commit {
        return Err(GitError::InvalidObject(format!(
            "expected commit {oid}, found {}",
            object.object_type.as_str()
        )));
    }
    let commit = Commit::parse(format, &object.body)?;
    Ok(CommitInfo {
        tree: commit.tree,
        parents: commit.parents,
    })
}

fn collect_last_modified_paths(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    pathspec: &sley_pathspec::Pathspec,
    max_depth: i64,
    show_trees: bool,
) -> Result<Vec<PathEntry>> {
    let mut paths =
        collect_visible_tree_entries(db, format, tree_oid, pathspec, max_depth, show_trees)?;
    paths.retain(|entry| pathspec.matches(&entry.path));
    Ok(paths)
}

fn collect_visible_tree_entries(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    pathspec: &sley_pathspec::Pathspec,
    max_depth: i64,
    show_trees: bool,
) -> Result<Vec<PathEntry>> {
    let mut out = Vec::new();
    collect_visible_tree_entries_inner(
        db,
        format,
        tree_oid,
        pathspec,
        max_depth,
        show_trees,
        Vec::new(),
        0,
        &mut out,
    )?;
    Ok(out)
}

fn collect_visible_tree_entries_inner(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    pathspec: &sley_pathspec::Pathspec,
    max_depth: i64,
    show_trees: bool,
    prefix: Vec<u8>,
    depth: i64,
    out: &mut Vec<PathEntry>,
) -> Result<()> {
    for (name, entry) in read_tree_map(db, format, tree_oid)? {
        let path = join_tree_path(&prefix, &name);
        if entry.mode == 0o040000 {
            let show_depth_tree = max_depth >= 0
                && depth >= max_depth
                && (pathspec.is_empty() || pathspec.matches(&path));
            let show_pathspec_tree =
                max_depth >= 0 && !pathspec.is_empty() && pathspec.matches(&path);
            let show_this_tree = show_trees || show_depth_tree || show_pathspec_tree;
            if show_this_tree && show_trees && !prefix.is_empty() {
                out.push(PathEntry {
                    path: path.clone(),
                    mode: entry.mode,
                    oid: entry.oid,
                });
            }
            let may_descend =
                max_depth < 0 || depth < max_depth || pathspec_needs_descent(pathspec, &path);
            if may_descend {
                collect_visible_tree_entries_inner(
                    db,
                    format,
                    &entry.oid,
                    pathspec,
                    max_depth,
                    show_trees,
                    path.clone(),
                    depth + 1,
                    out,
                )?;
            }
            if show_this_tree && (!show_trees || prefix.is_empty()) {
                out.push(PathEntry {
                    path,
                    mode: entry.mode,
                    oid: entry.oid,
                });
            }
        } else if max_depth < 0
            || depth <= max_depth
            || (!pathspec.is_empty() && pathspec.matches(&path))
        {
            out.push(PathEntry {
                path,
                mode: entry.mode,
                oid: entry.oid,
            });
        }
    }
    Ok(())
}

fn pathspec_needs_descent(pathspec: &sley_pathspec::Pathspec, path: &[u8]) -> bool {
    if pathspec.is_empty() {
        return false;
    }
    pathspec.elements().iter().any(|element| {
        let pattern = element.pattern();
        pattern.len() > path.len()
            && pattern.starts_with(path)
            && pattern.get(path.len()) == Some(&b'/')
    })
}

fn changed_paths_between_trees(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    left_tree: &ObjectId,
    right_tree: &ObjectId,
) -> Result<HashSet<Vec<u8>>> {
    let mut out = HashSet::new();
    let changes = sley_diff_merge::diff_name_status_trees_with_options(
        db,
        format,
        left_tree,
        right_tree,
        sley_diff_merge::DiffNameStatusOptions {
            detect_renames: false,
            detect_copies: false,
            find_copies_harder: false,
            rename_empty: true,
            ..Default::default()
        },
    )?;
    for entry in changes {
        insert_changed_path_and_parents(&mut out, entry.path.as_bytes());
    }
    Ok(out)
}

fn insert_changed_path_and_parents(out: &mut HashSet<Vec<u8>>, path: &[u8]) {
    out.insert(path.to_vec());
    for idx in 0..path.len() {
        if path[idx] == b'/' {
            out.insert(path[..idx].to_vec());
        }
    }
}

fn collect_all_tree_entries(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    include_trees: bool,
) -> Result<Vec<PathEntry>> {
    let mut out = Vec::new();
    collect_all_tree_entries_inner(db, format, tree_oid, include_trees, Vec::new(), &mut out)?;
    Ok(out)
}

fn collect_all_tree_entries_inner(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    include_trees: bool,
    prefix: Vec<u8>,
    out: &mut Vec<PathEntry>,
) -> Result<()> {
    for (name, entry) in read_tree_map(db, format, tree_oid)? {
        let path = join_tree_path(&prefix, &name);
        if entry.mode == 0o040000 {
            collect_all_tree_entries_inner(
                db,
                format,
                &entry.oid,
                include_trees,
                path.clone(),
                out,
            )?;
            if include_trees {
                out.push(PathEntry {
                    path,
                    mode: entry.mode,
                    oid: entry.oid,
                });
            }
        } else {
            out.push(PathEntry {
                path,
                mode: entry.mode,
                oid: entry.oid,
            });
        }
    }
    Ok(())
}

fn read_tree_map(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &ObjectId,
) -> Result<BTreeMap<Vec<u8>, TreeSide>> {
    let object = db.read_object(tree_oid)?;
    if object.object_type != ObjectType::Tree {
        return Err(GitError::InvalidObject(format!(
            "expected tree {tree_oid}, found {}",
            object.object_type.as_str()
        )));
    }
    let mut map = BTreeMap::new();
    for entry in TreeEntries::new(format, &object.body) {
        let entry = entry?;
        map.insert(
            entry.name.to_vec(),
            TreeSide {
                mode: entry.mode,
                oid: entry.oid,
            },
        );
    }
    Ok(map)
}

fn join_tree_path(prefix: &[u8], name: &[u8]) -> Vec<u8> {
    if prefix.is_empty() {
        name.to_vec()
    } else {
        let mut path = Vec::with_capacity(prefix.len() + 1 + name.len());
        path.extend_from_slice(prefix);
        path.push(b'/');
        path.extend_from_slice(name);
        path
    }
}

fn write_last_modified_outputs(outputs: &[LastModifiedOutput], nul: bool) -> Result<()> {
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    for output in outputs {
        if output.boundary {
            stdout.write_all(b"^")?;
        }
        write!(stdout, "{}\t", output.commit.to_hex())?;
        if nul {
            stdout.write_all(&output.path)?;
            stdout.write_all(&[0])?;
        } else {
            write_status_quoted_path(&mut stdout, &output.path, false)?;
            stdout.write_all(b"\n")?;
        }
    }
    Ok(())
}
