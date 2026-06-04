//! `git blame` — line-by-line authorship for a tracked path.
//!
//! Walks history from a start commit (default `HEAD`) toward the roots,
//! attributing each line of the path's final image to the commit that last
//! introduced it. The traversal diffs every commit's blob against its
//! parent(s) with the same Myers line diff the rest of the suite uses
//! (`git_diff_merge`); a line that is unchanged from a parent is "passed
//! through" to that parent, and a line with no unchanged counterpart in any
//! parent is charged to the current commit. Root commits are rendered as
//! boundaries (a leading `^` on the abbreviated object name) unless `--root`
//! is given, matching upstream `git blame`.
//!
//! Output is the default porcelain-ish line format
//! `<sha> (<author> <date> <lineno>) <line>` with the column widths and
//! boundary handling `git blame` uses. The flags below are supported; the
//! more exotic porcelain/move/copy detection options are intentionally out of
//! scope and reported as unsupported rather than silently ignored.
//!
//! Supported flags: `-L <range>` (with `start,end`, `start`, `,end`,
//! `start,+count`, and repeated ranges), `-l`/`--long`, `-s`, `-e`/
//! `--show-email`, `-t`, `--root`/`--no-root`, `--abbrev[=<n>]`, an optional
//! starting revision, and a `--` path separator.
//!
//! Limitation: the "final image" is the path's content at the start commit
//! (`HEAD` by default). Upstream `git blame` with no revision blames the
//! working-tree copy and renders uncommitted lines with the all-zero
//! `00000000 (Not Committed Yet ...)` pseudo-commit; that working-tree overlay
//! is not implemented here, so for a *clean* working tree this matches
//! `git blame` exactly, and for explicit revisions it always matches.

// Glob the crate root for shared plumbing (discover_git_dir,
// repository_object_format, repository_abbrev, resolve_revision,
// FileObjectDatabase, FileRefStore, Commit, Tree, the identity/date
// formatting helpers, and so on). See commands::stash for the rationale: a
// submodule can reach its ancestor module's private items, so everything
// visible at the crate root is in scope here without re-listing it.
use crate::*;

/// What to print in the metadata column for each line's author.
#[derive(Clone, Copy)]
enum AuthorField {
    /// Author name (default).
    Name,
    /// Author email, in angle brackets (`-e` / `--show-email`).
    Email,
}

/// How to render the per-line date column.
#[derive(Clone, Copy)]
enum DateField {
    /// ISO `YYYY-MM-DD HH:MM:SS +ZZZZ` in the author's timezone (default).
    Iso,
    /// Raw `<seconds> <tz>` (`-t`).
    Raw,
}

/// Parsed `git blame` invocation.
struct BlameOptions {
    /// Optional starting revision; `None` means `HEAD`.
    rev: Option<String>,
    /// The (cwd-relative) path to blame.
    path: String,
    /// Show the full object name instead of an abbreviation (`-l`).
    long_sha: bool,
    /// Suppress the author and date columns (`-s`).
    suppress_author: bool,
    /// Author column contents.
    author_field: AuthorField,
    /// Date column rendering.
    date_field: DateField,
    /// Treat root commits like any other commit (`--root`): no `^` marker.
    show_root: bool,
    /// Explicit abbreviation length (`--abbrev=<n>`), if given.
    abbrev_override: Option<usize>,
    /// Line ranges requested with `-L`; empty means the whole file.
    ranges: Vec<RawRange>,
}

/// A `-L` argument before it is resolved against the file's line count.
struct RawRange {
    start: RangeBound,
    end: RangeBound,
}

/// One side of a `-L <start>,<end>` range.
enum RangeBound {
    /// Omitted (`-L ,5` or `-L 5`): defaults to start-of-file or end-of-file.
    Omitted,
    /// An absolute 1-based line number.
    Absolute(usize),
    /// A relative offset from the other bound (`+N`), only valid as the end.
    Relative(usize),
}

/// The blame result for a single final-image line.
struct LineBlame {
    /// Commit the line is attributed to.
    commit: ObjectId,
    /// Whether that commit is a rendered boundary (root, absent `--root`).
    boundary: bool,
    /// The raw line bytes, including a trailing newline when present.
    content: Vec<u8>,
}

