//! `git blame` — line-by-line authorship for a tracked path.
//!
//! Walks history from a start commit (default `HEAD`) toward the roots,
//! attributing each line of the path's final image to the commit that last
//! introduced it. The traversal diffs every commit's blob against its
//! parent(s) with the same Myers line diff the rest of the suite uses
//! (`sley_diff_merge`); a line that is unchanged from a parent is "passed
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

// Glob the crate root for shared plumbing (RepositoryContext, repository_abbrev,
// FileObjectDatabase, FileRefStore, Commit, Tree, the identity/date formatting
// helpers, and so on). See commands::stash for the rationale: a submodule can
// reach its ancestor module's private items, so everything visible at the crate
// root is in scope here without re-listing it.
use crate::*;
use sley_object::TreeEntries;

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

/// Positional arguments to `git blame`, before rev-vs-path disambiguation
/// (which needs the repository to test whether a token names a revision).
struct BlamePositionals {
    /// Positionals seen before any `--`.
    bare: Vec<String>,
    /// Paths seen after `--` (these are always paths, never revisions).
    after_dd: Vec<String>,
}

/// Parsed `git blame` invocation.
struct BlameOptions {
    /// Raw positionals; resolved into (rev, path) in `run_blame`.
    positionals: BlamePositionals,
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
    /// Follow only the first parent of merges (`--first-parent`).
    first_parent: bool,
    /// Emit `git annotate`-compatible output (`-c`, or the `annotate` command):
    /// `<hex>\t(<author>\t<date>\t<lineno>)<content>`, no boundary `^`.
    compat: bool,
    /// `-b`: blank out the object name of boundary commits (render the hex
    /// column as spaces instead of `^`+hash).
    blank_boundary: bool,
    /// Whether the author column (`-e`/`--show-email`/`--no-show-email`) was set
    /// on the command line; if not, `blame.showEmail` config supplies the
    /// default.
    author_field_explicit: bool,
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
    /// A forward relative offset from the start (`+N`), only valid as the end:
    /// end = start + N - 1.
    Relative(usize),
    /// A backward relative offset from the start (`-N`), only valid as the end:
    /// end = start - N (git's `-L X,-N`, which yields a reversed span).
    RelativeNeg(usize),
    /// A `/regex/` bound: the first line matching `pattern` at or after the
    /// search anchor (the previous range's end + 1, or line 1 when
    /// `absolute`). `^/regex/` forces the absolute anchor.
    Regex { pattern: String, absolute: bool },
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
    run_blame(args, false)
}

/// `git annotate` — `git blame` with the annotate-compatible output mode forced
/// on (equivalent to `git blame -c`). Shares all of blame's parsing and the
/// scoreboard; only the output format differs.
pub(crate) fn cmd_annotate(args: &[String]) -> Result<()> {
    run_blame(args, true)
}

fn run_blame(args: &[String], force_compat: bool) -> Result<()> {
    let mut options = match parse_blame_args(args)? {
        BlameArgs::Run(mut options) => {
            if force_compat {
                options.compat = true;
            }
            options
        }
        BlameArgs::Help => {
            print!("{BLAME_USAGE}");
            return Ok(());
        }
    };

    let repo = RepositoryContext::discover_current()?;
    let cwd = repo.cwd();
    let git_dir = repo.git_dir();
    let format = repo.format();
    let db = repo.objects();

    // `blame.showEmail` config supplies the author-column default when no
    // `-e`/`--show-email`/`--no-show-email` was given on the command line.
    if !options.author_field_explicit
        && let Some(true) = blame_config_bool(git_dir, "showemail")?
    {
        options.author_field = AuthorField::Email;
    }

    // Disambiguate rev vs path now that the repository is available: a token
    // "is a rev" if it resolves to an object, matching git's `is_a_rev`. A
    // leading `^` marks a boundary rev (`git blame ^<rev>`); strip it before
    // testing/resolving.
    let (rev, path) = resolve_positionals(&options.positionals, |tok| {
        let core = tok.strip_prefix('^').unwrap_or(tok);
        repo.resolve_revision(core).is_ok()
    })?;

    // A `^<rev>` rev makes `<rev>` (and its ancestors) an uninteresting
    // *boundary*: the blame walk stops there and renders it with `^`. The final
    // image then comes from HEAD (the default), not from the boundary rev.
    let (rev_spec, boundary_tip): (String, Option<ObjectId>) = match &rev {
        Some(r) if r.starts_with('^') => {
            let core = &r[1..];
            let oid = repo.resolve_revision(core)?;
            let tip = sley_rev::peel_to_commit(db, format, &oid)?;
            ("HEAD".to_string(), Some(tip))
        }
        Some(r) => (r.clone(), None),
        None => ("HEAD".to_string(), None),
    };

    // Resolve the requested revision (default HEAD) to a commit.
    let start_oid = repo.resolve_revision(&rev_spec)?;
    let start_commit = sley_rev::peel_to_commit(db, format, &start_oid)?;

    // Turn the cwd-relative path into a repository-root-relative path the way
    // git's pathspec handling does, then locate the blob at the start commit.
    //
    // TODO(convert): blame's only convert step in upstream is `convert_to_git`
    // (clean) in `setup_scoreboard`, applied to *working-tree*-sourced content
    // (a dirty worktree copy or `--contents <file>`) to normalize it before
    // diffing against committed blobs. Committed blobs read from the object
    // store (`fill_origin_blob`) are NOT converted — they are already in
    // git-normalized form. sley's blame always reads its final image from
    // committed blobs (the working-tree overlay and `--contents` are
    // unimplemented; see the module doc), so there is nothing to convert here
    // and applying smudge would diverge from git. When the working-tree overlay
    // lands, route that worktree-sourced content through
    // `sley_worktree::apply_clean_filter` before it enters `compute_blame`.
    let repo_path = blame_repo_relative_path(cwd, git_dir, &path)?;
    let final_blob = match read_path_blob(db, format, &start_commit, &repo_path)? {
        Some(blob) => blob,
        None => {
            // git reports the repository-relative path here, not the literal
            // argument (so blaming a missing file from a subdirectory still
            // names `<dir>/<file>`).
            eprintln!("fatal: no such path '{repo_path}' in {rev_spec}");
            return Err(GitError::Exit(128));
        }
    };

    let lines = compute_blame(
        db,
        format,
        &start_commit,
        &repo_path,
        &final_blob,
        options.first_parent,
        boundary_tip,
    )?;

    // Resolve the -L ranges against the real line count, then render. The
    // repo-relative path is used for any -L error message, matching git.
    let selected = select_lines(&lines, &options, &repo_path)?;
    if selected.is_empty() {
        return Ok(());
    }
    render_blame(git_dir, format, db, &lines, &selected, &options)
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
    let mut author_field_explicit = false;
    let mut date_field = DateField::Iso;
    let mut show_root = false;
    let mut abbrev_override = None;
    let mut ranges = Vec::new();
    let mut first_parent = false;
    let mut compat = false;
    let mut blank_boundary = false;
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
            "-e" | "--show-email" => {
                author_field = AuthorField::Email;
                author_field_explicit = true;
            }
            "--no-show-email" => {
                author_field = AuthorField::Name;
                author_field_explicit = true;
            }
            "-t" => date_field = DateField::Raw,
            "--root" => show_root = true,
            "--no-root" => show_root = false,
            "--first-parent" => first_parent = true,
            "-c" => compat = true,
            "-b" => blank_boundary = true,
            "--abbrev" => abbrev_override = Some(0),
            // `--no-abbrev` shows the full object name, like `-l` / `--abbrev`
            // with the full hash length.
            "--no-abbrev" => long_sha = true,
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
                        "git blame option {other} is not supported by sley"
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

    // Collect the positionals for rev/path disambiguation; the rev-vs-path
    // decision for the ambiguous `blame X Y` form needs the repository (to test
    // whether a token resolves as a revision), so it is deferred to `run_blame`.
    let positionals = BlamePositionals {
        bare: positionals,
        after_dd: paths_after_dd,
    };

    Ok(BlameArgs::Run(BlameOptions {
        positionals,
        long_sha,
        suppress_author,
        author_field,
        date_field,
        show_root,
        abbrev_override,
        ranges,
        first_parent,
        compat,
        blank_boundary,
        author_field_explicit,
    }))
}