pub(crate) fn cmd_blame(args: &[String]) -> Result<()> {
    let options = match parse_blame_args(args)? {
        BlameArgs::Run(options) => options,
        BlameArgs::Help => {
            print!("{BLAME_USAGE}");
            return Ok(());
        }
    };

    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let format = repository_object_format(&git_dir)?;
    let db = FileObjectDatabase::from_git_dir(&git_dir, format);

    // Resolve the requested revision (default HEAD) to a commit.
    let rev_spec = options.rev.as_deref().unwrap_or("HEAD");
    let start_oid = resolve_revision(&git_dir, format, rev_spec)?;
    let start_commit = git_rev::peel_to_commit(&db, format, &start_oid)?;

    // Turn the cwd-relative path into a repository-root-relative path the way
    // git's pathspec handling does, then locate the blob at the start commit.
    let repo_path = blame_repo_relative_path(&cwd, &git_dir, &options.path)?;
    let final_blob = match read_path_blob(&db, format, &start_commit, &repo_path)? {
        Some(blob) => blob,
        None => {
            // git reports the repository-relative path here, not the literal
            // argument (so blaming a missing file from a subdirectory still
            // names `<dir>/<file>`).
            eprintln!("fatal: no such path '{repo_path}' in {rev_spec}");
            return Err(GitError::Exit(128));
        }
    };

    let lines = compute_blame(&db, format, &start_commit, &repo_path, &final_blob)?;

    // Resolve the -L ranges against the real line count, then render. The
    // repo-relative path is used for any -L error message, matching git.
    let selected = select_lines(&lines, &options, &repo_path)?;
    if selected.is_empty() {
        return Ok(());
    }
    render_blame(&git_dir, format, &lines, &selected, &options)
}

/// Either run with parsed options or print help and exit successfully.
enum BlameArgs {
    Run(BlameOptions),
    Help,
}

/// Parse the command line. Mirrors `git blame`'s tolerance for `<rev>` and
/// `<file>` appearing in either order, an optional `--` separator, and the
/// flag spellings we support; anything else is reported the way git does.
fn parse_blame_args(args: &[String]) -> Result<BlameArgs> {
    let mut long_sha = false;
    let mut suppress_author = false;
    let mut author_field = AuthorField::Name;
    let mut date_field = DateField::Iso;
    let mut show_root = false;
    let mut abbrev_override = None;
    let mut ranges = Vec::new();
    // Positionals collected before `--`; afterwards everything is a path.
    let mut positionals: Vec<String> = Vec::new();
    let mut paths_after_dd: Vec<String> = Vec::new();
    let mut saw_dd = false;

    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if saw_dd {
            paths_after_dd.push(arg.clone());
            continue;
        }
        match arg.as_str() {
            "-h" | "--help" => return Ok(BlameArgs::Help),
            "--" => saw_dd = true,
            "-l" | "--long" => long_sha = true,
            "-s" => suppress_author = true,
            "-e" | "--show-email" => author_field = AuthorField::Email,
            "--no-show-email" => author_field = AuthorField::Name,
            "-t" => date_field = DateField::Raw,
            "--root" => show_root = true,
            "--no-root" => show_root = false,
            "--abbrev" => abbrev_override = Some(0),
            "-L" => {
                let Some(value) = iter.next() else {
                    return Err(blame_option_requires_value("L"));
                };
                ranges.push(parse_line_range(value)?);
            }
            other if other.starts_with("--abbrev=") => {
                let value = &other["--abbrev=".len()..];
                abbrev_override = Some(parse_abbrev_value(value)?);
            }
            other if other.starts_with("-L") => {
                // Attached form: `-L1,3`.
                ranges.push(parse_line_range(&other[2..])?);
            }
            // Options we recognize from git but do not implement. Reject them
            // explicitly rather than misinterpreting them as a path.
            other
                if is_unsupported_blame_option(other)
                    || (other.starts_with('-')
                        && other.len() > 1
                        && !is_negative_number(other)) =>
            {
                // A leading-dash token that is not a value-bearing flag we
                // handled is an unknown option to git.
                if is_unsupported_blame_option(other) {
                    return Err(GitError::Unsupported(format!(
                        "git blame option {other} is not supported by git-rs"
                    )));
                }
                // git's parse-options prints the offending token verbatim,
                // keeping its leading dash(es): `error: unknown option `-Q'`.
                eprintln!("error: unknown option `{other}'");
                eprint!("{BLAME_USAGE}");
                return Err(GitError::Exit(129));
            }
            _ => {
                positionals.push(arg.clone());
            }
        }
    }

    // Resolve positionals into (optional rev, path). git accepts
    // `blame <file>`, `blame <rev> <file>`, `blame <rev> -- <file>`, and
    // `blame <file>` with `<rev>` omitted; paths after `--` take precedence.
    let (rev, path) = resolve_positionals(positionals, paths_after_dd)?;

    Ok(BlameArgs::Run(BlameOptions {
        rev,
        path,
        long_sha,
        suppress_author,
        author_field,
        date_field,
        show_root,
        abbrev_override,
        ranges,
    }))
}