/// Decide which positional is the revision and which is the path, using the
/// repository to disambiguate the `blame X Y` form the way git's builtin does
/// (blame.c cases 1a/1b/2a/2b): with no `--`, two positionals where the *last*
/// names a revision are `blame <path> <rev>` (e.g. `git blame file main`);
/// otherwise the last is the path and the first the rev.
fn resolve_positionals(
    positionals: &BlamePositionals,
    is_rev: impl Fn(&str) -> bool,
) -> Result<(Option<String>, String)> {
    let bare = &positionals.bare;
    if !positionals.after_dd.is_empty() {
        // `blame [<rev>] -- <path>` or `blame -- <path> <rev>`.
        if positionals.after_dd.len() > 1 {
            return Err(blame_too_many_paths());
        }
        let rev = match bare.len() {
            0 => None,
            1 => Some(bare[0].clone()),
            _ => return Err(blame_usage_error()),
        };
        return Ok((rev, positionals.after_dd[0].clone()));
    }
    match bare.len() {
        0 => Err(blame_usage_error()),
        1 => Ok((None, bare[0].clone())),
        2 => {
            if is_rev(&bare[1]) {
                // `blame <path> <rev>` — last token is the revision.
                Ok((Some(bare[1].clone()), bare[0].clone()))
            } else {
                // `blame <rev> <path>` — last token is the path.
                Ok((Some(bare[0].clone()), bare[1].clone()))
            }
        }
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
    // The `:funcname` / `:/regex/` function-name range forms are recognized by
    // git but not implemented here; report them as unsupported.
    if value.starts_with(':') || value.starts_with("^:") {
        return Err(GitError::Unsupported(format!(
            "git blame -L {value} (function-name range) is not supported by sley"
        )));
    }
    // Split on the FIRST `,` that is not inside a `/regex/` (a regex may contain
    // a comma), so `-L/a,b/,/c/` splits into `/a,b/` and `/c/`.
    let (start_raw, end_raw) = split_range_at_comma(value);
    let start = parse_range_bound(start_raw, false)?;
    let end = parse_range_bound(end_raw, true)?;
    Ok(RawRange { start, end })
}

/// Split a `-L` argument at the first top-level `,` (one not inside a `/.../`
/// regex). Returns `(start, end)`; `end` is empty when there is no comma.
fn split_range_at_comma(value: &str) -> (&str, &str) {
    let bytes = value.as_bytes();
    let mut i = 0;
    let mut in_regex = false;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if in_regex => i += 1, // skip the escaped char
            b'/' => in_regex = !in_regex,
            b',' if !in_regex => return (&value[..i], &value[i + 1..]),
            _ => {}
        }
        i += 1;
    }
    (value, "")
}

/// Parse one `-L` bound. `is_end` allows the `+N` relative form. A token that
/// cannot be read as a (non-negative) line number is a usage error.
fn parse_range_bound(raw: &str, is_end: bool) -> Result<RangeBound> {
    if raw.is_empty() {
        return Ok(RangeBound::Omitted);
    }
    // `/regex/` and `^/regex/` bounds: the first matching line at/after the
    // search anchor; `^` forces the absolute (line-1) anchor.
    let (regex_body, absolute) = match raw.strip_prefix('^') {
        Some(rest) if rest.starts_with('/') => (Some(rest), true),
        _ if raw.starts_with('/') => (Some(raw), false),
        _ => (None, false),
    };
    if let Some(body) = regex_body {
        // Strip the surrounding slashes; a trailing `/` is required.
        let inner = body
            .strip_prefix('/')
            .and_then(|s| s.strip_suffix('/'))
            .ok_or_else(blame_usage_error)?;
        return Ok(RangeBound::Regex {
            pattern: inner.to_string(),
            absolute,
        });
    }
    if let Some(rest) = raw.strip_prefix('+') {
        if !is_end {
            return Err(blame_usage_error());
        }
        let count = rest.parse::<usize>().map_err(|_| blame_usage_error())?;
        return Ok(RangeBound::Relative(count));
    }
    if let Some(rest) = raw.strip_prefix('-') {
        // `-N` is a backward-relative *end* bound (`-L X,-N`); as a start it is
        // a usage error (git's parser rejects a leading-dash start too).
        if !is_end {
            return Err(blame_usage_error());
        }
        let count = rest.parse::<usize>().map_err(|_| blame_usage_error())?;
        return Ok(RangeBound::RelativeNeg(count));
    }
    // Only absolute numbers and the `±N` end forms are accepted; anything else
    // (e.g. `abc`, `1.5`) is a usage error, matching git.
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
    let tree_oid = sley_rev::peel_to_tree(db, format, commit)?;
    let Some(blob_oid) = lookup_tree_path(db, format, &tree_oid, repo_path)? else {
        return Ok(None);
    };
    let object = db.read_object(&blob_oid)?;
    if object.object_type != ObjectType::Blob {
        return Ok(None);
    }
    Ok(Some(object.body.clone()))
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
    let mut current = *tree_oid;
    let last = components.len() - 1;
    for (idx, component) in components.iter().enumerate() {
        let object = db.read_object(&current)?;
        if object.object_type != ObjectType::Tree {
            return Ok(None);
        }
        let mut matched = None;
        for entry in TreeEntries::new(format, &object.body) {
            let entry = entry?;
            if matched.is_none() && entry.name == component.as_bytes() {
                matched = Some((entry.mode, entry.oid));
            }
        }
        let Some((mode, oid)) = matched else {
            return Ok(None);
        };
        if idx == last {
            return Ok(Some(oid));
        }
        if sley_object::tree_entry_object_type(mode) != ObjectType::Tree {
            return Ok(None);
        }
        current = oid;
    }
    Ok(None)
}

/// A contiguous run of final-image lines currently suspected to originate in
/// one commit's version of the path. Mirrors git's `struct blame_entry`
/// (blame.c): `lno` is the 0-based line in the *final image*, `s_lno` the
/// 0-based line in the *suspect's* blob, and the entry covers `num_lines`
/// consecutive lines in both. The invariant `final[lno + k] == suspect[s_lno +
/// k]` for `k in 0..num_lines` is what lets us pass whole chunks to a parent.
#[derive(Clone)]
struct BlameEntry {
    /// Commit currently suspected of introducing these lines.
    suspect: ObjectId,
    /// Whether the suspect is a rendered boundary (root, absent `--root`).
    boundary: bool,
    /// 0-based start line in the final image.
    lno: usize,
    /// 0-based start line in the suspect's blob.
    s_lno: usize,
    /// Number of lines this entry covers (in both the final image and the
    /// suspect's blob).
    num_lines: usize,
}