/// Decide which positional is the revision and which is the path.
fn resolve_positionals(
    positionals: Vec<String>,
    paths_after_dd: Vec<String>,
) -> Result<(Option<String>, String)> {
    if !paths_after_dd.is_empty() {
        if paths_after_dd.len() > 1 {
            return Err(blame_too_many_paths());
        }
        let rev = match positionals.len() {
            0 => None,
            1 => Some(positionals[0].clone()),
            _ => return Err(blame_usage_error()),
        };
        return Ok((rev, paths_after_dd[0].clone()));
    }
    match positionals.len() {
        0 => Err(blame_usage_error()),
        1 => Ok((None, positionals[0].clone())),
        2 => Ok((Some(positionals[0].clone()), positionals[1].clone())),
        _ => Err(blame_too_many_paths()),
    }
}

/// True for the `-C`/`-M`/porcelain-style options git understands but which
/// this implementation does not provide.
fn is_unsupported_blame_option(arg: &str) -> bool {
    if matches!(
        arg,
        "-p" | "--porcelain"
            | "--line-porcelain"
            | "--incremental"
            | "-c"
            | "-b"
            | "-f"
            | "--show-name"
            | "-n"
            | "--show-number"
            | "-w"
            | "--reverse"
            | "--color-lines"
            | "--color-by-age"
            | "--show-stats"
            | "--progress"
            | "--score-debug"
    ) {
        return true;
    }
    arg.starts_with("-C")
        || arg.starts_with("-M")
        || arg.starts_with("-S")
        || arg.starts_with("--contents")
        || arg.starts_with("--ignore-rev")
        || arg.starts_with("--ignore-revs-file")
        || arg.starts_with("--diff-algorithm")
        || arg.starts_with("--reverse=")
}

/// `-N` where N is all digits is a (rare) negative-number-looking token; treat
/// such a token as a positional rather than an unknown option, matching git's
/// argument parser which only rejects genuine unknown options.
fn is_negative_number(arg: &str) -> bool {
    arg.len() > 1 && arg.as_bytes()[0] == b'-' && arg[1..].bytes().all(|b| b.is_ascii_digit())
}

/// Parse a single `-L` range argument into unresolved bounds.
///
/// A token that is not a well-formed numeric range is, like git, a *usage*
/// error (exit 129) rather than the fatal "invalid line number" (exit 128)
/// that a syntactically valid but out-of-bounds/zero number produces later.
fn parse_line_range(value: &str) -> Result<RawRange> {
    if value.is_empty() {
        return Err(blame_usage_error());
    }
    // The `:funcname` and `/regex/` range forms are recognized by git but not
    // implemented here; report them as unsupported instead of misparsing.
    if value.starts_with(':') || value.starts_with('/') {
        return Err(GitError::Unsupported(format!(
            "git blame -L {value} (function/regex range) is not supported by git-rs"
        )));
    }
    let (start_raw, end_raw) = match value.split_once(',') {
        Some((start, end)) => (start, end),
        None => (value, ""),
    };
    let start = parse_range_bound(start_raw, false)?;
    let end = parse_range_bound(end_raw, true)?;
    Ok(RawRange { start, end })
}

/// Parse one `-L` bound. `is_end` allows the `+N` relative form. A token that
/// cannot be read as a (non-negative) line number is a usage error.
fn parse_range_bound(raw: &str, is_end: bool) -> Result<RangeBound> {
    if raw.is_empty() {
        return Ok(RangeBound::Omitted);
    }
    if let Some(rest) = raw.strip_prefix('+') {
        if !is_end {
            return Err(blame_usage_error());
        }
        let count = rest.parse::<usize>().map_err(|_| blame_usage_error())?;
        return Ok(RangeBound::Relative(count));
    }
    // Only absolute numbers and the `+N` end form are accepted; anything else
    // (e.g. `abc`, `1.5`, a leading `-`) is a usage error, matching git.
    let number = raw.parse::<usize>().map_err(|_| blame_usage_error())?;
    Ok(RangeBound::Absolute(number))
}