/// Core blame: assign each final-image line to a commit, mirroring git's
/// diff-driven multi-pass scoreboard (blame.c `pass_blame` / `blame_chunk` /
/// `split_overlap`).
///
/// Rather than routing each final line independently to the first parent that
/// preserves it, we carry line *chunks* (`BlameEntry`) and, for each commit
/// pulled off a commit-date priority queue, diff that commit's blob against
/// each parent in turn. Lines a parent preserves migrate to the parent as
/// (possibly split) chunks with their `s_lno` rebased onto the parent's blob;
/// lines no parent preserves are charged to this commit. For a merge this
/// gives the *correct* parent (parent 0 first, the residual to parent 1, …),
/// which whole-line first-parent routing got wrong — and `-L` is simply a
/// final-range filter applied at render time over correctly-attributed chunks.
fn compute_blame(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    start_commit: &ObjectId,
    repo_path: &str,
    final_blob: &[u8],
    first_parent: bool,
    boundary_tip: Option<ObjectId>,
) -> Result<Vec<LineBlame>> {
    let final_lines = sley_diff_merge::split_lines(final_blob);
    let line_count = final_lines.len();

    // Final attribution per line, filled in as commits are found guilty.
    let mut result: Vec<Option<LineBlame>> = (0..line_count).map(|_| None).collect();
    if line_count == 0 {
        return Ok(Vec::new());
    }

    // `git blame ^<rev>`: `<rev>` and all its ancestors are uninteresting
    // boundaries — when the walk reaches one, it charges the lines there as a
    // boundary and does not recurse into parents. Precompute that closure.
    let uninteresting = match boundary_tip {
        Some(tip) => ancestors_closure(db, format, &tip)?,
        None => HashSet::new(),
    };

    // Per-commit suspect chunks ("origin->suspects" in git). A commit is in the
    // queue iff it has at least one suspect chunk. The blob for each commit's
    // path is cached on first use.
    let mut suspects: HashMap<ObjectId, Vec<BlameEntry>> = HashMap::new();
    let mut blob_cache: HashMap<ObjectId, Option<Vec<u8>>> = HashMap::new();
    let mut date_cache: HashMap<ObjectId, i64> = HashMap::new();

    // The start commit owns the entire final image as one chunk.
    suspects.insert(
        *start_commit,
        vec![BlameEntry {
            suspect: *start_commit,
            boundary: false,
            lno: 0,
            s_lno: 0,
            num_lines: line_count,
        }],
    );

    // Commit-date priority queue, newest first (git's
    // compare_commits_by_commit_date). We materialise the comparator lazily via
    // `pop_newest_commit`, caching each commit's date.
    let mut queue: Vec<ObjectId> = vec![*start_commit];

    while let Some(commit_oid) = pop_newest_commit(&mut queue, db, format, &mut date_cache)? {
        let Some(mut owned) = suspects.remove(&commit_oid) else {
            continue;
        };
        if owned.is_empty() {
            continue;
        }

        // Uninteresting (`^<rev>`) commits are boundaries: charge their lines
        // with the boundary marker and stop — do not pass blame to parents.
        if uninteresting.contains(&commit_oid) {
            charge_remaining(&mut result, &final_lines, &commit_oid, true, owned);
            continue;
        }

        // Resolve this commit and its blob for the path.
        let commit_obj = db.read_object(&commit_oid)?;
        let commit = Commit::parse(format, &commit_obj.body)?;
        let child_blob = cached_blob(db, format, &commit_oid, repo_path, &mut blob_cache)?;
        let Some(child_blob) = child_blob else {
            // The path is absent at this commit (shouldn't normally happen for a
            // suspect): charge everything here so no line is lost.
            charge_remaining(&mut result, &final_lines, &commit_oid, false, owned);
            continue;
        };
        let child_lines = sley_diff_merge::split_lines(&child_blob);

        let mut parents = commit.parents.clone();
        if first_parent {
            parents.truncate(1);
        }
        if parents.is_empty() {
            // Root commit (or `--first-parent` past a root): every remaining
            // line is its own. Render as a boundary unless `--root`.
            charge_remaining(&mut result, &final_lines, &commit_oid, true, owned);
            continue;
        }

        // Pass blame to each parent in order. `owned` shrinks as parents claim
        // chunks; whatever remains after the last parent is charged here.
        for parent in &parents {
            if owned.is_empty() {
                break;
            }
            let parent_blob = cached_blob(db, format, parent, repo_path, &mut blob_cache)?;
            let Some(parent_blob) = parent_blob else {
                // Path absent in this parent: it preserves nothing, so all
                // chunks stay with the current commit for the next parent.
                continue;
            };

            // Whole-file shortcut: if the parent's blob is byte-identical, every
            // remaining chunk passes through unchanged (git's
            // pass_whole_blame / oideq(blob_oid) fast path).
            if parent_blob == child_blob {
                let passed = std::mem::take(&mut owned);
                queue_entries(&mut suspects, &mut queue, *parent, passed);
                break;
            }

            let parent_lines = sley_diff_merge::split_lines(&parent_blob);
            let mut still_ours = Vec::new();
            let passed = pass_blame_to_parent(&parent_lines, &child_lines, *parent, &mut owned);
            still_ours.append(&mut owned);
            owned = still_ours;
            if !passed.is_empty() {
                queue_entries(&mut suspects, &mut queue, *parent, passed);
            }
        }

        // Anything still suspected of this commit after every parent had a turn
        // is genuinely this commit's: charge it (non-boundary — it has parents).
        if !owned.is_empty() {
            charge_remaining(&mut result, &final_lines, &commit_oid, false, owned);
        }
    }

    // Any line not resolved (shouldn't happen) falls back to the start commit
    // as a non-boundary so output stays well-formed.
    let mut out = Vec::with_capacity(line_count);
    for (line_index, slot) in result.into_iter().enumerate() {
        match slot {
            Some(blame) => out.push(blame),
            None => out.push(LineBlame {
                commit: *start_commit,
                boundary: false,
                content: final_lines[line_index].content.to_vec(),
            }),
        }
    }
    Ok(out)
}