/// Parse the value for `--abbrev=<n>`; 0 (or `--abbrev` with no value) means
/// "use the default", matching git which clamps to the format's hex length.
fn parse_abbrev_value(value: &str) -> Result<usize> {
    value
        .parse::<usize>()
        .map_err(|_| GitError::Command(format!("invalid --abbrev value: {value}")))
}

/// Convert the user-supplied (cwd-relative) path into a repository-root
/// relative path. Outside-of-worktree paths are reported the way git does.
fn blame_repo_relative_path(cwd: &Path, git_dir: &Path, path: &str) -> Result<String> {
    let prefix = worktree_prefix(cwd, git_dir)?;
    // Join the prefix and the argument, then normalize `.`/`..`/duplicate
    // separators so the result is a clean repo-relative path with forward
    // slashes (the form tree entries use).
    let joined = format!("{prefix}{path}");
    normalize_repo_path(&joined)
}

/// Normalize a slash-or-backslash path into a clean forward-slash
/// repo-relative path, resolving `.` and `..` lexically.
fn normalize_repo_path(input: &str) -> Result<String> {
    let unified = input.replace('\\', "/");
    let mut parts: Vec<&str> = Vec::new();
    for component in unified.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    return Err(GitError::InvalidPath(format!(
                        "{input} is outside the repository"
                    )));
                }
            }
            other => parts.push(other),
        }
    }
    Ok(parts.join("/"))
}

/// Read the blob bytes for `repo_path` in `commit`'s tree. Returns `None` when
/// the path is absent (or names a non-blob, which blame treats as absent).
fn read_path_blob(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    commit: &ObjectId,
    repo_path: &str,
) -> Result<Option<Vec<u8>>> {
    let tree_oid = git_rev::peel_to_tree(db, format, commit)?;
    let Some(blob_oid) = lookup_tree_path(db, format, &tree_oid, repo_path)? else {
        return Ok(None);
    };
    let object = db.read_object(&blob_oid)?;
    if object.object_type != ObjectType::Blob {
        return Ok(None);
    }
    Ok(Some(object.body))
}

/// Walk `repo_path` component-by-component through `tree_oid`, returning the
/// blob id it names, or `None` if any component is missing or an intermediate
/// component is not a tree.
fn lookup_tree_path(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    repo_path: &str,
) -> Result<Option<ObjectId>> {
    let components: Vec<&str> = repo_path.split('/').filter(|p| !p.is_empty()).collect();
    if components.is_empty() {
        return Ok(None);
    }
    let mut current = tree_oid.clone();
    let last = components.len() - 1;
    for (idx, component) in components.iter().enumerate() {
        let object = db.read_object(&current)?;
        if object.object_type != ObjectType::Tree {
            return Ok(None);
        }
        let tree = Tree::parse(format, &object.body)?;
        let Some(entry) = tree
            .entries
            .iter()
            .find(|entry| entry.name == component.as_bytes())
        else {
            return Ok(None);
        };
        if idx == last {
            return Ok(Some(entry.oid.clone()));
        }
        if git_object::tree_entry_object_type(entry.mode) != ObjectType::Tree {
            return Ok(None);
        }
        current = entry.oid.clone();
    }
    Ok(None)
}

/// Core blame: assign each final-image line to a commit.
///
/// Maintains, per commit, the set of final lines still "owned" by that
/// commit's version of the path together with each line's index inside that
/// commit's blob. Commits are processed children-before-parents so that by the
/// time a commit is examined, every line it might own has been propagated to
/// it. A line is charged to a commit when it has no unchanged counterpart in
/// any parent (or the commit is a root / the path is absent in every parent).
fn compute_blame(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    start_commit: &ObjectId,
    repo_path: &str,
    final_blob: &[u8],
) -> Result<Vec<LineBlame>> {
    let final_lines = git_diff_merge::split_lines(final_blob);
    let line_count = final_lines.len();

    // Result slot per final line; filled in as commits claim lines.
    let mut result: Vec<Option<LineBlame>> = (0..line_count).map(|_| None).collect();

    // Per-commit pending work: for each owned final line, its index within
    // that commit's blob. `pending[commit][final_line] = blob_index`.
    let mut pending: HashMap<ObjectId, HashMap<usize, usize>> = HashMap::new();
    // Each final line initially maps to the identically-indexed line of the
    // start commit's blob (the final image is that blob).
    let seed: HashMap<usize, usize> = (0..line_count).map(|line| (line, line)).collect();
    pending.insert(start_commit.clone(), seed);

    // Reverse-topological order: every commit precedes its parents, so each
    // commit is finalized (all the lines it could own have propagated to it)
    // by the time we reach it. Ties are broken newest-first by committer date.
    let order = topo_order(db, format, start_commit)?;

    for commit_oid in order {
        let Some(owned) = pending.remove(&commit_oid) else {
            continue;
        };
        if owned.is_empty() {
            continue;
        }

        let commit_obj = db.read_object(&commit_oid)?;
        let commit = Commit::parse(format, &commit_obj.body)?;
        let Some(child_blob) = read_path_blob(db, format, &commit_oid, repo_path)? else {
            // Defensive: a commit that does not contain the path cannot own
            // any line; charge them here to avoid losing lines.
            assign_lines(&mut result, &commit_oid, false, &final_lines, owned);
            continue;
        };
        let child_lines = git_diff_merge::split_lines(&child_blob);

        // Find, for each owned line, whether some parent preserves it. We try
        // parents in order and route a line to the first parent that has an
        // unchanged counterpart. Lines preserved by no parent are charged to
        // this commit. A commit with no parents (root) charges everything.
        let parents = commit.parents.clone();
        if parents.is_empty() {
            assign_lines(&mut result, &commit_oid, true, &final_lines, owned);
            continue;
        }

        // For each parent compute the child->parent line mapping once.
        let mut parent_maps: Vec<(ObjectId, Vec<Option<usize>>)> = Vec::new();
        for parent in &parents {
            match read_path_blob(db, format, parent, repo_path)? {
                Some(parent_blob) => {
                    let parent_lines = git_diff_merge::split_lines(&parent_blob);
                    let map = child_to_parent_map(&parent_lines, &child_lines);
                    parent_maps.push((parent.clone(), map));
                }
                None => {
                    // Path absent in this parent: it preserves nothing.
                    parent_maps.push((parent.clone(), vec![None; child_lines.len()]));
                }
            }
        }

        let mut charged: HashMap<usize, usize> = HashMap::new();
        for (final_line, child_index) in owned {
            let mut routed = false;
            for (parent_oid, map) in &parent_maps {
                if let Some(Some(parent_index)) = map.get(child_index) {
                    pending
                        .entry(parent_oid.clone())
                        .or_default()
                        .insert(final_line, *parent_index);
                    routed = true;
                    break;
                }
            }
            if !routed {
                charged.insert(final_line, child_index);
            }
        }
        if !charged.is_empty() {
            assign_lines(&mut result, &commit_oid, false, &final_lines, charged);
        }
        // No rescheduling is needed: `topo_order` emits every commit after all
        // of its children (Kahn's algorithm over the child→parent DAG), so any
        // parent we just routed lines to still lies ahead in `order`.
    }

    // Any line not resolved (shouldn't happen) falls back to the start commit
    // as a non-boundary so output stays well-formed.
    let mut out = Vec::with_capacity(line_count);
    for (line_index, slot) in result.into_iter().enumerate() {
        match slot {
            Some(blame) => out.push(blame),
            None => out.push(LineBlame {
                commit: start_commit.clone(),
                boundary: false,
                content: final_lines[line_index].content.to_vec(),
            }),
        }
    }
    Ok(out)
}