/// Diff the suspect blob (`child_lines`) against `parent_lines` and split every
/// chunk in `owned` along the diff boundaries: lines unchanged from the parent
/// migrate to `parent` (returned, with `s_lno` rebased onto the parent's blob);
/// lines that differ remain in `owned` for the next parent (or to be charged to
/// the suspect). This is the port of git's `blame_chunk` driven by
/// `blame_chunk_cb` over `diff_hunks` (blame.c).
///
/// The diff is computed *parent → child* so a hunk `(start_a, count_a) ->
/// (start_b, count_b)` says child lines `[start_b, start_b+count_b)` differ from
/// the parent, while the run before each hunk (and after the last) is common.
/// `offset = start_a - start_b` is how far the parent's line numbers lead the
/// child's across a common run.
fn pass_blame_to_parent(
    parent_lines: &[sley_diff_merge::DiffLine<'_>],
    child_lines: &[sley_diff_merge::DiffLine<'_>],
    _parent: ObjectId,
    owned: &mut Vec<BlameEntry>,
) -> Vec<BlameEntry> {
    let hunks = diff_hunks(parent_lines, child_lines);

    let mut passed: Vec<BlameEntry> = Vec::new();
    let mut still_ours: Vec<BlameEntry> = Vec::new();

    // Process chunks in `s_lno` (child line) order so the running `offset`
    // — how far the parent's line numbers lead the child's across the current
    // common run — stays valid. `deferred` holds split tails to re-merge in
    // order; `entries` is the original sorted suspect list.
    owned.sort_by_key(|e| e.s_lno);
    let mut entries = std::mem::take(owned).into_iter().peekable();
    let mut deferred: Vec<BlameEntry> = Vec::new();
    let mut offset: isize = 0;

    for hunk in &hunks {
        let tlno = hunk.start_b; // first child line that differs
        let same = hunk.start_b + hunk.count_b; // first child line common again

        // Pre-chunk region [.., tlno): common with the parent → pass it through.
        while let Some(mut e) = take_next_before(&mut deferred, &mut entries, tlno) {
            if e.s_lno + e.num_lines > tlno {
                // Straddles the boundary: pass the common head, defer the tail.
                let head_len = tlno - e.s_lno;
                let tail = split_entry_at(&mut e, head_len);
                pass_entry(&mut e, offset, &mut passed);
                put_back(&mut deferred, tail);
            } else {
                pass_entry(&mut e, offset, &mut passed);
            }
        }

        // Differing region [tlno, same): stays with the suspect; split a chunk
        // that reaches past `same` so its common tail is handled by a later hunk.
        while let Some(mut e) = take_next_before(&mut deferred, &mut entries, same) {
            if e.s_lno + e.num_lines > same {
                let head_len = same - e.s_lno;
                let tail = split_entry_at(&mut e, head_len);
                still_ours.push(e);
                put_back(&mut deferred, tail);
            } else {
                still_ours.push(e);
            }
        }

        // Advance the offset across this hunk (parent count - child count delta).
        offset = hunk.start_a as isize + hunk.count_a as isize
            - (hunk.start_b as isize + hunk.count_b as isize);
    }

    // Everything after the last hunk is common with the parent.
    while let Some(mut e) = take_next_before(&mut deferred, &mut entries, usize::MAX) {
        pass_entry(&mut e, offset, &mut passed);
    }

    *owned = still_ours;
    passed
}

/// A changed region of a parent→child line diff: parent lines
/// `[start_a, start_a+count_a)` were replaced by child lines
/// `[start_b, start_b+count_b)`. Equivalent to one `xdl` hunk / one
/// `blame_chunk_cb` call in git.
struct DiffHunk {
    start_a: usize,
    count_a: usize,
    start_b: usize,
    count_b: usize,
}

/// Convert the run-length `DiffOp` script (parent → child) into the changed
/// hunks git's `diff_hunks` yields: maximal runs of non-`Equal` ops collapse
/// into a single `(start_a, count_a, start_b, count_b)` hunk; `Equal` runs are
/// the common stretches between hunks.
fn diff_hunks(
    parent_lines: &[sley_diff_merge::DiffLine<'_>],
    child_lines: &[sley_diff_merge::DiffLine<'_>],
) -> Vec<DiffHunk> {
    let ops = sley_diff_merge::myers_diff_lines(parent_lines, child_lines);
    let mut hunks = Vec::new();
    let mut a = 0usize; // parent line cursor
    let mut b = 0usize; // child line cursor
    let mut pending: Option<DiffHunk> = None;
    for op in ops {
        match op {
            sley_diff_merge::DiffOp::Equal(n) => {
                if let Some(h) = pending.take() {
                    hunks.push(h);
                }
                a += n;
                b += n;
            }
            sley_diff_merge::DiffOp::Delete(n) => {
                let h = pending.get_or_insert(DiffHunk {
                    start_a: a,
                    count_a: 0,
                    start_b: b,
                    count_b: 0,
                });
                h.count_a += n;
                a += n;
            }
            sley_diff_merge::DiffOp::Insert(n) => {
                let h = pending.get_or_insert(DiffHunk {
                    start_a: a,
                    count_a: 0,
                    start_b: b,
                    count_b: 0,
                });
                h.count_b += n;
                b += n;
            }
        }
    }
    if let Some(h) = pending.take() {
        hunks.push(h);
    }
    hunks
}

/// Pass `entry` to a parent: rebase its `s_lno` by `offset` (the parent leads
/// the child by `offset` across this common run). The suspect id is left as the
/// child's; [`queue_entries`] re-stamps it with the parent before enqueuing.
fn pass_entry(entry: &mut BlameEntry, offset: isize, passed: &mut Vec<BlameEntry>) {
    let s_lno = (entry.s_lno as isize + offset) as usize;
    passed.push(BlameEntry {
        suspect: entry.suspect,
        boundary: false,
        lno: entry.lno,
        s_lno,
        num_lines: entry.num_lines,
    });
}

/// Split `e` into a head of `head_len` lines (kept in `e`) and a returned tail
/// covering the remainder, mirroring git's `split_blame_at`.
fn split_entry_at(e: &mut BlameEntry, head_len: usize) -> BlameEntry {
    let tail = BlameEntry {
        suspect: e.suspect,
        boundary: e.boundary,
        lno: e.lno + head_len,
        s_lno: e.s_lno + head_len,
        num_lines: e.num_lines - head_len,
    };
    e.num_lines = head_len;
    tail
}

/// Pop the next chunk whose `s_lno < limit`, in global `s_lno` order, drawing
/// from `deferred` (split tails) and `entries` (the original sorted suspect
/// chunks) — both individually sorted. Returns `None` when the next chunk in
/// order starts at or past `limit` (or both streams are exhausted).
fn take_next_before(
    deferred: &mut Vec<BlameEntry>,
    entries: &mut std::iter::Peekable<std::vec::IntoIter<BlameEntry>>,
    limit: usize,
) -> Option<BlameEntry> {
    // Determine which stream has the smaller front without consuming it.
    let from_deferred = match (deferred.first(), entries.peek()) {
        (Some(d), Some(e)) => d.s_lno <= e.s_lno,
        (Some(_), None) => true,
        (None, Some(_)) => false,
        (None, None) => return None,
    };
    let next_s_lno = if from_deferred {
        deferred[0].s_lno
    } else {
        entries.peek().unwrap().s_lno
    };
    if next_s_lno >= limit {
        return None;
    }
    if from_deferred {
        Some(deferred.remove(0))
    } else {
        entries.next()
    }
}

/// Return an entry to the deferred stream, keeping it sorted by `s_lno`.
fn put_back(deferred: &mut Vec<BlameEntry>, e: BlameEntry) {
    let pos = deferred
        .iter()
        .position(|d| d.s_lno > e.s_lno)
        .unwrap_or(deferred.len());
    deferred.insert(pos, e);
}

/// Queue a batch of chunks onto `parent`'s suspect list, stamping them with the
/// parent id and enqueuing the parent commit if it was not already pending.
fn queue_entries(
    suspects: &mut HashMap<ObjectId, Vec<BlameEntry>>,
    queue: &mut Vec<ObjectId>,
    parent: ObjectId,
    mut entries: Vec<BlameEntry>,
) {
    for e in &mut entries {
        e.suspect = parent;
    }
    let slot = suspects.entry(parent).or_default();
    let was_empty = slot.is_empty();
    slot.extend(entries);
    if was_empty {
        queue.push(parent);
    }
}

/// Charge every remaining chunk to `commit_oid` as a final attribution.
fn charge_remaining(
    result: &mut [Option<LineBlame>],
    final_lines: &[sley_diff_merge::DiffLine<'_>],
    commit_oid: &ObjectId,
    boundary: bool,
    owned: Vec<BlameEntry>,
) {
    for entry in owned {
        for k in 0..entry.num_lines {
            let final_line = entry.lno + k;
            if let Some(slot) = result.get_mut(final_line)
                && slot.is_none()
            {
                *slot = Some(LineBlame {
                    commit: *commit_oid,
                    boundary,
                    content: final_lines[final_line].content.to_vec(),
                });
            }
        }
    }
}

/// The set of `tip` and all commits reachable from it (its ancestor closure),
/// used to mark `git blame ^<rev>` boundaries as uninteresting.
fn ancestors_closure(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tip: &ObjectId,
) -> Result<HashSet<ObjectId>> {
    let mut seen: HashSet<ObjectId> = HashSet::new();
    let mut stack = vec![*tip];
    while let Some(oid) = stack.pop() {
        if !seen.insert(oid) {
            continue;
        }
        let object = db.read_object(&oid)?;
        if object.object_type != ObjectType::Commit {
            continue;
        }
        let commit = Commit::parse(format, &object.body)?;
        for parent in &commit.parents {
            stack.push(*parent);
        }
    }
    Ok(seen)
}

/// Read a `blame.<key>` boolean config value (e.g. `blame.showEmail`), checking
/// the repository config then the global config, mirroring git's precedence.
/// `key` is the lowercase, dotless tail (git config keys are case-insensitive).
/// Returns `None` when unset.
fn blame_config_bool(git_dir: &Path, key: &str) -> Result<Option<bool>> {
    // Repository config takes precedence over global.
    let config_path = git_dir.join("config");
    if let Ok(config) = GitConfig::read(config_path)
        && let Some(value) = config.get("blame", None, key)
    {
        return Ok(parse_config_bool(value));
    }
    if let Some(value) = global_config_value(&format!("blame.{key}"))? {
        return Ok(parse_config_bool(&value));
    }
    Ok(None)
}

/// Read (and memoise) the blob for `repo_path` at `commit`. `None` means the
/// path is absent (or names a non-blob) at that commit.
fn cached_blob(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    commit: &ObjectId,
    repo_path: &str,
    cache: &mut HashMap<ObjectId, Option<Vec<u8>>>,
) -> Result<Option<Vec<u8>>> {
    if let Some(hit) = cache.get(commit) {
        return Ok(hit.clone());
    }
    let blob = read_path_blob(db, format, commit, repo_path)?;
    cache.insert(*commit, blob.clone());
    Ok(blob)
}

/// Pop the newest-by-committer-date commit from the working queue, mirroring
/// git's commit-date priority queue (`compare_commits_by_commit_date`: larger
/// date first, ties broken deterministically by ascending hex id). Commit dates
/// are memoised in `date_cache`. Returns `None` when the queue is empty.
///
/// The queue may transiently hold the same commit twice (it is enqueued when
/// its suspect list first becomes non-empty); a duplicate pops with an empty
/// suspect list and the caller simply skips it, exactly as git's `assign_blame`
/// does.
fn pop_newest_commit(
    queue: &mut Vec<ObjectId>,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    date_cache: &mut HashMap<ObjectId, i64>,
) -> Result<Option<ObjectId>> {
    if queue.is_empty() {
        return Ok(None);
    }
    // Ensure every queued commit has a cached date.
    for oid in queue.iter() {
        if !date_cache.contains_key(oid) {
            let object = db.read_object(oid)?;
            if object.object_type != ObjectType::Commit {
                return Err(GitError::InvalidObject(format!(
                    "expected commit {oid}, found {}",
                    object.object_type.as_str()
                )));
            }
            let commit = Commit::parse(format, &object.body)?;
            let ts = for_each_ref_identity_timestamp(&commit.committer).unwrap_or(0);
            date_cache.insert(*oid, ts);
        }
    }
    let mut best = 0usize;
    for i in 1..queue.len() {
        let ti = date_cache.get(&queue[i]).copied().unwrap_or(0);
        let tb = date_cache.get(&queue[best]).copied().unwrap_or(0);
        if ti > tb || (ti == tb && queue[i].to_hex() < queue[best].to_hex()) {
            best = i;
        }
    }
    Ok(Some(queue.swap_remove(best)))
}

/// Resolve the `-L` ranges to a sorted, de-duplicated set of 1-based line
/// numbers to display. With no ranges, all lines are selected.
fn select_lines(lines: &[LineBlame], options: &BlameOptions, path: &str) -> Result<Vec<usize>> {
    let total = lines.len();
    if options.ranges.is_empty() {
        return Ok((1..=total).collect());
    }
    // The line *contents* (newline-stripped) feed the `/regex/` bound search.
    let contents: Vec<&[u8]> = lines
        .iter()
        .map(|l| strip_trailing_newline(&l.content))
        .collect();
    let mut selected: BTreeSet<usize> = BTreeSet::new();
    // git threads an `anchor` between ranges: a relative `/regex/` starts its
    // search at the previous range's end + 1.
    let mut anchor = 1usize;
    for range in &options.ranges {
        let (lo, hi) = resolve_range(range, total, &contents, anchor, path)?;
        for line in lo..=hi {
            selected.insert(line);
        }
        anchor = hi + 1;
    }
    Ok(selected.into_iter().collect())
}

/// Resolve a `/regex/` `-L` bound: the 1-based number of the first line at or
/// after `from` (1-based) whose content matches `pattern`. git compiles the
/// pattern as a POSIX *basic* regex (`regcomp` without `REG_EXTENDED`) with
/// `REG_NEWLINE`; we mirror that with sley's BRE engine. No match is the fatal
/// "no match" error git reports.
fn resolve_regex_bound(
    pattern: &str,
    contents: &[&[u8]],
    from: usize,
    _total: usize,
) -> Result<usize> {
    use crate::grep_source::{Regex, RegexMode};
    let re = Regex::compile(pattern, RegexMode::Bre, false, false).map_err(|_| {
        eprintln!("fatal: -L parameter '{pattern}': invalid regex");
        GitError::Exit(128)
    })?;
    let start_idx = from.saturating_sub(1); // 0-based
    for (idx, line) in contents.iter().enumerate().skip(start_idx) {
        if re.find_from(line, 0).is_some() {
            return Ok(idx + 1);
        }
    }
    eprintln!("fatal: -L parameter '{pattern}' starting at line {from}: No match");
    Err(GitError::Exit(128))
}

/// Resolve one `-L` range against the file's `total` line count, applying git's
/// defaults and error messages. Returns an inclusive `(lo, hi)` 1-based span,
/// already clamped to the file, that the caller can iterate directly.
///
/// git accepts a "reversed" range (`-L 5,2`): it resolves both endpoints and
/// then displays the *inclusive span between them*, i.e. `[min(start,end),
/// max(start,end)]`. The "file has only N lines" error is therefore keyed off
/// the *smaller* endpoint, not the literal start: `-L 100,2` lists lines 2..N
/// (no error), while `-L 100,200` — whose smaller endpoint is also past the end
/// — is the error. We mirror that exactly.
fn resolve_range(
    range: &RawRange,
    total: usize,
    contents: &[&[u8]],
    anchor: usize,
    path: &str,
) -> Result<(usize, usize)> {
    let start = match &range.start {
        RangeBound::Omitted => 1,
        RangeBound::Absolute(n) => *n,
        // The `/regex/` start searches from the running anchor (or line 1 for
        // the `^/regex/` absolute form).
        RangeBound::Regex { pattern, absolute } => {
            let from = if *absolute { 1 } else { anchor };
            resolve_regex_bound(pattern, contents, from, total)?
        }
        // `±N` is only meaningful as an end bound; the parser already rejects a
        // relative start, so this is defensive and reports a usage error.
        RangeBound::Relative(_) | RangeBound::RelativeNeg(_) => return Err(blame_usage_error()),
    };
    // git validates the start line number before anything else (so `-L 0,+5` and
    // `-L 0,3` both report the zero error rather than an empty-range/clamp).
    if start == 0 {
        eprintln!("fatal: -L invalid line number: 0");
        return Err(GitError::Exit(128));
    }

    // `begin` mirrors git's `*begin` (the start endpoint); `end` is the explicit
    // end (0 means "omitted", to be defaulted to end-of-file below — NOT to
    // `total` here, so a start past the end is still detected as an error).
    let mut begin = start;
    let mut end = match &range.end {
        RangeBound::Omitted => 0,
        RangeBound::Absolute(n) => {
            if *n == 0 {
                eprintln!("fatal: -L invalid line number: 0");
                return Err(GitError::Exit(128));
            }
            *n
        }
        // The `/regex/` end searches from `begin + 1` (git's `*begin + 1`). The
        // absolute `^/regex/` anchor is only valid as a *start* bound — git
        // rejects `-L X,^/RE/` as a usage error.
        RangeBound::Regex { pattern, absolute } => {
            if *absolute {
                return Err(blame_usage_error());
            }
            resolve_regex_bound(pattern, contents, start + 1, total)?
        }
        // `start,+0` is an empty range git rejects; else end = start + count - 1.
        RangeBound::Relative(count) => {
            if *count == 0 {
                eprintln!("fatal: -L invalid empty range");
                return Err(GitError::Exit(128));
            }
            start + count - 1
        }
        // `start,-N` selects the N lines ending at `start`: span [start-N+1, start].
        RangeBound::RelativeNeg(count) => {
            if *count == 0 {
                eprintln!("fatal: -L invalid empty range");
                return Err(GitError::Exit(128));
            }
            begin = start.saturating_sub(count - 1).max(1);
            start
        }
    };

    // git swaps when both endpoints are present and reversed (line-range.c).
    if begin != 0 && end != 0 && end < begin {
        std::mem::swap(&mut begin, &mut end);
    }
    // The "file has only N lines" error keys off the (lower) start endpoint,
    // BEFORE the end is defaulted to end-of-file — so `-L <past-end>` errors
    // while `-L <past-end>,<in-range>` (reversed) does not (blame.c:1211).
    if total < begin {
        let lines_word = if total == 1 { "line" } else { "lines" };
        eprintln!("fatal: file {path} has only {total} {lines_word}");
        return Err(GitError::Exit(128));
    }
    // Default/clamp the upper endpoint to end-of-file (blame.c:1217).
    let top = if end == 0 || total < end { total } else { end };
    let bottom = begin.max(1);
    Ok((bottom, top))
}

/// Print the selected lines in git blame's default format.
fn render_blame(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    lines: &[LineBlame],
    selected: &[usize],
    options: &BlameOptions,
) -> Result<()> {
    let (abbrev, hex_width) = blame_display_abbrev(git_dir, format, options)?;

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
            let (author, date) = author_and_date(db, format, blame, options)?;
            author_strings.push(author);
            date_strings.push(date);
        }
    }
    let author_width = author_strings.iter().map(String::len).max().unwrap_or(0);

    let stdout = io::stdout();
    let mut handle = stdout.lock();
    for (display_idx, &line_no) in selected.iter().enumerate() {
        let blame = &lines[line_no - 1];

        // Strip the trailing newline from the stored content; we always emit a
        // newline ourselves, matching git which prints one line per entry even
        // when the final source line lacked a newline. The line bytes are
        // written raw (not via a lossy String) so non-UTF-8 content round-trips
        // exactly.
        let content = strip_trailing_newline(&blame.content);

        if options.compat {
            // `git annotate` / `git blame -c` format:
            // `<hex>\t(<author %10s>\t<date>\t<lineno>)<content>`. No boundary
            // `^` marker (compat mode never prints it) and no space before the
            // content.
            let hex = render_compat_sha(&blame.commit, hex_width);
            let author = &author_strings[display_idx];
            let date = &date_strings[display_idx];
            write!(handle, "{hex}\t({author:>10}\t{date}\t{line_no})")?;
            handle.write_all(content)?;
            handle.write_all(b"\n")?;
            continue;
        }

        let sha = render_sha(
            &blame.commit,
            abbrev,
            blame.boundary,
            options.show_root,
            hex_width,
            options.blank_boundary,
        );

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

/// Render the object-name column for annotate-compat (`-c`) mode: exactly
/// `hex_width` hex digits, with no boundary `^` (compat output suppresses it,
/// blame.c `emit_other`).
fn render_compat_sha(commit: &ObjectId, hex_width: usize) -> String {
    commit.to_hex().chars().take(hex_width).collect()
}

/// Compute the abbreviation length for object names. git blame displays
/// `core.abbrev` (default 7) hex digits for boundary commits and one more for
/// non-boundary commits, so the column width is `abbrev + 1`.
fn blame_display_abbrev(
    git_dir: &Path,
    format: ObjectFormat,
    options: &BlameOptions,
) -> Result<(usize, usize)> {
    let hexsz = format.hex_len();
    if options.long_sha {
        // Full object name: non-boundary is the full hash (`hex_width = hexsz`);
        // the boundary form is `^` + (hexsz - 1) digits.
        return Ok((hexsz - 1, hexsz));
    }
    // Mirror git's abbrev resolution (builtin/blame.c):
    //   0 < abbrev < hexsz  -> abbrev++  (boundary `^`+abbrev-1, non-bnd abbrev)
    //   abbrev == 0         -> abbrev = hexsz
    //   abbrev >= hexsz     -> left as is, then the hex column is capped to hexsz
    // `abbrev_override` carries the user `--abbrev=<n>` (Some(0)/None => auto).
    let raw = match options.abbrev_override {
        Some(0) | None => repository_abbrev(git_dir, format)?.unwrap_or(hexsz),
        Some(n) => n,
    };
    let git_abbrev = if raw == 0 {
        hexsz
    } else if raw < hexsz {
        raw + 1
    } else {
        raw
    };
    // git prints `length = abbrev` hex digits (then `length--` for the `^` on a
    // boundary), each capped at hexsz by the final `printf`. So the non-boundary
    // column is min(git_abbrev, hexsz) and the boundary hash min(git_abbrev-1,
    // hexsz) — for a huge `--abbrev` the decrement still exceeds hexsz, leaving
    // the full hash under the boundary marker.
    let hex_width = git_abbrev.min(hexsz);
    let boundary_abbrev = git_abbrev.saturating_sub(1).min(hexsz);
    Ok((boundary_abbrev, hex_width))
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
    blank_boundary: bool,
) -> String {
    if boundary && !show_root && blank_boundary {
        // `-b`: blank out the boundary object name with spaces (full column
        // width), matching git's `memset(hex, ' ', ...)` in emit_other.
        return " ".repeat(hex_width);
    }
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
    db: &FileObjectDatabase,
    format: ObjectFormat,
    blame: &LineBlame,
    options: &BlameOptions,
) -> Result<(String, String)> {
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
        DateField::Iso => DateMode::Iso,
        DateField::Raw => DateMode::Raw,
    };
    let date = for_each_ref_identity_date(identity, &date_mode).unwrap_or_default();

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