/// Record `owned` final lines as attributed to `commit_oid`.
fn assign_lines(
    result: &mut [Option<LineBlame>],
    commit_oid: &ObjectId,
    boundary: bool,
    final_lines: &[git_diff_merge::DiffLine<'_>],
    owned: HashMap<usize, usize>,
) {
    for final_line in owned.into_keys() {
        if let Some(slot) = result.get_mut(final_line)
            && slot.is_none()
        {
            *slot = Some(LineBlame {
                commit: commit_oid.clone(),
                boundary,
                content: final_lines[final_line].content.to_vec(),
            });
        }
    }
}

/// Build `child_index -> Some(parent_index)` for lines unchanged from parent to
/// child (the `Equal` runs of the Myers diff); changed/inserted child lines map
/// to `None`.
fn child_to_parent_map(
    parent_lines: &[git_diff_merge::DiffLine<'_>],
    child_lines: &[git_diff_merge::DiffLine<'_>],
) -> Vec<Option<usize>> {
    let mut map = vec![None; child_lines.len()];
    let ops = git_diff_merge::myers_diff_lines(parent_lines, child_lines);
    let mut parent_idx = 0usize;
    let mut child_idx = 0usize;
    for op in ops {
        match op {
            git_diff_merge::DiffOp::Equal(n) => {
                for _ in 0..n {
                    if child_idx < map.len() {
                        map[child_idx] = Some(parent_idx);
                    }
                    parent_idx += 1;
                    child_idx += 1;
                }
            }
            git_diff_merge::DiffOp::Delete(n) => {
                parent_idx += n;
            }
            git_diff_merge::DiffOp::Insert(n) => {
                child_idx += n;
            }
        }
    }
    map
}

/// Reachable commits from `start`, ordered so that every commit precedes its
/// parents (a reverse-topological / children-first order). Uses Kahn's
/// algorithm over the in-degree induced by child→parent edges, breaking ties
/// by descending committer timestamp to match git's tendency to surface newer
/// commits first.
fn topo_order(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    start: &ObjectId,
) -> Result<Vec<ObjectId>> {
    // Gather the reachable subgraph and each commit's parents + timestamp.
    let mut parents_of: HashMap<ObjectId, Vec<ObjectId>> = HashMap::new();
    let mut timestamp_of: HashMap<ObjectId, i64> = HashMap::new();
    let mut stack = vec![start.clone()];
    while let Some(oid) = stack.pop() {
        if parents_of.contains_key(&oid) {
            continue;
        }
        let object = db.read_object(&oid)?;
        if object.object_type != ObjectType::Commit {
            return Err(GitError::InvalidObject(format!(
                "expected commit {oid}, found {}",
                object.object_type.as_str()
            )));
        }
        let commit = Commit::parse(format, &object.body)?;
        let ts = for_each_ref_identity_timestamp(&commit.committer).unwrap_or(0);
        timestamp_of.insert(oid.clone(), ts);
        let parents = commit.parents.clone();
        for parent in &parents {
            stack.push(parent.clone());
        }
        parents_of.insert(oid, parents);
    }

    // child_count[parent] = number of in-subgraph children pointing at it.
    let mut child_count: HashMap<ObjectId, usize> = HashMap::new();
    for oid in parents_of.keys() {
        child_count.entry(oid.clone()).or_insert(0);
    }
    for parents in parents_of.values() {
        for parent in parents {
            if parents_of.contains_key(parent) {
                *child_count.entry(parent.clone()).or_insert(0) += 1;
            }
        }
    }

    // Ready set: commits with no remaining children. `pop_newest` selects the
    // newest deterministically each step, so the collection order here (a
    // HashMap iteration) does not affect the result.
    let mut ready: Vec<ObjectId> = child_count
        .iter()
        .filter(|(_, count)| **count == 0)
        .map(|(oid, _)| oid.clone())
        .collect();

    let mut order = Vec::with_capacity(parents_of.len());
    while let Some(next) = pop_newest(&mut ready, &timestamp_of) {
        order.push(next.clone());
        if let Some(parents) = parents_of.get(&next) {
            for parent in parents.clone() {
                if let Some(count) = child_count.get_mut(&parent) {
                    *count -= 1;
                    if *count == 0 {
                        ready.push(parent);
                    }
                }
            }
        }
    }

    // If the graph had a cycle (corrupt history), append anything left so we
    // never silently drop commits.
    if order.len() < parents_of.len() {
        for oid in parents_of.keys() {
            if !order.contains(oid) {
                order.push(oid.clone());
            }
        }
    }
    Ok(order)
}

/// Remove and return the newest commit from `ready` (treated as a priority
/// queue keyed by descending timestamp, ties broken by ascending hex id for
/// determinism).
fn pop_newest(ready: &mut Vec<ObjectId>, timestamp_of: &HashMap<ObjectId, i64>) -> Option<ObjectId> {
    if ready.is_empty() {
        return None;
    }
    let mut best = 0usize;
    for i in 1..ready.len() {
        let ti = timestamp_of.get(&ready[i]).copied().unwrap_or(0);
        let tb = timestamp_of.get(&ready[best]).copied().unwrap_or(0);
        if ti > tb || (ti == tb && ready[i].to_hex() < ready[best].to_hex()) {
            best = i;
        }
    }
    Some(ready.swap_remove(best))
}

/// Resolve the `-L` ranges to a sorted, de-duplicated set of 1-based line
/// numbers to display. With no ranges, all lines are selected.
fn select_lines(lines: &[LineBlame], options: &BlameOptions, path: &str) -> Result<Vec<usize>> {
    let total = lines.len();
    if options.ranges.is_empty() {
        return Ok((1..=total).collect());
    }
    let mut selected: BTreeSet<usize> = BTreeSet::new();
    for range in &options.ranges {
        let (start, end) = resolve_range(range, total, path)?;
        if start > end {
            // git accepts start>end (both valid) and prints nothing for it.
            continue;
        }
        for line in start..=end {
            selected.insert(line);
        }
    }
    Ok(selected.into_iter().collect())
}

/// Resolve one `-L` range against the file's `total` line count, applying git's
/// defaults and error messages.
fn resolve_range(range: &RawRange, total: usize, path: &str) -> Result<(usize, usize)> {
    let start = match range.start {
        RangeBound::Omitted => 1,
        RangeBound::Absolute(n) => n,
        // `+N` is only meaningful as an end bound; the parser already rejects a
        // relative start, so this is defensive and reports a usage error.
        RangeBound::Relative(_) => return Err(blame_usage_error()),
    };
    if start == 0 {
        eprintln!("fatal: -L invalid line number: 0");
        return Err(GitError::Exit(128));
    }
    if start > total {
        eprintln!("fatal: file {path} has only {total} lines");
        return Err(GitError::Exit(128));
    }
    let end = match range.end {
        RangeBound::Omitted => total,
        RangeBound::Absolute(n) => {
            if n == 0 {
                eprintln!("fatal: -L invalid line number: 0");
                return Err(GitError::Exit(128));
            }
            n.min(total)
        }
        RangeBound::Relative(count) => (start + count.saturating_sub(1)).min(total),
    };
    Ok((start, end))
}

/// Print the selected lines in git blame's default format.
fn render_blame(
    git_dir: &Path,
    format: ObjectFormat,
    lines: &[LineBlame],
    selected: &[usize],
    options: &BlameOptions,
) -> Result<()> {
    let abbrev = blame_display_abbrev(git_dir, format, options)?;
    let hex_width = abbrev + 1;

    // Column widths are computed over the displayed lines only, matching git
    // (e.g. `-L 2,2` does not pad the line number to the whole file's width).
    let max_lineno = selected.iter().copied().max().unwrap_or(1);
    let lineno_width = decimal_width(max_lineno);

    // Author/email column width: the longest rendered author string among the
    // displayed lines. Only computed when the author column is shown.
    let mut author_strings: Vec<String> = Vec::new();
    let mut date_strings: Vec<String> = Vec::new();
    if !options.suppress_author {
        for &line_no in selected {
            let blame = &lines[line_no - 1];
            let (author, date) = author_and_date(git_dir, format, blame, options)?;
            author_strings.push(author);
            date_strings.push(date);
        }
    }
    let author_width = author_strings.iter().map(String::len).max().unwrap_or(0);

    let stdout = io::stdout();
    let mut handle = stdout.lock();
    for (display_idx, &line_no) in selected.iter().enumerate() {
        let blame = &lines[line_no - 1];
        let sha = render_sha(&blame.commit, abbrev, blame.boundary, options.show_root, hex_width);

        // Strip the trailing newline from the stored content; we always emit a
        // newline ourselves, matching git which prints one line per entry even
        // when the final source line lacked a newline. The line bytes are
        // written raw (not via a lossy String) so non-UTF-8 content round-trips
        // exactly.
        let content = strip_trailing_newline(&blame.content);

        if options.suppress_author {
            // `<sha> <lineno>) <line>`
            write!(handle, "{sha} {line_no:>lineno_width$}) ")?;
        } else {
            let author = &author_strings[display_idx];
            let date = &date_strings[display_idx];
            // `<sha> (<author-padded> <date> <lineno>) <line>`
            write!(
                handle,
                "{sha} ({author:<author_width$} {date} {line_no:>lineno_width$}) "
            )?;
        }
        handle.write_all(content)?;
        handle.write_all(b"\n")?;
    }
    Ok(())
}

/// Compute the abbreviation length for object names. git blame displays
/// `core.abbrev` (default 7) hex digits for boundary commits and one more for
/// non-boundary commits, so the column width is `abbrev + 1`.
fn blame_display_abbrev(
    git_dir: &Path,
    format: ObjectFormat,
    options: &BlameOptions,
) -> Result<usize> {
    if options.long_sha {
        // Full object name; the boundary marker replaces one leading digit so
        // the displayed boundary form is `^` + (hex_len - 1) digits.
        return Ok(format.hex_len() - 1);
    }
    let configured = match options.abbrev_override {
        Some(0) | None => repository_abbrev(git_dir, format)?.unwrap_or(format.hex_len()),
        Some(n) => n.clamp(1, format.hex_len()),
    };
    // The displayed boundary form is `^` + `configured` digits; non-boundary
    // is `configured + 1` digits. Cap so non-boundary never exceeds hex_len.
    Ok(configured.min(format.hex_len() - 1))
}

/// Render the object-name column for one entry.
///
/// Non-boundary: `abbrev + 1` hex digits. Boundary: `^` followed by `abbrev`
/// hex digits. Both occupy `hex_width` columns. `-l` widens both to the full
/// object name. `--root` suppresses the boundary marker.
fn render_sha(
    commit: &ObjectId,
    abbrev: usize,
    boundary: bool,
    show_root: bool,
    hex_width: usize,
) -> String {
    let hex = commit.to_hex();
    if boundary && !show_root {
        let body: String = hex.chars().take(abbrev).collect();
        format!("^{body}")
    } else {
        let body: String = hex.chars().take(hex_width).collect();
        body
    }
}

/// Produce the author and date strings for one entry given the active options.
fn author_and_date(
    git_dir: &Path,
    format: ObjectFormat,
    blame: &LineBlame,
    options: &BlameOptions,
) -> Result<(String, String)> {
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let object = db.read_object(&blame.commit)?;
    let commit = Commit::parse(format, &object.body)?;
    let identity = &commit.author;

    let author = match options.author_field {
        AuthorField::Name => for_each_ref_identity_name(identity)
            .map(|name| String::from_utf8_lossy(name).into_owned())
            .unwrap_or_default(),
        AuthorField::Email => for_each_ref_identity_email(identity, ForEachRefEmailMode::Bracketed)
            .map(|email| String::from_utf8_lossy(email).into_owned())
            .unwrap_or_default(),
    };

    // The default date column is ISO `YYYY-MM-DD HH:MM:SS +ZZZZ` (in the
    // author's timezone); `-t` selects the raw `<seconds> <tz>` form. Both are
    // produced by the shared date-mode formatter.
    let date_mode = match options.date_field {
        DateField::Iso => ForEachRefDateMode::Iso,
        DateField::Raw => ForEachRefDateMode::Raw,
    };
    let date = for_each_ref_identity_date(identity, date_mode).unwrap_or_default();

    Ok((author, date))
}

/// Strip a single trailing `\n` from a stored line. A `\r` (CRLF) is left in
/// place: git blame prints the raw line bytes up to but not including the final
/// newline, so a CRLF line still shows its carriage return.
fn strip_trailing_newline(content: &[u8]) -> &[u8] {
    content.strip_suffix(b"\n").unwrap_or(content)
}

/// Number of decimal digits in `value` (at least 1).
fn decimal_width(value: usize) -> usize {
    let mut width = 1;
    let mut v = value;
    while v >= 10 {
        v /= 10;
        width += 1;
    }
    width
}

const BLAME_USAGE: &str = "usage: git blame [<options>] [<rev-opts>] [<rev>] [--] <file>\n";

fn blame_usage_error() -> GitError {
    eprint!("{BLAME_USAGE}");
    GitError::Exit(129)
}

fn blame_too_many_paths() -> GitError {
    eprintln!("fatal: git blame supports blaming a single path at a time");
    GitError::Exit(129)
}

fn blame_option_requires_value(option: &str) -> GitError {
    eprintln!("error: switch `{option}' requires a value");
    eprint!("{BLAME_USAGE}");
    GitError::Exit(129)
}
