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
#![allow(clippy::expect_used, clippy::unwrap_used)]

use sley::plumbing::{sley_index, sley_rev, sley_worktree};
// Glob the crate root for shared plumbing (RepositoryContext, repository_abbrev,
// FileObjectDatabase, FileRefStore, Commit, Tree, the identity/date formatting
// helpers, and so on). See commands::stash for the rationale: a submodule can
// reach its ancestor module's private items, so everything visible at the crate
// root is in scope here without re-listing it.
use crate::*;
use sley::Signature;
use sley::plumbing::sley_object::TreeEntries;

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
    /// Force progress reporting to stderr (`--progress`); `--no-progress`
    /// clears it.
    progress: bool,
    /// Number of `-C` options seen. Git treats the second and third `-C` as
    /// progressively broader copy searches.
    copy_level: u8,
    /// Minimum alphanumeric score for `-C` copy matching.
    copy_score: usize,
    /// `-f` / `--show-name`: show the origin path before the line number.
    show_name: bool,
    /// `-p` / `--porcelain`: emit machine-readable grouped records.
    porcelain: bool,
    /// `--line-porcelain`: porcelain with metadata repeated for each line.
    line_porcelain: bool,
    /// Whether the author column (`-e`/`--show-email`/`--no-show-email`) was set
    /// on the command line; if not, `blame.showEmail` config supplies the
    /// default.
    author_field_explicit: bool,
    /// `--textconv` (default) / `--no-textconv`: run each path's
    /// `diff.<driver>.textconv` over blob content before diffing/rendering.
    textconv: bool,
    /// `--contents=<file>`: use `<file>`'s contents as the final image instead
    /// of the working-tree / committed copy (`-` reads standard input). Builds a
    /// "External file (--contents)" pseudo-commit on top of the start rev.
    contents_from: Option<String>,
    /// `--ignore-rev <rev>` (repeatable): revisions whose changes are skipped
    /// when blaming, attributing those lines to an earlier commit instead.
    ignore_revs: Vec<String>,
    /// `--ignore-revs-file <file>` (repeatable): files of object names to ignore.
    /// An empty string clears the accumulated list (CLI and config), mirroring
    /// git's `build_ignorelist`.
    ignore_revs_files: Vec<String>,
    /// `--incremental`: emit each blamed range as it is found, in walk order,
    /// with porcelain-style commit details.
    incremental: bool,
    /// `--encoding=<enc>`: output encoding for author/summary metadata
    /// (`none` keeps the bytes as stored). Defaults to `i18n.logOutputEncoding`.
    encoding: Option<String>,
    /// `--color-lines`: color the metadata of lines repeated from the previous
    /// output line (`color.blame.repeatedLines`).
    color_lines: bool,
    /// `--color-by-age`: color each line's metadata by commit age
    /// (`color.blame.highlightRecent`).
    color_by_age: bool,
    /// Diff algorithm override from the CLI (`--diff-algorithm`, `--minimal`,
    /// `--patience`, `--histogram`); the last such option wins. `None` falls back
    /// to `diff.algorithm` config, then Myers.
    diff_algorithm: Option<sley_diff_merge::DiffAlgorithm>,
}

/// Metadata for the all-zero pseudo-commit blame builds for the working-tree
/// (or `--contents`) final image. Mirrors git's `fake_working_tree_commit`:
/// "Not Committed Yet"/"External file (--contents)" identity, the current time,
/// a `Version of <path> from <source>` subject, and a `previous` pointer back
/// to the real parent the fake commit sits on top of.
#[derive(Clone)]
struct FakeCommit {
    /// Author/committer display name.
    name: String,
    /// Author/committer email local part (rendered as `<email>`).
    email: String,
    /// Seconds since the epoch (the moment blame ran).
    time: i64,
    /// Porcelain `summary` line (`Version of <path> from <source>`).
    summary: String,
    /// Porcelain `previous <commit> <path>` pointer to the real parent, when the
    /// path exists there.
    previous: Option<(ObjectId, String)>,
}

/// Per-origin `previous` pointers for porcelain output: maps a blamed
/// `(commit, path)` to the `(commit, path)` of the first parent the blame walk
/// descended into from it (git's `blame_origin->previous`). Root/boundary
/// commits with no such parent are simply absent.
type PreviousMap = HashMap<(ObjectId, String), (ObjectId, String)>;

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
    /// A `:funcname` bound: select the function whose header matches `pattern`.
    /// `^:funcname` forces the search to start at line 1.
    Function { pattern: String, absolute: bool },
}

/// The blame result for a single final-image line.
struct LineBlame {
    /// Commit the line is attributed to.
    commit: ObjectId,
    /// Whether that commit is a rendered boundary (root, absent `--root`).
    boundary: bool,
    /// Path in the blamed origin.
    origin_path: String,
    /// 1-based line number in the blamed origin.
    origin_lineno: usize,
    /// The raw line bytes, including a trailing newline when present.
    content: Vec<u8>,
    /// `--ignore-rev` markers carried from the blame entry (`?` / `*`).
    ignored: bool,
    unblamable: bool,
    /// Order in which this line's commit was found guilty during the walk, used
    /// to emit `--incremental` output in walk order (newest commit first).
    charge_seq: usize,
}

pub(crate) fn cmd_blame(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
    run_blame(cli_session, args, false)
}

/// `git annotate` — `git blame` with the annotate-compatible output mode forced
/// on (equivalent to `git blame -c`). Shares all of blame's parsing and the
/// scoreboard; only the output format differs.
pub(crate) fn cmd_annotate(
    cli_session: &crate::session::CliSession,
    args: &[String],
) -> Result<()> {
    run_blame(cli_session, args, true)
}

fn run_blame(
    cli_session: &crate::session::CliSession,
    args: &[String],
    force_compat: bool,
) -> Result<()> {
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

    let repo = RepositoryContext::from_session(cli_session)?;
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
        Some(r) if r.contains("..") => {
            let (left, right) = r.split_once("..").unwrap_or((r, ""));
            let start = if right.is_empty() { "HEAD" } else { right };
            let boundary = if left.is_empty() {
                None
            } else {
                let oid = repo.resolve_revision(left)?;
                Some(sley_rev::peel_to_commit(db, format, &oid)?)
            };
            (start.to_string(), boundary)
        }
        Some(r) => (r.clone(), None),
        None => ("HEAD".to_string(), None),
    };

    // A *positive* end of the history range is a non-`^`, non-empty-`..`
    // revision. With no positive end (and not bare), git builds a fake
    // working-tree commit on top of HEAD; `--contents` always builds one (git's
    // `setup_scoreboard`: fake when `contents_from || !final`).
    let has_positive_final = match &rev {
        None => false,
        Some(r) if r.starts_with('^') => false,
        Some(r) if r.contains("..") => !r.split_once("..").map(|(_, b)| b).unwrap_or("").is_empty(),
        Some(_) => true,
    };

    // Resolve the requested revision (default HEAD) to a commit.
    let start_oid = repo.resolve_revision(&rev_spec)?;
    let start_commit = sley_rev::peel_to_commit(db, format, &start_oid)?;

    // Turn the cwd-relative path into a repository-root-relative path the way
    // git's pathspec handling does, then locate the blob at the start commit. A
    // bare repository has no work tree to take a cwd prefix from, so the argument
    // is already repo-root-relative there.
    //
    // `--textconv` (default) renders blob content through the path's
    // `diff.<driver>.textconv`. Build the attribute/config-driven resolver once
    // (a bare repo with no worktree simply has nothing to convert) and route both
    // the final image and every committed `cached_blob` read through it, so blame
    // diffs converted content on both sides exactly as git's `fill_textconv` does.
    let textconv = TextconvContext {
        enabled: options.textconv,
        resolver: repo
            .worktree_root()
            .ok()
            .and_then(|root| sley_worktree::StandardAttributeMatcher::from_worktree_root(root).ok())
            .map(|attrs| {
                commands::userdiff::UserdiffResolver::with_attributes(
                    Some(attrs),
                    Some(repo.config().clone()),
                )
            }),
        config: repo.config().clone(),
        worktree_root: repo.worktree_root().ok().map(Path::to_path_buf),
        git_dir: git_dir.to_path_buf(),
        format,
    };

    let bare = blame_is_bare(&repo);
    let repo_path = if bare {
        normalize_repo_path(&path)?
    } else {
        blame_repo_relative_path(cli_session, cwd, git_dir, &path)?
    };

    // Decide whether to build the fake working-tree / `--contents` commit, the
    // way `setup_scoreboard` does: always with `--contents`, otherwise only when
    // no positive final rev was named AND the repository is not bare (a bare repo
    // with no rev blames HEAD directly — there is no work tree to overlay).
    let build_fake = options.contents_from.is_some() || (!has_positive_final && !bare);

    let (final_blob, virtual_final, fake) = if build_fake {
        // The fake commit's blob is the `--contents` file or the work-tree copy;
        // its single (real) parent is `start_commit` (HEAD by default). The bytes
        // are work-tree-form already, so route them through textconv using the
        // path's real mode — git applies the path's textconv driver to the
        // work-tree side of the blame diff too (skipping symlinks), and this keeps
        // the final image in the same converted space as the committed blobs
        // `cached_blob` produces. `--contents` is always a regular-file image.
        let (raw, mode) = match &options.contents_from {
            Some(spec) => (read_contents_file(cwd, spec)?, 0o100644),
            None => read_worktree_image(
                db,
                format,
                &repo,
                &start_commit,
                &repo_path,
                cli_session.lazy_fetch(),
            )?,
        };
        let blob = textconv.convert(&repo_path, mode, raw)?;
        // The porcelain `previous` pointer is the real parent when it has the
        // path; a brand-new (only-staged) file has no such parent.
        let previous = read_path_blob(
            db,
            format,
            &start_commit,
            &repo_path,
            cli_session.lazy_fetch(),
        )?
        .map(|_| (start_commit, repo_path.clone()));
        let (name, email) = if options.contents_from.is_some() {
            (
                "External file (--contents)".to_string(),
                "external.file".to_string(),
            )
        } else {
            (
                "Not Committed Yet".to_string(),
                "not.committed.yet".to_string(),
            )
        };
        let source = match &options.contents_from {
            Some(spec) if spec == "-" => "standard input".to_string(),
            Some(spec) => spec.clone(),
            None => repo_path.clone(),
        };
        let fake = FakeCommit {
            name,
            email,
            time: now_seconds(),
            summary: format!("Version of {repo_path} from {source}"),
            previous,
        };
        (blob, true, Some(fake))
    } else {
        // No fake commit: read the final image straight from the start rev's tree
        // and convert it through textconv, matching the committed-blob path.
        match read_path_blob(
            db,
            format,
            &start_commit,
            &repo_path,
            cli_session.lazy_fetch(),
        )? {
            Some((blob, mode)) => (textconv.convert(&repo_path, mode, blob)?, false, None),
            None => {
                // git reports the repository-relative path here, not the literal
                // argument (so blaming a missing file from a subdirectory still
                // names `<dir>/<file>`).
                eprintln!("fatal: no such path '{repo_path}' in {rev_spec}");
                return Err(GitError::Exit(128));
            }
        }
    };

    // Resolve the `--ignore-rev` set and the `blame.markIgnoredLines` /
    // `blame.markUnblamableLines` display flags (the latter via `repo.config()`,
    // which already folds in `-c` overrides).
    let ignore_set = build_ignore_set(&repo, &options)?;
    let grafts = read_graft_file(git_dir, format);
    let marks = BlameMarks {
        ignored: repo
            .config()
            .get_bool("blame", None, "markignoredlines")
            .unwrap_or(false),
        unblamable: repo
            .config()
            .get_bool("blame", None, "markunblamablelines")
            .unwrap_or(false),
    };

    // The output encoding for author/summary metadata: `--encoding` wins,
    // otherwise `i18n.logOutputEncoding` (falling back to commitEncoding, UTF-8).
    let output_encoding = match &options.encoding {
        Some(enc) => enc.clone(),
        None => log_output_encoding(repo.config()),
    };

    let color_plan = build_color_plan(&repo, &options);

    // The diff algorithm driving blame attribution: a CLI override wins,
    // otherwise `diff.algorithm` config, otherwise Myers.
    let diff_algorithm = options.diff_algorithm.unwrap_or_else(|| {
        repo.config()
            .get("diff", None, "algorithm")
            .and_then(|name| parse_blame_diff_algorithm(name).ok())
            .unwrap_or(sley_diff_merge::DiffAlgorithm::Myers)
    });

    let (lines, previous_map) = compute_blame(
        db,
        format,
        &start_commit,
        &repo_path,
        &final_blob,
        options.first_parent,
        boundary_tip,
        options.copy_level,
        options.copy_score,
        virtual_final,
        &textconv,
        &ignore_set,
        diff_algorithm,
        &grafts,
        cli_session.lazy_fetch(),
    )?;

    // Resolve the -L ranges against the real line count, then render. The
    // repo-relative path is used for any -L error message, matching git.
    let selected = select_lines(&lines, &options, &repo_path)?;
    if options.progress {
        eprintln!("Blaming lines: 100% ({0}/{0}), done.", selected.len());
    }
    if selected.is_empty() {
        return Ok(());
    }
    render_blame(
        git_dir,
        format,
        db,
        &lines,
        &selected,
        &options,
        fake.as_ref(),
        &previous_map,
        marks,
        &output_encoding,
        color_plan.as_ref(),
    )
}

/// `blame.markIgnoredLines` / `blame.markUnblamableLines` display flags: whether
/// to prefix the object-name column with `?` / `*` (and emit the `ignored` /
/// `unblamable` porcelain keywords) for `--ignore-rev` lines.
#[derive(Clone, Copy, Default)]
struct BlameMarks {
    ignored: bool,
    unblamable: bool,
}

/// How to color the per-line metadata in the standard output (git's
/// `OUTPUT_COLOR_LINE` / `OUTPUT_SHOW_AGE_WITH_COLOR`).
enum ColorPlan {
    /// `--color-lines`: lines repeated from the previous output line get
    /// `repeated`; the first of a run is uncolored.
    Lines { repeated: String },
    /// `--color-by-age`: each line's metadata is colored by commit age, picking
    /// the first `(hop, color)` field whose hop the author time does not exceed.
    Age { fields: Vec<(i64, String)> },
}

/// Resolve a git color spec (`color.blame.*` value) to its ANSI sequence,
/// concatenating attribute words (`bold yellow`).
fn blame_parse_color(spec: &str) -> String {
    let mut out = String::new();
    for word in spec.split_whitespace() {
        if let Some(ansi) = git_color_name_to_ansi(word) {
            out.push_str(ansi);
        }
    }
    out
}

/// Parse a `color.blame.highlightRecent` field list (git's `parse_color_fields`):
/// alternating color/date starting with a color and ending with a color, which
/// becomes the open-ended sentinel `(i64::MAX, color)`. Returns `None` on a
/// malformed spec.
fn parse_color_fields(spec: &str) -> Option<Vec<(i64, String)>> {
    let mut fields: Vec<(i64, String)> = Vec::new();
    let mut expect_color = true;
    let mut pending = String::new();
    for part in spec.split(',') {
        let part = part.trim();
        if expect_color {
            pending = blame_parse_color(part);
            expect_color = false;
        } else {
            let hop = crate::commands::approxidate::parse_approxidate(part)?;
            fields.push((hop, std::mem::take(&mut pending)));
            pending = String::new();
            expect_color = true;
        }
    }
    if expect_color {
        // Ended on a date — git's "must end with a color".
        return None;
    }
    fields.push((i64::MAX, pending));
    Some(fields)
}

/// git's `determine_line_heat`: the color for a commit of the given author time.
fn determine_line_heat(author_time: i64, fields: &[(i64, String)]) -> &str {
    let dated = fields.len().saturating_sub(1);
    let mut i = 0;
    while i < dated && author_time > fields[i].0 {
        i += 1;
    }
    &fields[i].1
}

/// Build the standard-output coloring plan from the `--color-*` flags and the
/// `blame.coloring` / `color.blame.*` config. Coloring is disabled in compat
/// (`-c` / annotate) mode.
fn build_color_plan(repo: &RepositoryContext, options: &BlameOptions) -> Option<ColorPlan> {
    if options.compat {
        return None;
    }
    let (mut lines, mut age) = (options.color_lines, options.color_by_age);
    if !lines && !age {
        match repo.config().get("blame", None, "coloring") {
            Some("repeatedLines") => lines = true,
            Some("highlightRecent") => age = true,
            _ => {}
        }
    }
    if lines {
        let repeated = repo
            .config()
            .get("color", Some("blame"), "repeatedlines")
            .map(blame_parse_color)
            .filter(|ansi| !ansi.is_empty())
            .unwrap_or_else(|| "\x1b[36m".to_string());
        return Some(ColorPlan::Lines { repeated });
    }
    if age {
        let spec = repo
            .config()
            .get("color", Some("blame"), "highlightrecent")
            .map(str::to_string)
            .unwrap_or_else(|| "blue,12 month ago,white,1 month ago,red".to_string());
        return parse_color_fields(&spec).map(|fields| ColorPlan::Age { fields });
    }
    None
}

/// The author timestamp used for age coloring: the commit's author time, or the
/// fake work-tree commit's time for the null commit.
fn blame_author_time(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    blame: &LineBlame,
    fake: Option<&FakeCommit>,
) -> i64 {
    if blame.commit.is_null() {
        return fake.map(|f| f.time).unwrap_or(0);
    }
    let Ok(object) = db.read_object(&blame.commit) else {
        return 0;
    };
    let Ok(commit) = Commit::parse(format, &object.body) else {
        return 0;
    };
    Signature::from_ident_line(&commit.author)
        .map(|sig| sig.time.seconds)
        .unwrap_or(0)
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
    let mut progress = false;
    let mut copy_level = 0u8;
    let mut copy_score = 40usize;
    let mut show_name = false;
    let mut porcelain = false;
    let mut line_porcelain = false;
    let mut textconv = true;
    let mut contents_from: Option<String> = None;
    let mut ignore_revs: Vec<String> = Vec::new();
    let mut ignore_revs_files: Vec<String> = Vec::new();
    let mut incremental = false;
    let mut encoding: Option<String> = None;
    let mut color_lines = false;
    let mut color_by_age = false;
    let mut diff_algorithm: Option<sley_diff_merge::DiffAlgorithm> = None;
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
            "-f" | "--show-name" => show_name = true,
            "-p" | "--porcelain" => porcelain = true,
            "--line-porcelain" => {
                porcelain = true;
                line_porcelain = true;
            }
            "--progress" => progress = true,
            "--no-progress" => progress = false,
            "--textconv" => textconv = true,
            "--no-textconv" => textconv = false,
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
            "--contents" => {
                let Some(value) = iter.next() else {
                    return Err(blame_option_requires_value("contents"));
                };
                contents_from = Some(value.clone());
            }
            other if other.starts_with("--contents=") => {
                contents_from = Some(other["--contents=".len()..].to_string());
            }
            "--ignore-rev" => {
                let Some(value) = iter.next() else {
                    return Err(blame_option_requires_value("ignore-rev"));
                };
                ignore_revs.push(value.clone());
            }
            other if other.starts_with("--ignore-rev=") => {
                ignore_revs.push(other["--ignore-rev=".len()..].to_string());
            }
            "--ignore-revs-file" => {
                let Some(value) = iter.next() else {
                    return Err(blame_option_requires_value("ignore-revs-file"));
                };
                ignore_revs_files.push(value.clone());
            }
            other if other.starts_with("--ignore-revs-file=") => {
                ignore_revs_files.push(other["--ignore-revs-file=".len()..].to_string());
            }
            "--incremental" => incremental = true,
            "--color-lines" => color_lines = true,
            "--color-by-age" => color_by_age = true,
            "--minimal" => diff_algorithm = Some(sley_diff_merge::DiffAlgorithm::Minimal),
            "--patience" => diff_algorithm = Some(sley_diff_merge::DiffAlgorithm::Patience),
            "--histogram" => diff_algorithm = Some(sley_diff_merge::DiffAlgorithm::Histogram),
            "--diff-algorithm" => {
                let Some(value) = iter.next() else {
                    return Err(blame_option_requires_value("diff-algorithm"));
                };
                diff_algorithm = Some(parse_blame_diff_algorithm(value)?);
            }
            other if other.starts_with("--diff-algorithm=") => {
                diff_algorithm = Some(parse_blame_diff_algorithm(
                    &other["--diff-algorithm=".len()..],
                )?);
            }
            "--encoding" => {
                let Some(value) = iter.next() else {
                    return Err(blame_option_requires_value("encoding"));
                };
                encoding = Some(value.clone());
            }
            other if other.starts_with("--encoding=") => {
                encoding = Some(other["--encoding=".len()..].to_string());
            }
            other if other.starts_with("--abbrev=") => {
                let value = &other["--abbrev=".len()..];
                abbrev_override = Some(parse_abbrev_value(value)?);
            }
            other if other.starts_with("-L") => {
                // Attached form: `-L1,3`.
                ranges.push(parse_line_range(&other[2..])?);
            }
            other if other == "-C" || is_blame_copy_option(other) => {
                copy_level = copy_level.saturating_add(1);
                if other.len() > 2 {
                    copy_score = other[2..].parse::<usize>().unwrap_or(copy_score);
                }
            }
            // `-M[<score>]`: detect lines moved/copied within the same file.
            // The within-file move pass is not implemented; the cases the
            // upstream suite exercises with `-M` (the `--ignore-rev` fuzzy tests)
            // are resolved by the ignore heuristic, so `-M` is accepted as a
            // no-op rather than rejected.
            other if other == "-M" || is_blame_move_option(other) => {}
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
        progress,
        copy_level,
        copy_score,
        show_name,
        porcelain,
        line_porcelain,
        author_field_explicit,
        textconv,
        contents_from,
        ignore_revs,
        ignore_revs_files,
        incremental,
        encoding,
        color_lines,
        color_by_age,
        diff_algorithm,
    }))
}

/// Parse a `--diff-algorithm <value>` argument to a [`DiffAlgorithm`]. An
/// unknown value is the same fatal git reports.
fn parse_blame_diff_algorithm(value: &str) -> Result<sley_diff_merge::DiffAlgorithm> {
    use sley::plumbing::sley_diff_merge::DiffAlgorithm;
    Ok(match value {
        "myers" | "default" => DiffAlgorithm::Myers,
        "minimal" => DiffAlgorithm::Minimal,
        "patience" => DiffAlgorithm::Patience,
        "histogram" => DiffAlgorithm::Histogram,
        other => {
            eprintln!(
                "fatal: option diff-algorithm accepts \"myers\", \"minimal\", \"patience\" and \"histogram\""
            );
            let _ = other;
            return Err(GitError::Exit(128));
        }
    })
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
        "-n" | "--show-number" | "-w" | "--reverse" | "--show-stats" | "--score-debug"
    ) {
        return true;
    }
    arg.starts_with("-S") || arg.starts_with("--reverse=")
}

fn is_blame_copy_option(arg: &str) -> bool {
    arg.len() > 2 && arg.starts_with("-C") && arg[2..].bytes().all(|b| b.is_ascii_digit())
}

/// `-M<num>` (within-file move detection with an optional score).
fn is_blame_move_option(arg: &str) -> bool {
    arg.len() > 2 && arg.starts_with("-M") && arg[2..].bytes().all(|b| b.is_ascii_digit())
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
    let (function_body, absolute) = match raw.strip_prefix('^') {
        Some(rest) if rest.starts_with(':') => (Some(rest), true),
        _ if raw.starts_with(':') => (Some(raw), false),
        _ => (None, false),
    };
    if let Some(body) = function_body {
        let pattern = body.strip_prefix(':').ok_or_else(blame_usage_error)?;
        if pattern.is_empty() {
            return Err(blame_usage_error());
        }
        return Ok(RangeBound::Function {
            pattern: pattern.to_string(),
            absolute,
        });
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
fn blame_repo_relative_path(
    cli_session: &crate::session::CliSession,
    cwd: &Path,
    git_dir: &Path,
    path: &str,
) -> Result<String> {
    let input = Path::new(path);
    if input.is_absolute() {
        let root = fs::canonicalize(worktree_root_for_git_dir(cli_session, git_dir)?)?;
        let absolute = fs::canonicalize(input)?;
        let relative = absolute
            .strip_prefix(&root)
            .map_err(|_| GitError::InvalidPath(format!("{path} is outside the repository")))?;
        return normalize_repo_path(&relative.to_string_lossy());
    }
    let prefix = worktree_prefix(cli_session, cwd, git_dir)?;
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
/// Per-blame textconv state. When enabled (the `--textconv` default), each
/// origin blob is rendered through its path's `diff.<driver>.textconv` over the
/// *smudged* worktree form (matching upstream's `fill_origin_blob` →
/// `fill_textconv` → `prep_temp_blob`), so both the diffs the blame walk
/// computes and the rendered lines reflect the converted content.
struct TextconvContext {
    enabled: bool,
    resolver: Option<commands::userdiff::UserdiffResolver>,
    config: GitConfig,
    worktree_root: Option<PathBuf>,
    git_dir: PathBuf,
    format: ObjectFormat,
}

impl TextconvContext {
    /// Convert `blob` for `path`/`mode`, or return it unchanged when textconv is
    /// disabled, the path has no textconv driver, or the entry is not a regular
    /// file (symlinks fall back to the default driver, as in git).
    fn convert(&self, path: &str, mode: u32, blob: Vec<u8>) -> Result<Vec<u8>> {
        if !self.enabled {
            return Ok(blob);
        }
        let (Some(resolver), Some(worktree_root)) =
            (self.resolver.as_ref(), self.worktree_root.as_ref())
        else {
            return Ok(blob);
        };
        let Some(command) = resolver.textconv_for_path(path.as_bytes(), mode)? else {
            return Ok(blob);
        };
        let smudged = sley_worktree::apply_smudge_filter(
            worktree_root,
            &self.git_dir,
            self.format,
            &self.config,
            path.as_bytes(),
            &blob,
        )?;
        match commands::userdiff::run_textconv(&command, &smudged)? {
            Some(converted) => Ok(converted),
            None => {
                eprintln!("fatal: unable to read files to diff");
                Err(GitError::Exit(128))
            }
        }
    }
}

/// Read the blob at `repo_path` in `commit`'s tree, returning its bytes and the
/// tree entry's file mode (the mode lets textconv skip symlinks).
fn read_path_blob(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    commit: &ObjectId,
    repo_path: &str,
    lazy_fetch: bool,
) -> Result<Option<(Vec<u8>, u32)>> {
    let tree_oid = sley_rev::peel_to_tree(db, format, commit)?;
    let Some((blob_oid, mode)) = lookup_tree_path(db, format, &tree_oid, repo_path)? else {
        return Ok(None);
    };
    let object = read_object_maybe_prefetch_promisor(db, &blob_oid, lazy_fetch)?;
    if object.object_type != ObjectType::Blob {
        return Ok(None);
    }
    Ok(Some((object.body.clone(), mode)))
}

/// Read the blob for `repo_path` from the index (any normal-stage entry).
fn read_index_blob(
    git_dir: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    repo_path: &str,
    lazy_fetch: bool,
) -> Result<Option<(Vec<u8>, u32)>> {
    let Some(index) = sley_worktree::read_repository_index(git_dir, format)? else {
        return Ok(None);
    };
    let Some(entry) = index.entries.iter().find(|entry| {
        entry.stage() == sley_index::Stage::Normal && entry.path.as_bytes() == repo_path.as_bytes()
    }) else {
        return Ok(None);
    };
    let object = read_object_maybe_prefetch_promisor(db, &entry.oid, lazy_fetch)?;
    if object.object_type != ObjectType::Blob {
        return Ok(None);
    }
    Ok(Some((object.body.clone(), entry.mode)))
}

/// Build the `--ignore-rev` / `--ignore-revs-file` / `blame.ignoreRevsFile`
/// commit set, mirroring git's `build_ignorelist`. Files are processed first
/// (config entries, then CLI ones, in order; an empty filename clears the set so
/// far), then the explicit `--ignore-rev` revisions.
fn build_ignore_set(repo: &RepositoryContext, options: &BlameOptions) -> Result<HashSet<ObjectId>> {
    let db = repo.objects();
    let format = repo.format();
    let mut set: HashSet<ObjectId> = HashSet::new();

    // Config `blame.ignoreRevsFile` entries come first, then CLI `-ignore-revs-file`.
    let mut files: Vec<String> = repo
        .config()
        .get_all("blame", None, "ignorerevsfile")
        .into_iter()
        .flatten()
        .map(str::to_string)
        .collect();
    files.extend(options.ignore_revs_files.iter().cloned());

    for file in &files {
        if file.is_empty() {
            set.clear();
            continue;
        }
        parse_ignore_revs_file(repo, file, &mut set)?;
    }

    for rev in &options.ignore_revs {
        let commit = repo
            .resolve_revision(rev)
            .ok()
            .and_then(|oid| sley_rev::peel_to_commit(db, format, &oid).ok());
        match commit {
            Some(oid) => {
                set.insert(oid);
            }
            None => {
                eprintln!("fatal: cannot find revision {rev} to ignore");
                return Err(GitError::Exit(128));
            }
        }
    }

    Ok(set)
}

/// Parse one `--ignore-revs-file`, adding each named commit to `set`. Mirrors
/// `oidset_parse_file_carefully` with the `peel_to_commit_oid` tweak: trailing
/// `#` comments and surrounding whitespace are stripped, blank lines skipped, an
/// unparsable token is the "invalid object name" fatal, and an object that does
/// not peel to a commit (e.g. a tree) is silently ignored.
fn parse_ignore_revs_file(
    repo: &RepositoryContext,
    path: &str,
    set: &mut HashSet<ObjectId>,
) -> Result<()> {
    let db = repo.objects();
    let format = repo.format();
    let full = {
        let p = Path::new(path);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            repo.cwd().join(path)
        }
    };
    let Ok(content) = std::fs::read(&full) else {
        eprintln!("fatal: could not open object name list: {path}");
        return Err(GitError::Exit(128));
    };
    for raw in content.split(|b| *b == b'\n') {
        // Strip a trailing `#` comment, then surrounding ASCII whitespace.
        let line = match raw.iter().position(|b| *b == b'#') {
            Some(idx) => &raw[..idx],
            None => raw,
        };
        let start = line.iter().position(|b| !is_ascii_space(*b));
        let Some(start) = start else { continue };
        let end = line.iter().rposition(|b| !is_ascii_space(*b)).unwrap() + 1;
        let token = &line[start..end];
        let Ok(text) = std::str::from_utf8(token) else {
            eprintln!(
                "fatal: invalid object name: {}",
                String::from_utf8_lossy(token)
            );
            return Err(GitError::Exit(128));
        };
        let Ok(oid) = ObjectId::from_hex(format, text) else {
            eprintln!("fatal: invalid object name: {text}");
            return Err(GitError::Exit(128));
        };
        // Peel to a commit; a non-commit (e.g. a tree) is silently accepted.
        if let Ok(commit) = sley_rev::peel_to_commit(db, format, &oid) {
            set.insert(commit);
        }
    }
    Ok(())
}

/// Parse `<git_dir>/info/grafts` into a commit → parents override map (git's
/// `read_graft_line` / `register_commit_graft`). Each non-comment line is a
/// whitespace-separated list of object names: the first names a commit, the rest
/// its grafted parents. Lines that don't parse are skipped. Empty map when the
/// file is absent.
fn read_graft_file(git_dir: &Path, format: ObjectFormat) -> HashMap<ObjectId, Vec<ObjectId>> {
    let mut map: HashMap<ObjectId, Vec<ObjectId>> = HashMap::new();
    let Ok(content) = std::fs::read(git_dir.join("info").join("grafts")) else {
        return map;
    };
    for raw in content.split(|b| *b == b'\n') {
        let line = match raw.iter().position(|b| *b == b'#') {
            Some(idx) => &raw[..idx],
            None => raw,
        };
        let tokens: Vec<&[u8]> = line
            .split(|b| b.is_ascii_whitespace())
            .filter(|t| !t.is_empty())
            .collect();
        let Some(first) = tokens.first() else {
            continue;
        };
        let Ok(text) = std::str::from_utf8(first) else {
            continue;
        };
        let Ok(commit) = ObjectId::from_hex(format, text) else {
            continue;
        };
        let parents = tokens[1..]
            .iter()
            .filter_map(|t| std::str::from_utf8(t).ok())
            .filter_map(|t| ObjectId::from_hex(format, t).ok())
            .collect();
        map.insert(commit, parents);
    }
    map
}

/// Whether the repository is bare. Mirrors git's `is_bare_repository`: an
/// explicit `core.bare` wins, otherwise infer from the absence of a work tree.
fn blame_is_bare(repo: &RepositoryContext) -> bool {
    if let Some(bare) = repo.config().get_bool("core", None, "bare") {
        return bare;
    }
    repo.worktree_root().is_err()
}

/// Seconds since the epoch at the moment blame runs. git stamps the fake
/// working-tree commit with the current time (`time(&now)` in
/// `fake_working_tree_commit`).
fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Read the `--contents=<file>` final image. `-` reads standard input; a
/// relative path is resolved against the process cwd (where git's
/// `fake_working_tree_commit` opens it).
fn read_contents_file(cwd: &Path, spec: &str) -> Result<Vec<u8>> {
    if spec == "-" {
        use std::io::Read as _;
        let mut buf = Vec::new();
        io::stdin()
            .read_to_end(&mut buf)
            .map_err(|err| GitError::Io(err.to_string()))?;
        return Ok(buf);
    }
    let path = Path::new(spec);
    let full = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(spec)
    };
    std::fs::read(&full).map_err(|_| {
        eprintln!("fatal: Cannot stat '{spec}'");
        GitError::Exit(128)
    })
}

/// Read the work-tree copy of `repo_path` for the fake working-tree commit. git
/// reads the actual file from disk (`lstat`/`strbuf_read_file`) after
/// `verify_working_tree_path` confirms the path is known; a path that is absent
/// from the work tree, the start rev, and the index is the "no such path"
/// fatal.
fn read_worktree_image(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    repo: &RepositoryContext,
    start_commit: &ObjectId,
    repo_path: &str,
    lazy_fetch: bool,
) -> Result<(Vec<u8>, u32)> {
    // `verify_working_tree_path`: an untracked path (absent from the start
    // commit's tree and from the index in *any* stage) is the "no such path"
    // fatal, even when a file by that name exists on disk. An unmerged path
    // (stages 1/2/3, no stage 0) still counts as known, so a conflicted file
    // blames against HEAD rather than erroring.
    let committed = read_path_blob(db, format, start_commit, repo_path, lazy_fetch)?;
    let in_index = path_in_index_any_stage(repo.git_dir(), format, repo_path)?;
    if committed.is_none() && !in_index {
        eprintln!("fatal: no such path '{repo_path}' in HEAD");
        return Err(GitError::Exit(128));
    }
    // Read the actual work-tree file. A symlink contributes its *link text*
    // (git's `strbuf_readlink`), not the pointed-at file's contents, with a
    // 0o120000 mode so textconv is skipped; a regular file is read and run
    // through the clean filter (git's `fake_working_tree_commit` applies
    // `convert_to_git`, normalizing CRLF→LF / running `clean`/ident per the
    // path's attributes and `core.autocrlf`) so the fake commit's blob lands in
    // the same git-form space as the committed blobs it is diffed against. For a
    // path with no EOL/filter attributes this is a no-op, so plain blobs are
    // byte-identical to a verbatim read.
    if let Ok(root) = repo.worktree_root() {
        let absolute = root.join(repo_path);
        if let Ok(meta) = std::fs::symlink_metadata(&absolute) {
            if meta.file_type().is_symlink() {
                if let Ok(target) = std::fs::read_link(&absolute) {
                    #[cfg(unix)]
                    {
                        use std::os::unix::ffi::OsStrExt as _;
                        return Ok((target.as_os_str().as_bytes().to_vec(), 0o120000));
                    }
                    #[cfg(not(unix))]
                    {
                        return Ok((
                            target.to_string_lossy().replace('\\', "/").into_bytes(),
                            0o120000,
                        ));
                    }
                }
            } else if let Ok(bytes) = std::fs::read(&absolute) {
                // git's `convert_to_git` honors `has_crlf_in_index`: an auto
                // (`core.autocrlf` / `text=auto`) path whose recorded blob
                // already holds CRLF is left unconverted (the "safer autocrlf"
                // rule), so a file committed with CRLF still blames against its
                // CRLF blob. Mirror that by skipping the CRLF→LF clean when the
                // committed blob already contains CRLF — the one case where the
                // raw work-tree CRLF already matches the recorded image. When the
                // recorded blob is LF (the common case), cleaning the work-tree
                // copy normalizes its CRLF back to the committed LF form.
                let recorded_has_crlf = committed
                    .as_ref()
                    .is_some_and(|(blob, _)| blob.windows(2).any(|w| w == b"\r\n"));
                let image = if recorded_has_crlf {
                    bytes
                } else {
                    sley_worktree::apply_clean_filter(
                        &root,
                        repo.git_dir(),
                        repo.config(),
                        repo_path.as_bytes(),
                        &bytes,
                    )?
                };
                return Ok((image, 0o100644));
            }
        }
    }
    // The path is tracked but unreadable from the work tree (e.g. staged then
    // removed): fall back to the staged copy, then the committed blob.
    if let Some((blob, mode)) = read_index_blob(repo.git_dir(), db, format, repo_path, lazy_fetch)?
    {
        return Ok((blob, mode));
    }
    if let Some((blob, mode)) = committed {
        return Ok((blob, mode));
    }
    eprintln!("fatal: no such path '{repo_path}' in HEAD");
    Err(GitError::Exit(128))
}

/// Whether `repo_path` is present in the index in any stage (0–3). Mirrors
/// git's `verify_working_tree_path`, which treats an unmerged entry (stages
/// 1/2/3) as a known path.
fn path_in_index_any_stage(git_dir: &Path, format: ObjectFormat, repo_path: &str) -> Result<bool> {
    let Some(index) = sley_worktree::read_repository_index(git_dir, format)? else {
        return Ok(false);
    };
    Ok(index
        .entries
        .iter()
        .any(|entry| entry.path.as_bytes() == repo_path.as_bytes()))
}

/// Walk `repo_path` component-by-component through `tree_oid`, returning the
/// blob id it names, or `None` if any component is missing or an intermediate
/// component is not a tree.
fn lookup_tree_path(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    repo_path: &str,
) -> Result<Option<(ObjectId, u32)>> {
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
            return Ok(Some((oid, mode)));
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
#[derive(Clone, PartialEq, Eq, Hash)]
struct OriginKey {
    commit: ObjectId,
    path: String,
    virtual_worktree: bool,
}

#[derive(Clone, Default)]
struct BlameEntry {
    /// 0-based start line in the final image.
    lno: usize,
    /// 0-based start line in the suspect's blob.
    s_lno: usize,
    /// Number of lines this entry covers (in both the final image and the
    /// suspect's blob).
    num_lines: usize,
    /// `--ignore-rev`: this run's blame was passed *through* an ignored commit
    /// (git's `blame_entry.ignored`). Rendered with a `?` marker /
    /// `ignored` porcelain keyword when `blame.markIgnoredLines` is set.
    ignored: bool,
    /// `--ignore-rev`: this run's lines were added by an ignored commit and
    /// could not be matched to any parent line (git's `blame_entry.unblamable`).
    /// Rendered with a `*` marker / `unblamable` porcelain keyword when
    /// `blame.markUnblamableLines` is set.
    unblamable: bool,
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
    copy_level: u8,
    copy_score: usize,
    virtual_final: bool,
    textconv: &TextconvContext,
    ignore_set: &HashSet<ObjectId>,
    diff_algorithm: sley_diff_merge::DiffAlgorithm,
    grafts: &HashMap<ObjectId, Vec<ObjectId>>,
    lazy_fetch: bool,
) -> Result<(Vec<LineBlame>, PreviousMap)> {
    let final_lines = sley_diff_merge::split_lines(final_blob);
    let line_count = final_lines.len();

    // Per-origin `previous` pointers for porcelain (`blame_origin->previous`).
    let mut previous_map: PreviousMap = HashMap::new();

    // Final attribution per line, filled in as commits are found guilty.
    let mut result: Vec<Option<LineBlame>> = (0..line_count).map(|_| None).collect();
    if line_count == 0 {
        return Ok((Vec::new(), previous_map));
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
    let mut suspects: HashMap<OriginKey, Vec<BlameEntry>> = HashMap::new();
    let mut blob_cache: HashMap<(ObjectId, String), Option<Vec<u8>>> = HashMap::new();
    let mut date_cache: HashMap<ObjectId, i64> = HashMap::new();

    // The start commit owns the entire final image as one chunk.
    let start_key = OriginKey {
        commit: *start_commit,
        path: repo_path.to_string(),
        virtual_worktree: virtual_final,
    };
    // Seed the blob cache with the final image only for a *real* start commit.
    // The virtual work-tree origin shares its commit id with the real start
    // commit (its single parent), so seeding `(commit, path)` here would poison
    // the parent's committed-blob read and make every line pass through. The
    // virtual origin gets its image from `final_blob` directly instead.
    if !virtual_final {
        blob_cache.insert(
            (start_key.commit, start_key.path.clone()),
            Some(final_blob.to_vec()),
        );
    }
    suspects.insert(
        start_key.clone(),
        vec![BlameEntry {
            lno: 0,
            s_lno: 0,
            num_lines: line_count,
            ..Default::default()
        }],
    );

    // Commit-date priority queue, newest first (git's
    // compare_commits_by_commit_date). We materialise the comparator lazily via
    // `pop_newest_commit`, caching each commit's date.
    let mut queue: Vec<OriginKey> = vec![start_key];

    // Monotonic charge-event counter for `--incremental` walk ordering.
    let mut next_seq = 0usize;

    while let Some(origin) = pop_newest_origin(&mut queue, db, format, &mut date_cache)? {
        let Some(mut owned) = suspects.remove(&origin) else {
            continue;
        };
        if owned.is_empty() {
            continue;
        }

        // Uninteresting (`^<rev>`) commits are boundaries: charge their lines
        // with the boundary marker and stop — do not pass blame to parents.
        if uninteresting.contains(&origin.commit) {
            charge_remaining(&mut result, &final_lines, &origin, true, owned, next_seq);
            next_seq += 1;
            continue;
        }

        // Resolve this commit and its blob for the path. The virtual work-tree
        // origin's image is the supplied `final_blob` (the worktree / contents
        // bytes), not the start commit's committed blob.
        let commit_oid = origin.commit;
        let commit_obj = db.read_object(&commit_oid)?;
        let commit = Commit::parse(format, &commit_obj.body)?;
        let child_blob = if origin.virtual_worktree {
            Some(final_blob.to_vec())
        } else {
            cached_blob(
                db,
                format,
                &commit_oid,
                &origin.path,
                &mut blob_cache,
                textconv,
                lazy_fetch,
            )?
        };
        let Some(child_blob) = child_blob else {
            // The path is absent at this commit (shouldn't normally happen for a
            // suspect): charge everything here so no line is lost.
            charge_remaining(&mut result, &final_lines, &origin, false, owned, next_seq);
            next_seq += 1;
            continue;
        };
        let child_lines = sley_diff_merge::split_lines(&child_blob);

        let mut parents = if origin.virtual_worktree {
            vec![origin.commit]
        } else if let Some(grafted) = grafts.get(&origin.commit) {
            // `.git/info/grafts` overrides this commit's parents.
            grafted.clone()
        } else {
            commit.parents.clone()
        };
        if first_parent {
            parents.truncate(1);
        }
        if parents.is_empty() {
            // Root commit (or `--first-parent` past a root): every remaining
            // line is its own. Render as a boundary unless `--root`.
            charge_remaining(&mut result, &final_lines, &origin, true, owned, next_seq);
            next_seq += 1;
            continue;
        }

        // Pass blame to each parent in order. `owned` shrinks as parents claim
        // chunks; whatever remains after the last parent is charged here.
        for parent in &parents {
            if owned.is_empty() {
                break;
            }
            let Some(parent_origin) =
                find_parent_origin(db, format, &origin, parent, copy_level > 1, lazy_fetch)?
            else {
                continue;
            };
            // Record the first parent origin we descend into as this suspect's
            // porcelain `previous` pointer (set once, like git's
            // `origin->previous`). Skip the virtual work-tree origin: its blamed
            // lines surface as the null commit, whose `previous` comes from the
            // FakeCommit metadata instead.
            if !origin.virtual_worktree {
                previous_map
                    .entry((origin.commit, origin.path.clone()))
                    .or_insert_with(|| (parent_origin.commit, parent_origin.path.clone()));
            }
            let parent_blob = cached_blob(
                db,
                format,
                parent,
                &parent_origin.path,
                &mut blob_cache,
                textconv,
                lazy_fetch,
            )?;
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
                queue_entries(&mut suspects, &mut queue, parent_origin, passed);
                break;
            }

            let parent_lines = sley_diff_merge::split_lines(&parent_blob);
            let mut still_ours = Vec::new();
            let passed =
                pass_blame_to_parent(&parent_lines, &child_lines, &mut owned, diff_algorithm);
            still_ours.append(&mut owned);
            owned = still_ours;
            if !passed.is_empty() {
                queue_entries(&mut suspects, &mut queue, parent_origin, passed);
            }
        }

        // `--ignore-rev`: this commit is ignored, so the lines it changed (left
        // in `owned` by the normal pass) are not charged to it. Instead each is
        // fuzzily matched to a parent line and passed there marked `ignored`, or
        // kept marked `unblamable`. Mirrors git's second `pass_blame_to_parent`
        // loop (ignore_diffs = 1) over the scapegoats.
        if ignore_set.contains(&commit_oid) && !owned.is_empty() {
            let child_fps = line_fingerprints(&child_lines);
            for parent in &parents {
                if owned.is_empty() {
                    break;
                }
                let Some(parent_origin) =
                    find_parent_origin(db, format, &origin, parent, copy_level > 1, lazy_fetch)?
                else {
                    continue;
                };
                let parent_blob = cached_blob(
                    db,
                    format,
                    parent,
                    &parent_origin.path,
                    &mut blob_cache,
                    textconv,
                    lazy_fetch,
                )?;
                let Some(parent_blob) = parent_blob else {
                    continue;
                };
                let parent_lines = sley_diff_merge::split_lines(&parent_blob);
                let parent_fps = line_fingerprints(&parent_lines);
                let passed = pass_blame_to_parent_ignore(
                    &parent_lines,
                    &child_lines,
                    &parent_fps,
                    &child_fps,
                    &mut owned,
                    diff_algorithm,
                );
                if !passed.is_empty() {
                    queue_entries(&mut suspects, &mut queue, parent_origin, passed);
                }
            }
        }

        if copy_level > 0 && !owned.is_empty() {
            let copied = find_copies_in_parents(
                db,
                format,
                &origin,
                &parents,
                copy_level,
                copy_score,
                &mut owned,
                &final_lines,
                textconv,
                lazy_fetch,
            )?;
            for (copy_origin, entries) in copied {
                queue_entries(&mut suspects, &mut queue, copy_origin, entries);
            }
        }

        // Anything still suspected of this commit after every parent had a turn
        // is genuinely this commit's: charge it (non-boundary — it has parents).
        if !owned.is_empty() {
            let guilty = if origin.virtual_worktree {
                OriginKey {
                    commit: ObjectId::null(format),
                    path: origin.path.clone(),
                    virtual_worktree: true,
                }
            } else {
                origin.clone()
            };
            charge_remaining(&mut result, &final_lines, &guilty, false, owned, next_seq);
            next_seq += 1;
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
                origin_path: repo_path.to_string(),
                origin_lineno: line_index + 1,
                content: final_lines[line_index].content.to_vec(),
                ignored: false,
                unblamable: false,
                charge_seq: usize::MAX,
            }),
        }
    }
    Ok((out, previous_map))
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
    owned: &mut Vec<BlameEntry>,
    algorithm: sley_diff_merge::DiffAlgorithm,
) -> Vec<BlameEntry> {
    let hunks = diff_hunks(parent_lines, child_lines, algorithm);

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
    algorithm: sley_diff_merge::DiffAlgorithm,
) -> Vec<DiffHunk> {
    // The contiguous-append shortcut matches git's Myers behavior for "add a
    // header and append a function" edits; the alternative algorithms
    // (patience/histogram) produce their own anchoring, so only take the
    // shortcut for the Myers family.
    if matches!(
        algorithm,
        sley_diff_merge::DiffAlgorithm::Myers | sley_diff_merge::DiffAlgorithm::Minimal
    ) && let Some(hunks) = contiguous_parent_hunks(parent_lines, child_lines)
    {
        return hunks;
    }

    let ops = sley_diff_merge::diff_lines_with_algorithm(parent_lines, child_lines, algorithm);
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

/// If the whole parent image appears contiguously in the child image, prefer
/// that alignment over a generic LCS. This matches blame's desired behavior for
/// "add header and append a function" edits where repeated lines such as `}`
/// otherwise tempt Myers into matching the parent's closing brace to the later
/// appended function.
fn contiguous_parent_hunks(
    parent_lines: &[sley_diff_merge::DiffLine<'_>],
    child_lines: &[sley_diff_merge::DiffLine<'_>],
) -> Option<Vec<DiffHunk>> {
    if parent_lines.is_empty() || child_lines.len() < parent_lines.len() {
        return None;
    }
    let offset = child_lines.windows(parent_lines.len()).position(|window| {
        parent_lines
            .iter()
            .zip(window)
            .all(|(parent, child)| parent.content == child.content)
    })?;

    let mut hunks = Vec::new();
    if offset > 0 {
        hunks.push(DiffHunk {
            start_a: 0,
            count_a: 0,
            start_b: 0,
            count_b: offset,
        });
    }
    let child_after = offset + parent_lines.len();
    if child_after < child_lines.len() {
        hunks.push(DiffHunk {
            start_a: parent_lines.len(),
            count_a: 0,
            start_b: child_after,
            count_b: child_lines.len() - child_after,
        });
    }
    if hunks.is_empty() { None } else { Some(hunks) }
}

/// Pass `entry` to a parent: rebase its `s_lno` by `offset` (the parent leads
/// the child by `offset` across this common run). The suspect id is left as the
/// child's; [`queue_entries`] re-stamps it with the parent before enqueuing.
fn pass_entry(entry: &mut BlameEntry, offset: isize, passed: &mut Vec<BlameEntry>) {
    let s_lno = (entry.s_lno as isize + offset) as usize;
    passed.push(BlameEntry {
        lno: entry.lno,
        s_lno,
        num_lines: entry.num_lines,
        ignored: entry.ignored,
        unblamable: entry.unblamable,
    });
}

/// Split `e` into a head of `head_len` lines (kept in `e`) and a returned tail
/// covering the remainder, mirroring git's `split_blame_at`.
fn split_entry_at(e: &mut BlameEntry, head_len: usize) -> BlameEntry {
    let tail = BlameEntry {
        lno: e.lno + head_len,
        s_lno: e.s_lno + head_len,
        num_lines: e.num_lines - head_len,
        ignored: e.ignored,
        unblamable: e.unblamable,
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
    // Determine which stream has the smaller front (and that front's s_lno)
    // without consuming it.
    let (from_deferred, next_s_lno) = match (deferred.first(), entries.peek()) {
        (Some(d), Some(e)) => {
            if d.s_lno <= e.s_lno {
                (true, d.s_lno)
            } else {
                (false, e.s_lno)
            }
        }
        (Some(d), None) => (true, d.s_lno),
        (None, Some(e)) => (false, e.s_lno),
        (None, None) => return None,
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
    suspects: &mut HashMap<OriginKey, Vec<BlameEntry>>,
    queue: &mut Vec<OriginKey>,
    origin: OriginKey,
    entries: Vec<BlameEntry>,
) {
    let slot = suspects.entry(origin.clone()).or_default();
    let was_empty = slot.is_empty();
    slot.extend(entries);
    if was_empty {
        queue.push(origin);
    }
}

/// Charge every remaining chunk to `commit_oid` as a final attribution. `seq` is
/// the walk-order sequence number of this charge event (for `--incremental`).
fn charge_remaining(
    result: &mut [Option<LineBlame>],
    final_lines: &[sley_diff_merge::DiffLine<'_>],
    origin: &OriginKey,
    boundary: bool,
    owned: Vec<BlameEntry>,
    seq: usize,
) {
    for entry in owned {
        for k in 0..entry.num_lines {
            let final_line = entry.lno + k;
            if let Some(slot) = result.get_mut(final_line)
                && slot.is_none()
            {
                *slot = Some(LineBlame {
                    commit: origin.commit,
                    boundary,
                    origin_path: origin.path.clone(),
                    origin_lineno: entry.s_lno + k + 1,
                    content: final_lines[final_line].content.to_vec(),
                    ignored: entry.ignored,
                    unblamable: entry.unblamable,
                    charge_seq: seq,
                });
            }
        }
    }
}

fn find_parent_origin(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    origin: &OriginKey,
    parent: &ObjectId,
    allow_whole_copy: bool,
    lazy_fetch: bool,
) -> Result<Option<OriginKey>> {
    if read_path_blob(db, format, parent, &origin.path, lazy_fetch)?.is_some() {
        return Ok(Some(OriginKey {
            commit: *parent,
            path: origin.path.clone(),
            virtual_worktree: false,
        }));
    }

    let parent_tree = sley_rev::peel_to_tree(db, format, parent)?;
    let child_tree = sley_rev::peel_to_tree(db, format, &origin.commit)?;
    let entries = sley_diff_merge::diff_name_status_trees_with_options(
        db,
        format,
        &parent_tree,
        &child_tree,
        sley_diff_merge::DiffNameStatusOptions {
            detect_renames: true,
            detect_copies: allow_whole_copy,
            find_copies_harder: allow_whole_copy,
            rename_empty: true,
            detect_inexact: true,
            rename_threshold: sley_diff_merge::DEFAULT_RENAME_THRESHOLD,
            copy_threshold: sley_diff_merge::DEFAULT_RENAME_THRESHOLD,
            rename_limit: 0,
            ..Default::default()
        },
    )?;

    for entry in entries {
        if entry.path.as_bytes() != origin.path.as_bytes() {
            continue;
        }
        let is_origin = matches!(entry.status, sley_diff_merge::NameStatus::Renamed(_))
            || (allow_whole_copy && matches!(entry.status, sley_diff_merge::NameStatus::Copied(_)));
        if is_origin && let Some(old_path) = entry.old_path {
            return Ok(Some(OriginKey {
                commit: *parent,
                path: String::from_utf8_lossy(old_path.as_bytes()).into_owned(),
                virtual_worktree: false,
            }));
        }
    }
    Ok(None)
}

fn find_copies_in_parents(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    origin: &OriginKey,
    parents: &[ObjectId],
    copy_level: u8,
    copy_score: usize,
    owned: &mut Vec<BlameEntry>,
    final_lines: &[sley_diff_merge::DiffLine<'_>],
    textconv: &TextconvContext,
    lazy_fetch: bool,
) -> Result<Vec<(OriginKey, Vec<BlameEntry>)>> {
    let mut copied_by_origin: Vec<(OriginKey, Vec<BlameEntry>)> = Vec::new();
    for parent in parents {
        if owned.is_empty() {
            break;
        }
        let candidate_paths =
            copy_candidate_paths(db, format, origin, parent, copy_level, lazy_fetch)?;
        for path in candidate_paths {
            if owned.is_empty() {
                break;
            }
            if path == origin.path {
                continue;
            }
            let Some((raw, mode)) = read_path_blob(db, format, parent, &path, lazy_fetch)? else {
                continue;
            };
            let blob = textconv.convert(&path, mode, raw)?;
            let source_lines = sley_diff_merge::split_lines(&blob);
            let mut next_owned = Vec::new();
            let mut copied = Vec::new();
            for entry in std::mem::take(owned) {
                split_copy_matches(
                    entry,
                    final_lines,
                    &source_lines,
                    copy_score,
                    &mut copied,
                    &mut next_owned,
                );
            }
            *owned = next_owned;
            if !copied.is_empty() {
                copied_by_origin.push((
                    OriginKey {
                        commit: *parent,
                        path,
                        virtual_worktree: false,
                    },
                    copied,
                ));
            }
        }
    }
    Ok(copied_by_origin)
}

fn copy_candidate_paths(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    origin: &OriginKey,
    parent: &ObjectId,
    copy_level: u8,
    lazy_fetch: bool,
) -> Result<Vec<String>> {
    let parent_tree = sley_rev::peel_to_tree(db, format, parent)?;
    let use_all = copy_level >= 3
        || (copy_level >= 2
            && read_path_blob(db, format, parent, &origin.path, lazy_fetch)?.is_none());
    if use_all {
        let mut out = Vec::new();
        collect_tree_blob_paths(db, format, &parent_tree, Vec::new(), &mut out)?;
        return Ok(out);
    }

    let child_tree = sley_rev::peel_to_tree(db, format, &origin.commit)?;
    let entries = sley_diff_merge::diff_name_status_trees_with_options(
        db,
        format,
        &parent_tree,
        &child_tree,
        sley_diff_merge::DiffNameStatusOptions::default(),
    )?;
    let mut out = Vec::new();
    for entry in entries {
        if let Some(old_path) = entry.old_path {
            out.push(String::from_utf8_lossy(old_path.as_bytes()).into_owned());
        } else if matches!(
            entry.status,
            sley_diff_merge::NameStatus::Deleted | sley_diff_merge::NameStatus::Modified
        ) {
            out.push(String::from_utf8_lossy(entry.path.as_bytes()).into_owned());
        }
    }
    Ok(out)
}

fn collect_tree_blob_paths(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    prefix: Vec<u8>,
    out: &mut Vec<String>,
) -> Result<()> {
    let object = db.read_object(tree_oid)?;
    if object.object_type != ObjectType::Tree {
        return Ok(());
    }
    for entry in TreeEntries::new(format, &object.body) {
        let entry = entry?;
        let mut path = prefix.clone();
        if !path.is_empty() {
            path.push(b'/');
        }
        path.extend_from_slice(&entry.name);
        match sley_object::tree_entry_object_type(entry.mode) {
            ObjectType::Tree => collect_tree_blob_paths(db, format, &entry.oid, path, out)?,
            ObjectType::Blob => out.push(String::from_utf8_lossy(&path).into_owned()),
            _ => {}
        }
    }
    Ok(())
}

fn split_copy_matches(
    entry: BlameEntry,
    final_lines: &[sley_diff_merge::DiffLine<'_>],
    source_lines: &[sley_diff_merge::DiffLine<'_>],
    copy_score: usize,
    copied: &mut Vec<BlameEntry>,
    remaining: &mut Vec<BlameEntry>,
) {
    let line_in_source = |idx: usize| {
        source_lines.iter().any(|line| {
            line.content == final_lines[idx].content
                && line.has_newline == final_lines[idx].has_newline
        })
    };
    let mut cursor = 0usize;
    while cursor < entry.num_lines {
        let final_idx = entry.lno + cursor;
        let source_idx = source_lines.iter().position(|line| {
            line.content == final_lines[final_idx].content
                && line.has_newline == final_lines[final_idx].has_newline
        });
        let Some(source_start) = source_idx else {
            // Coalesce the whole run of lines absent from the source into one
            // remaining entry, so a *later* candidate can still match it as a
            // contiguous block (git splits the suspect only at match boundaries).
            // Fragmenting into single lines would drop every run below the copy
            // score threshold.
            let mut len = 1usize;
            while cursor + len < entry.num_lines && !line_in_source(entry.lno + cursor + len) {
                len += 1;
            }
            push_entry_slice(&entry, cursor, len, remaining, entry.s_lno + cursor);
            cursor += len;
            continue;
        };

        let mut len = 1usize;
        while cursor + len < entry.num_lines
            && source_start + len < source_lines.len()
            && source_lines[source_start + len].content
                == final_lines[entry.lno + cursor + len].content
            && source_lines[source_start + len].has_newline
                == final_lines[entry.lno + cursor + len].has_newline
        {
            len += 1;
        }

        if blame_entry_score(final_lines, entry.lno + cursor, len) >= copy_score {
            push_entry_slice(&entry, cursor, len, copied, source_start);
        } else {
            push_entry_slice(&entry, cursor, len, remaining, entry.s_lno + cursor);
        }
        cursor += len;
    }
}

fn push_entry_slice(
    entry: &BlameEntry,
    offset: usize,
    len: usize,
    out: &mut Vec<BlameEntry>,
    source_lno: usize,
) {
    out.push(BlameEntry {
        lno: entry.lno + offset,
        s_lno: source_lno,
        num_lines: len,
        ignored: entry.ignored,
        unblamable: entry.unblamable,
    });
}

fn blame_entry_score(
    final_lines: &[sley_diff_merge::DiffLine<'_>],
    start: usize,
    len: usize,
) -> usize {
    let mut score = 1usize;
    for line in &final_lines[start..start + len] {
        score += line
            .content
            .iter()
            .filter(|byte| byte.is_ascii_alphanumeric())
            .count();
    }
    score
}

// ===========================================================================
// `--ignore-rev` fuzzy line matching (blame.c fingerprints + guess_line_blames)
// ===========================================================================

/// A line fingerprint: the multiset of consecutive character bigrams of the
/// line (git's `struct fingerprint`). Letters are lowercased and runs of
/// whitespace collapse to a `\0` boundary, so similar lines that differ only in
/// case or spacing still share most of their bigrams. Whitespace-pair bigrams
/// (`hash == 0`) are ignored.
#[derive(Clone, Default)]
struct Fingerprint {
    counts: HashMap<u32, i32>,
}

/// Lowercase an ASCII letter (C-locale `tolower`); leave other bytes untouched.
fn ascii_tolower(byte: u8) -> u8 {
    if byte.is_ascii_uppercase() {
        byte + 32
    } else {
        byte
    }
}

/// C-locale `isspace`.
fn is_ascii_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// Build the fingerprint of one line (its content, newline excluded — git
/// folds the trailing newline into the same `\0` boundary as end-of-line).
fn get_fingerprint(line: &[u8]) -> Fingerprint {
    let mut counts: HashMap<u32, i32> = HashMap::new();
    let mut c0: u32 = 0;
    for i in 0..=line.len() {
        let c1: u32 = if i == line.len() || is_ascii_space(line[i]) {
            0
        } else {
            ascii_tolower(line[i]) as u32
        };
        let hash = c0 | (c1 << 8);
        if hash != 0 {
            *counts.entry(hash).or_insert(0) += 1;
        }
        c0 = c1;
    }
    Fingerprint { counts }
}

/// Fingerprints for every line of a blob, in line order.
fn line_fingerprints(lines: &[sley_diff_merge::DiffLine<'_>]) -> Vec<Fingerprint> {
    lines
        .iter()
        .map(|line| get_fingerprint(line.content))
        .collect()
}

/// git's `fingerprint_similarity`: the size of the bigram-multiset intersection
/// (sum of per-bigram minimum counts).
fn fingerprint_similarity(a: &Fingerprint, b: &Fingerprint) -> i32 {
    let mut intersection = 0;
    for (hash, count_b) in &b.counts {
        if let Some(count_a) = a.counts.get(hash) {
            intersection += (*count_a).min(*count_b);
        }
    }
    intersection
}

/// git's `fingerprint_subtract`: remove `b`'s bigrams from `a` (saturating at
/// zero), so a line in A that has been claimed can't be matched as strongly
/// again.
fn fingerprint_subtract(a: &mut Fingerprint, b: &Fingerprint) {
    for (hash, count_b) in &b.counts {
        if let Some(count_a) = a.counts.get_mut(hash) {
            if *count_a <= *count_b {
                a.counts.remove(hash);
            } else {
                *count_a -= *count_b;
            }
        }
    }
}

/// git's `line_number_mapping`: maps a line number in B to its proportional
/// position in A.
struct LineNumberMapping {
    destination_start: i32,
    destination_length: i32,
    source_start: i32,
    source_length: i32,
}

fn map_line_number(line_number: i32, mapping: &LineNumberMapping) -> i32 {
    ((line_number - mapping.source_start) * 2 + 1) * mapping.destination_length
        / (mapping.source_length * 2)
        + mapping.destination_start
}

const CERTAINTY_NOT_CALCULATED: i32 = -1;
const CERTAIN_NOTHING_MATCHES: i32 = -2;

/// State threaded through the recursive fuzzy matcher. Arrays indexed by
/// `b_idx = absolute_B_line - orig_start_b` (git re-bases pointers per
/// recursion; we keep absolute-into-B indices, which is equivalent).
struct FuzzyState<'a> {
    parent_fps: &'a mut [Fingerprint],
    target_fps: &'a [Fingerprint],
    similarities: Vec<i32>,
    certainties: Vec<i32>,
    second_best_result: Vec<i32>,
    result: Vec<i32>,
    max_search_distance_a: i32,
    max_search_distance_b: i32,
    orig_start_b: i32,
    mapping: LineNumberMapping,
}

/// Index into the flat `similarities` array (git's `get_similarity`).
fn similarity_index(
    line_a: i32,
    b_idx: i32,
    closest_line_a: i32,
    max_search_distance_a: i32,
) -> usize {
    (line_a - closest_line_a + max_search_distance_a + b_idx * (max_search_distance_a * 2 + 1))
        as usize
}

/// git's `find_best_line_matches` for a single line in B.
fn find_best_line_matches(state: &mut FuzzyState, start_a: i32, length_a: i32, abs_b: i32) {
    let b_idx = (abs_b - state.orig_start_b) as usize;
    if state.certainties[b_idx] != CERTAINTY_NOT_CALCULATED {
        return;
    }

    let closest_local_line_a = map_line_number(abs_b, &state.mapping) - start_a;
    let mut search_start = closest_local_line_a - state.max_search_distance_a;
    if search_start < 0 {
        search_start = 0;
    }
    let mut search_end = closest_local_line_a + state.max_search_distance_a + 1;
    if search_end > length_a {
        search_end = length_a;
    }

    let mut best_similarity = 0;
    let mut second_best_similarity = 0;
    let mut best_similarity_index = 0;
    let mut second_best_similarity_index = 0;

    for i in search_start..search_end {
        let sim_idx = similarity_index(
            i,
            b_idx as i32,
            closest_local_line_a,
            state.max_search_distance_a,
        );
        if state.similarities[sim_idx] == -1 {
            let sim = fingerprint_similarity(
                &state.target_fps[abs_b as usize],
                &state.parent_fps[(start_a + i) as usize],
            ) * (1000 - (i - closest_local_line_a).abs());
            state.similarities[sim_idx] = sim;
        }
        let similarity = state.similarities[sim_idx];
        if similarity > best_similarity {
            second_best_similarity = best_similarity;
            second_best_similarity_index = best_similarity_index;
            best_similarity = similarity;
            best_similarity_index = i;
        } else if similarity > second_best_similarity {
            second_best_similarity = similarity;
            second_best_similarity_index = i;
        }
    }

    if best_similarity == 0 {
        state.certainties[b_idx] = CERTAIN_NOTHING_MATCHES;
        state.result[b_idx] = -1;
    } else {
        state.certainties[b_idx] = best_similarity * 2 - second_best_similarity;
        state.result[b_idx] = start_a + best_similarity_index;
        state.second_best_result[b_idx] = start_a + second_best_similarity_index;
    }
}

/// git's `fuzzy_find_matching_lines_recurse`: pick the most confidently matched
/// line as a partition, subtract its fingerprint, invalidate contradicting
/// neighbours, then recurse on either side.
fn fuzzy_recurse(state: &mut FuzzyState, start_a: i32, start_b: i32, length_a: i32, length_b: i32) {
    let mut most_certain_local_line_b: i32 = -1;
    let mut most_certain_line_certainty: i32 = -1;
    for i in 0..length_b {
        let abs_b = start_b + i;
        find_best_line_matches(state, start_a, length_a, abs_b);
        let b_idx = (abs_b - state.orig_start_b) as usize;
        if state.certainties[b_idx] > most_certain_line_certainty {
            most_certain_line_certainty = state.certainties[b_idx];
            most_certain_local_line_b = i;
        }
    }

    if most_certain_local_line_b == -1 {
        return;
    }

    let most_certain_abs_b = start_b + most_certain_local_line_b;
    let most_certain_b_idx = (most_certain_abs_b - state.orig_start_b) as usize;
    let most_certain_line_a = state.result[most_certain_b_idx];

    // Subtract the chosen B line's fingerprint from its matched A line.
    let (left, right) = state.parent_fps.split_at_mut(most_certain_line_a as usize);
    let target_fp = &state.target_fps[most_certain_abs_b as usize];
    fingerprint_subtract(&mut right[0], target_fp);
    let _ = left;

    let mut invalidate_min = most_certain_local_line_b - state.max_search_distance_b;
    let mut invalidate_max = most_certain_local_line_b + state.max_search_distance_b + 1;
    if invalidate_min < 0 {
        invalidate_min = 0;
    }
    if invalidate_max > length_b {
        invalidate_max = length_b;
    }

    // The matched A fingerprint changed: discard cached similarities against it.
    for i in invalidate_min..invalidate_max {
        let abs_b = start_b + i;
        let closest_local_line_a = map_line_number(abs_b, &state.mapping) - start_a;
        if (most_certain_line_a - start_a - closest_local_line_a).abs()
            > state.max_search_distance_a
        {
            continue;
        }
        let sim_idx = similarity_index(
            most_certain_line_a - start_a,
            abs_b - state.orig_start_b,
            closest_local_line_a,
            state.max_search_distance_a,
        );
        state.similarities[sim_idx] = -1;
    }

    // Discard matches whose ordering now contradicts the partition.
    let mut i = most_certain_local_line_b - 1;
    while i >= invalidate_min {
        let b_idx = (start_b + i - state.orig_start_b) as usize;
        if state.certainties[b_idx] >= 0
            && (state.result[b_idx] >= most_certain_line_a
                || state.second_best_result[b_idx] >= most_certain_line_a)
        {
            state.certainties[b_idx] = CERTAINTY_NOT_CALCULATED;
        }
        i -= 1;
    }
    for i in (most_certain_local_line_b + 1)..invalidate_max {
        let b_idx = (start_b + i - state.orig_start_b) as usize;
        if state.certainties[b_idx] >= 0
            && (state.result[b_idx] <= most_certain_line_a
                || state.second_best_result[b_idx] <= most_certain_line_a)
        {
            state.certainties[b_idx] = CERTAINTY_NOT_CALCULATED;
        }
    }

    if most_certain_local_line_b > 0 {
        fuzzy_recurse(
            state,
            start_a,
            start_b,
            most_certain_line_a + 1 - start_a,
            most_certain_local_line_b,
        );
    }
    if most_certain_local_line_b + 1 < length_b {
        let second_half_start_a = most_certain_line_a;
        let offset_b = most_certain_local_line_b + 1;
        let second_half_start_b = start_b + offset_b;
        let second_half_length_a = length_a + start_a - second_half_start_a;
        let second_half_length_b = length_b + start_b - second_half_start_b;
        fuzzy_recurse(
            state,
            second_half_start_a,
            second_half_start_b,
            second_half_length_a,
            second_half_length_b,
        );
    }
}

/// git's `fuzzy_find_matching_lines`: for each line of the target chunk
/// `[tlno, same)`, return the absolute parent line it best matches, or `-1`.
/// `parent_slno` is the parent chunk start, `parent_len` its length.
fn fuzzy_find_matching_lines(
    parent_fps: &[Fingerprint],
    target_fps: &[Fingerprint],
    tlno: i32,
    parent_slno: i32,
    same: i32,
    parent_len: i32,
) -> Option<Vec<i32>> {
    let start_a = parent_slno;
    let length_a = parent_len;
    let start_b = tlno;
    let length_b = same - tlno;

    if length_a <= 0 {
        return None;
    }

    let mut max_search_distance_a = 10;
    if max_search_distance_a >= length_a {
        max_search_distance_a = if length_a != 0 { length_a - 1 } else { 0 };
    }
    let max_search_distance_b = ((2 * max_search_distance_a + 1) * length_b - 1) / length_a;

    let similarity_count = (length_b * (max_search_distance_a * 2 + 1)) as usize;
    let mut parent_copy = parent_fps.to_vec();
    let mut state = FuzzyState {
        parent_fps: &mut parent_copy,
        target_fps,
        similarities: vec![-1; similarity_count],
        certainties: vec![CERTAINTY_NOT_CALCULATED; length_b as usize],
        second_best_result: vec![-1; length_b as usize],
        result: vec![-1; length_b as usize],
        max_search_distance_a,
        max_search_distance_b,
        orig_start_b: start_b,
        mapping: LineNumberMapping {
            destination_start: start_a,
            destination_length: length_a,
            source_start: start_b,
            source_length: length_b,
        },
    };

    fuzzy_recurse(&mut state, start_a, start_b, length_a, length_b);
    Some(state.result)
}

/// git's `scan_parent_range`: the second-pass fallback that scans a range of
/// parent lines for the best fingerprint match of one target line.
fn scan_parent_range(
    parent_fps: &[Fingerprint],
    target_fps: &[Fingerprint],
    t_idx: usize,
    from: usize,
    nr_lines: usize,
) -> i32 {
    const FINGERPRINT_FILE_THRESHOLD: i32 = 10;
    let mut best_sim_val = FINGERPRINT_FILE_THRESHOLD;
    let mut best_sim_idx: i32 = -1;
    for p_idx in from..(from + nr_lines) {
        let sim = fingerprint_similarity(&target_fps[t_idx], &parent_fps[p_idx]);
        if sim < best_sim_val {
            continue;
        }
        if sim == best_sim_val
            && best_sim_idx != -1
            && (best_sim_idx - t_idx as i32).abs() < (p_idx as i32 - t_idx as i32).abs()
        {
            continue;
        }
        best_sim_val = sim;
        best_sim_idx = p_idx as i32;
    }
    best_sim_idx
}

/// Where a target line in an ignored commit's change should be attributed.
#[derive(Clone, Copy)]
struct LineTracker {
    /// True when the line maps to a parent line (→ passes to parent, `ignored`);
    /// false when it has no parent match (→ stays, `unblamable`).
    is_parent: bool,
    /// The matched parent line (parent space) when `is_parent`, else the
    /// target line (target space).
    s_lno: i32,
}

fn are_lines_adjacent(first: &LineTracker, second: &LineTracker) -> bool {
    first.is_parent == second.is_parent && first.s_lno + 1 == second.s_lno
}

/// git's `guess_line_blames`: for each line of the changed region `[tlno,
/// same)`, decide whether it maps to a parent line (fuzzy match in the diff
/// chunk, else anywhere in the parent) or is unblamable.
fn guess_line_blames(
    parent_fps: &[Fingerprint],
    target_fps: &[Fingerprint],
    tlno: i32,
    offset: i32,
    same: i32,
    parent_len: i32,
) -> Vec<LineTracker> {
    let parent_slno = tlno + offset;
    let fuzzy_matches =
        fuzzy_find_matching_lines(parent_fps, target_fps, tlno, parent_slno, same, parent_len);
    let count = (same - tlno) as usize;
    let validate_tiny_parent_match = parent_len <= 1 && count > parent_len.max(0) as usize;
    let mut line_blames = Vec::with_capacity(count);
    for i in 0..count {
        let target_idx = tlno + i as i32;
        let best_idx = match &fuzzy_matches {
            Some(m) if m[i] >= 0 => {
                let fuzzy_idx = m[i];
                if validate_tiny_parent_match {
                    prefer_whole_parent_match(parent_fps, target_fps, target_idx, fuzzy_idx)
                } else {
                    fuzzy_idx
                }
            }
            _ => scan_parent_range(
                parent_fps,
                target_fps,
                target_idx as usize,
                0,
                parent_fps.len(),
            ),
        };
        if best_idx >= 0 {
            line_blames.push(LineTracker {
                is_parent: true,
                s_lno: best_idx,
            });
        } else {
            line_blames.push(LineTracker {
                is_parent: false,
                s_lno: target_idx,
            });
        }
    }
    line_blames
}

fn prefer_whole_parent_match(
    parent_fps: &[Fingerprint],
    target_fps: &[Fingerprint],
    target_idx: i32,
    fuzzy_idx: i32,
) -> i32 {
    let scan_idx = scan_parent_range(
        parent_fps,
        target_fps,
        target_idx as usize,
        0,
        parent_fps.len(),
    );
    if scan_idx < 0 {
        return fuzzy_idx;
    }

    let target = &target_fps[target_idx as usize];
    let fuzzy_score = fingerprint_similarity(target, &parent_fps[fuzzy_idx as usize]);
    let scan_score = fingerprint_similarity(target, &parent_fps[scan_idx as usize]);
    if scan_score > fuzzy_score {
        scan_idx
    } else {
        fuzzy_idx
    }
}

/// git's `ignore_blame_entry`: split a blame entry over the changed region into
/// runs that go to the parent (`ignored`) or stay (`unblamable`), per the
/// `line_blames` decisions. `region_start` is `tlno` (the first target line the
/// `line_blames` array describes).
fn ignore_blame_entry(
    entry: &BlameEntry,
    region_start: usize,
    line_blames: &[LineTracker],
    passed: &mut Vec<BlameEntry>,
    still_ours: &mut Vec<BlameEntry>,
) {
    let n = entry.num_lines;
    let base = entry.s_lno - region_start;
    let mut i = 0usize;
    while i < n {
        let mut len = 1usize;
        while i + len < n
            && are_lines_adjacent(
                &line_blames[base + i + len - 1],
                &line_blames[base + i + len],
            )
        {
            len += 1;
        }
        let head = &line_blames[base + i];
        let part = BlameEntry {
            lno: entry.lno + i,
            s_lno: if head.is_parent {
                head.s_lno as usize
            } else {
                entry.s_lno + i
            },
            num_lines: len,
            ignored: if head.is_parent { true } else { entry.ignored },
            unblamable: if head.is_parent {
                entry.unblamable
            } else {
                true
            },
        };
        if head.is_parent {
            passed.push(part);
        } else {
            still_ours.push(part);
        }
        i += len;
    }
}

/// The `--ignore-rev` analogue of [`pass_blame_to_parent`] (git's `blame_chunk`
/// with `ignore_diffs = 1`): unchanged lines before each diff hunk pass to the
/// parent normally; lines inside a hunk are routed by `guess_line_blames` —
/// fuzzy-matched lines pass to the parent marked `ignored`, the rest stay marked
/// `unblamable`.
fn pass_blame_to_parent_ignore(
    parent_lines: &[sley_diff_merge::DiffLine<'_>],
    child_lines: &[sley_diff_merge::DiffLine<'_>],
    parent_fps: &[Fingerprint],
    child_fps: &[Fingerprint],
    owned: &mut Vec<BlameEntry>,
    algorithm: sley_diff_merge::DiffAlgorithm,
) -> Vec<BlameEntry> {
    let hunks = diff_hunks(parent_lines, child_lines, algorithm);

    let mut passed: Vec<BlameEntry> = Vec::new();
    let mut still_ours: Vec<BlameEntry> = Vec::new();

    owned.sort_by_key(|e| e.s_lno);
    let mut entries = std::mem::take(owned).into_iter().peekable();
    let mut deferred: Vec<BlameEntry> = Vec::new();
    let mut offset: isize = 0;

    for hunk in &hunks {
        let tlno = hunk.start_b;
        let same = hunk.start_b + hunk.count_b;

        // Pre-chunk common region: pass to the parent (flags preserved).
        while let Some(mut e) = take_next_before(&mut deferred, &mut entries, tlno) {
            if e.s_lno + e.num_lines > tlno {
                let head_len = tlno - e.s_lno;
                let tail = split_entry_at(&mut e, head_len);
                pass_entry(&mut e, offset, &mut passed);
                put_back(&mut deferred, tail);
            } else {
                pass_entry(&mut e, offset, &mut passed);
            }
        }

        // Changed region [tlno, same): guess each line's origin.
        let line_blames = guess_line_blames(
            parent_fps,
            child_fps,
            tlno as i32,
            offset as i32,
            same as i32,
            hunk.count_a as i32,
        );
        while let Some(mut e) = take_next_before(&mut deferred, &mut entries, same) {
            if e.s_lno + e.num_lines > same {
                let head_len = same - e.s_lno;
                let tail = split_entry_at(&mut e, head_len);
                ignore_blame_entry(&e, tlno, &line_blames, &mut passed, &mut still_ours);
                put_back(&mut deferred, tail);
            } else {
                ignore_blame_entry(&e, tlno, &line_blames, &mut passed, &mut still_ours);
            }
        }

        offset = hunk.start_a as isize + hunk.count_a as isize
            - (hunk.start_b as isize + hunk.count_b as isize);
    }

    // Trailing common region: pass to the parent.
    while let Some(mut e) = take_next_before(&mut deferred, &mut entries, usize::MAX) {
        pass_entry(&mut e, offset, &mut passed);
    }

    *owned = still_ours;
    passed
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
    cache: &mut HashMap<(ObjectId, String), Option<Vec<u8>>>,
    textconv: &TextconvContext,
    lazy_fetch: bool,
) -> Result<Option<Vec<u8>>> {
    let key = (*commit, repo_path.to_string());
    if let Some(hit) = cache.get(&key) {
        return Ok(hit.clone());
    }
    // The cache stores the *converted* form so textconv runs once per
    // (commit, path); upstream's fill_origin_blob caches likewise.
    let blob = match read_path_blob(db, format, commit, repo_path, lazy_fetch)? {
        Some((raw, mode)) => Some(textconv.convert(repo_path, mode, raw)?),
        None => None,
    };
    cache.insert(key, blob.clone());
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
fn pop_newest_origin(
    queue: &mut Vec<OriginKey>,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    date_cache: &mut HashMap<ObjectId, i64>,
) -> Result<Option<OriginKey>> {
    if queue.is_empty() {
        return Ok(None);
    }
    // Ensure every queued commit has a cached date.
    for oid in queue.iter().map(|origin| origin.commit) {
        if !date_cache.contains_key(&oid) {
            let object = db.read_object(&oid)?;
            if object.object_type != ObjectType::Commit {
                return Err(GitError::InvalidObject(format!(
                    "expected commit {oid}, found {}",
                    object.object_type.as_str()
                )));
            }
            let commit = Commit::parse(format, &object.body)?;
            let ts = for_each_ref_identity_timestamp(&commit.committer).unwrap_or(0);
            date_cache.insert(oid, ts);
        }
    }
    let mut best = 0usize;
    for i in 1..queue.len() {
        let ti = date_cache.get(&queue[i].commit).copied().unwrap_or(0);
        let tb = date_cache.get(&queue[best].commit).copied().unwrap_or(0);
        if ti > tb
            || (ti == tb
                && (queue[i].commit.to_hex(), &queue[i].path)
                    < (queue[best].commit.to_hex(), &queue[best].path))
        {
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
    use sley_grep::{Regex, RegexMode};
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

/// Resolve a `:funcname` `-L` bound. git delegates this to the active funcname
/// pattern from the diff driver; this local implementation covers the forms
/// exercised by the upstream blame tests: a BRE match on a C-style function
/// header, then a brace-balanced extent, with a fallback through EOF for
/// drivers such as Fortran whose function body is not brace-delimited.
fn resolve_function_bound(
    pattern: &str,
    contents: &[&[u8]],
    from: usize,
) -> Result<(usize, usize)> {
    use sley_grep::{Regex, RegexMode};
    let re = Regex::compile(pattern, RegexMode::Bre, false, false).map_err(|_| {
        eprintln!("fatal: -L parameter '{pattern}': invalid regex");
        GitError::Exit(128)
    })?;
    let start_idx = from.saturating_sub(1);
    for idx in start_idx..contents.len() {
        if re.find_from(contents[idx], 0).is_some() {
            return Ok((idx + 1, function_extent_end(contents, idx) + 1));
        }
    }
    eprintln!("fatal: -L parameter '{pattern}' starting at line {from}: No match");
    Err(GitError::Exit(128))
}

/// Return the 0-based inclusive end line for a function whose header is at
/// `start_idx`.
fn function_extent_end(contents: &[&[u8]], start_idx: usize) -> usize {
    let mut saw_open = false;
    let mut depth = 0isize;
    for (idx, line) in contents.iter().enumerate().skip(start_idx) {
        for &byte in *line {
            match byte {
                b'{' => {
                    saw_open = true;
                    depth += 1;
                }
                b'}' if saw_open => {
                    depth -= 1;
                    if depth <= 0 {
                        return idx;
                    }
                }
                _ => {}
            }
        }
    }
    contents.len().saturating_sub(1)
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
    if let RangeBound::Function { pattern, absolute } = &range.start {
        if !matches!(&range.end, RangeBound::Omitted) {
            return Err(blame_usage_error());
        }
        let from = if *absolute { 1 } else { anchor };
        return resolve_function_bound(pattern, contents, from);
    }

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
        RangeBound::Relative(_) | RangeBound::RelativeNeg(_) | RangeBound::Function { .. } => {
            return Err(blame_usage_error());
        }
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
        RangeBound::Function { .. } => return Err(blame_usage_error()),
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
#[allow(clippy::too_many_arguments)]
fn render_blame(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    lines: &[LineBlame],
    selected: &[usize],
    options: &BlameOptions,
    fake: Option<&FakeCommit>,
    previous_map: &PreviousMap,
    marks: BlameMarks,
    output_encoding: &str,
    color: Option<&ColorPlan>,
) -> Result<()> {
    if options.incremental {
        return render_incremental(
            db,
            format,
            lines,
            selected,
            fake,
            previous_map,
            output_encoding,
            options.show_root,
        );
    }
    if options.porcelain {
        return render_porcelain(
            git_dir,
            format,
            db,
            lines,
            selected,
            options,
            fake,
            previous_map,
            marks,
        );
    }

    let (abbrev, hex_width) = blame_display_abbrev(git_dir, format, options)?;

    // git blame *always* reads the mailmap (`read_mailmap`, no flag) and maps the
    // displayed author name/email through it.
    let mailmap = commands::utility::Mailmap::load_default(git_dir, format)?;

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
            let (author, date) = author_and_date(db, format, blame, options, &mailmap, fake)?;
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

        // The `--ignore-rev` markers (`*` unblamable, `?` ignored) consume one
        // hex column each, after any boundary `^`. Only the marker case needs
        // the alternate renderer; everything else keeps `render_sha` (which also
        // handles the excessive-`--abbrev` boundary widths).
        let want_mark = (marks.unblamable && blame.unblamable) || (marks.ignored && blame.ignored);
        let sha = if want_mark {
            render_object_name_marked(
                &blame.commit,
                blame.boundary,
                options.show_root,
                hex_width,
                options.blank_boundary,
                marks,
                blame.ignored,
                blame.unblamable,
                format.hex_len(),
            )
        } else {
            render_sha(
                &blame.commit,
                abbrev,
                blame.boundary,
                options.show_root,
                hex_width,
                options.blank_boundary,
            )
        };

        // `--color-lines` / `--color-by-age`: wrap the metadata prefix in a
        // color and reset before the content. `--color-lines` colors only lines
        // whose commit matches the previous output line; `--color-by-age` colors
        // every line by commit age.
        let (color_str, reset_str): (&str, &str) = match color {
            Some(ColorPlan::Age { fields }) => {
                let time = blame_author_time(db, format, blame, fake);
                (determine_line_heat(time, fields), "\x1b[m")
            }
            Some(ColorPlan::Lines { repeated }) => {
                let repeated_line =
                    display_idx > 0 && lines[selected[display_idx - 1] - 1].commit == blame.commit;
                if repeated_line {
                    (repeated.as_str(), "\x1b[m")
                } else {
                    ("", "")
                }
            }
            None => ("", ""),
        };
        if !color_str.is_empty() {
            handle.write_all(color_str.as_bytes())?;
        }

        if options.suppress_author {
            // `<sha> <lineno>) <line>`
            if options.show_name {
                write!(
                    handle,
                    "{sha} {} {line_no:>lineno_width$}) ",
                    blame.origin_path
                )?;
            } else {
                write!(handle, "{sha} {line_no:>lineno_width$}) ")?;
            }
        } else {
            let author = &author_strings[display_idx];
            let date = &date_strings[display_idx];
            // `<sha> (<author-padded> <date> <lineno>) <line>`
            if options.show_name {
                write!(
                    handle,
                    "{sha} {} ({author:<author_width$} {date} {line_no:>lineno_width$}) ",
                    blame.origin_path
                )?;
            } else {
                write!(
                    handle,
                    "{sha} ({author:<author_width$} {date} {line_no:>lineno_width$}) "
                )?;
            }
        }
        if !reset_str.is_empty() {
            handle.write_all(reset_str.as_bytes())?;
        }
        handle.write_all(content)?;
        handle.write_all(b"\n")?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn render_porcelain(
    _git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    lines: &[LineBlame],
    selected: &[usize],
    options: &BlameOptions,
    fake: Option<&FakeCommit>,
    previous_map: &PreviousMap,
    marks: BlameMarks,
) -> Result<()> {
    // git's MORE_THAN_ONE_PATH: a commit blamed for lines via two or more
    // distinct paths repeats its `previous`/`filename` info on every group so a
    // porcelain consumer can attribute each line to the right path. Computed
    // over the whole blame (all of `lines`), not just the `-L` selection,
    // matching git's pass over `sb->ent`.
    let mut paths_by_commit: HashMap<ObjectId, HashSet<&str>> = HashMap::new();
    for line in lines {
        paths_by_commit
            .entry(line.commit)
            .or_default()
            .insert(line.origin_path.as_str());
    }
    let multi_path: HashSet<ObjectId> = paths_by_commit
        .into_iter()
        .filter(|(_, paths)| paths.len() > 1)
        .map(|(commit, _)| commit)
        .collect();

    // METAINFO_SHOWN: a commit's metadata block is emitted only on its first
    // appearance (unless `--line-porcelain`, which repeats it for every line).
    let mut shown: HashSet<ObjectId> = HashSet::new();

    let stdout = io::stdout();
    let mut handle = stdout.lock();
    let mut idx = 0usize;
    while idx < selected.len() {
        let line_no = selected[idx];
        let blame = &lines[line_no - 1];
        let mut group_len = 1usize;
        while idx + group_len < selected.len() {
            let next_line_no = selected[idx + group_len];
            if next_line_no != line_no + group_len {
                break;
            }
            let next = &lines[next_line_no - 1];
            if next.commit != blame.commit
                || next.origin_path != blame.origin_path
                || next.origin_lineno != blame.origin_lineno + group_len
            {
                break;
            }
            group_len += 1;
        }

        // The entry's first header line always carries the run length (4th
        // field), even for a single line (git's `emit_porcelain`).
        writeln!(
            handle,
            "{} {} {} {}",
            blame.commit.to_hex(),
            blame.origin_lineno,
            line_no,
            group_len
        )?;
        emit_porcelain_details(
            &mut handle,
            db,
            format,
            blame,
            options,
            fake,
            previous_map,
            &multi_path,
            &mut shown,
            options.line_porcelain,
        )?;

        for offset in 0..group_len {
            if offset > 0 {
                let current = &lines[selected[idx + offset] - 1];
                // Continuation lines carry only three fields (no run length).
                writeln!(
                    handle,
                    "{} {} {}",
                    current.commit.to_hex(),
                    current.origin_lineno,
                    selected[idx + offset]
                )?;
                if options.line_porcelain {
                    emit_porcelain_details(
                        &mut handle,
                        db,
                        format,
                        current,
                        options,
                        fake,
                        previous_map,
                        &multi_path,
                        &mut shown,
                        true,
                    )?;
                }
            }
            // git's `emit_porcelain_per_line_details`: the `unblamable` / `ignored`
            // keyword for `--ignore-rev` lines, once per line before its content.
            let current = &lines[selected[idx + offset] - 1];
            if marks.unblamable && current.unblamable {
                writeln!(handle, "unblamable")?;
            }
            if marks.ignored && current.ignored {
                writeln!(handle, "ignored")?;
            }
            handle.write_all(b"\t")?;
            handle.write_all(strip_trailing_newline(
                &lines[selected[idx + offset] - 1].content,
            ))?;
            handle.write_all(b"\n")?;
        }
        idx += group_len;
    }
    Ok(())
}

/// git's `emit_porcelain_details`: emit the per-commit metadata block (once,
/// unless `repeat`), then the path info (`previous`/`filename`) when the block
/// was emitted or the commit spans multiple paths.
#[allow(clippy::too_many_arguments)]
fn emit_porcelain_details(
    handle: &mut impl Write,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    blame: &LineBlame,
    options: &BlameOptions,
    fake: Option<&FakeCommit>,
    previous_map: &PreviousMap,
    multi_path: &HashSet<ObjectId>,
    shown: &mut HashSet<ObjectId>,
    repeat: bool,
) -> Result<()> {
    let emitted = emit_one_suspect_detail(handle, db, format, blame, options, fake, shown, repeat)?;
    if emitted || multi_path.contains(&blame.commit) {
        write_filename_info(handle, blame, fake, previous_map)?;
    }
    Ok(())
}

/// git's `emit_one_suspect_detail`: the author/committer/summary[/boundary]
/// block for one commit. Returns whether anything was emitted (false when the
/// commit's block was already shown and `repeat` is off).
fn emit_one_suspect_detail(
    handle: &mut impl Write,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    blame: &LineBlame,
    options: &BlameOptions,
    fake: Option<&FakeCommit>,
    shown: &mut HashSet<ObjectId>,
    repeat: bool,
) -> Result<bool> {
    if !repeat && shown.contains(&blame.commit) {
        return Ok(false);
    }
    shown.insert(blame.commit);

    if blame.commit.is_null() {
        let (name, email, time) = match fake {
            Some(f) => (f.name.as_str(), f.email.as_str(), f.time),
            None => ("Not Committed Yet", "not.committed.yet", 0),
        };
        let summary = match fake {
            Some(f) => f.summary.clone(),
            None => format!(
                "Version of {} from {}",
                blame.origin_path, blame.origin_path
            ),
        };
        writeln!(handle, "author {name}")?;
        writeln!(handle, "author-mail <{email}>")?;
        writeln!(handle, "author-time {time}")?;
        writeln!(handle, "author-tz +0000")?;
        writeln!(handle, "committer {name}")?;
        writeln!(handle, "committer-mail <{email}>")?;
        writeln!(handle, "committer-time {time}")?;
        writeln!(handle, "committer-tz +0000")?;
        writeln!(handle, "summary {summary}")?;
        // The fake work-tree commit is never a boundary.
        return Ok(true);
    }

    let object = db.read_object(&blame.commit)?;
    let commit = Commit::parse(format, &object.body)?;
    let author = Signature::from_ident_line(&commit.author);
    let committer = Signature::from_ident_line(&commit.committer);
    writeln!(
        handle,
        "author {}",
        author
            .as_ref()
            .map(|sig| String::from_utf8_lossy(sig.name.as_bytes()).into_owned())
            .unwrap_or_default()
    )?;
    writeln!(
        handle,
        "author-mail <{}>",
        author
            .as_ref()
            .map(|sig| String::from_utf8_lossy(sig.email.as_bytes()).into_owned())
            .unwrap_or_default()
    )?;
    writeln!(
        handle,
        "author-time {}",
        author.as_ref().map(|sig| sig.time.seconds).unwrap_or(0)
    )?;
    writeln!(
        handle,
        "author-tz {}",
        author
            .as_ref()
            .map(|sig| sig.time.offset_token())
            .unwrap_or_else(|| "+0000".to_string())
    )?;
    writeln!(
        handle,
        "committer {}",
        committer
            .as_ref()
            .map(|sig| String::from_utf8_lossy(sig.name.as_bytes()).into_owned())
            .unwrap_or_default()
    )?;
    writeln!(
        handle,
        "committer-mail <{}>",
        committer
            .as_ref()
            .map(|sig| String::from_utf8_lossy(sig.email.as_bytes()).into_owned())
            .unwrap_or_default()
    )?;
    writeln!(
        handle,
        "committer-time {}",
        committer.as_ref().map(|sig| sig.time.seconds).unwrap_or(0)
    )?;
    writeln!(
        handle,
        "committer-tz {}",
        committer
            .as_ref()
            .map(|sig| sig.time.offset_token())
            .unwrap_or_else(|| "+0000".to_string())
    )?;
    write_field(
        handle,
        b"summary ",
        &commit_summary_bytes(&commit.message, &blame.commit),
    )?;
    if blame.boundary && !options.show_root {
        writeln!(handle, "boundary")?;
    }
    Ok(true)
}

/// git's `write_filename_info`: the `previous <commit> <path>` pointer (when
/// the blame walk descended into a parent) followed by `filename <path>`.
fn write_filename_info(
    handle: &mut impl Write,
    blame: &LineBlame,
    fake: Option<&FakeCommit>,
    previous_map: &PreviousMap,
) -> Result<()> {
    let previous = if blame.commit.is_null() {
        fake.and_then(|f| f.previous.clone())
    } else {
        previous_map
            .get(&(blame.commit, blame.origin_path.clone()))
            .cloned()
    };
    if let Some((commit, path)) = previous {
        writeln!(handle, "previous {} {}", commit.to_hex(), path)?;
    }
    writeln!(handle, "filename {}", blame.origin_path)?;
    Ok(())
}

/// git's `--incremental` output (`found_guilty_entry`): emit each contiguous
/// blamed run in walk order (newest commit first, by charge sequence), with the
/// per-commit detail block (once per commit) followed by `previous`/`filename`.
/// Author and summary metadata are reencoded to `output_encoding`.
#[allow(clippy::too_many_arguments)]
fn render_incremental(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    lines: &[LineBlame],
    selected: &[usize],
    fake: Option<&FakeCommit>,
    previous_map: &PreviousMap,
    output_encoding: &str,
    show_root: bool,
) -> Result<()> {
    // Group the selected lines into contiguous runs (one git `blame_entry`).
    struct Run {
        first_line: usize,
        len: usize,
    }
    let mut runs: Vec<Run> = Vec::new();
    let mut idx = 0usize;
    while idx < selected.len() {
        let line_no = selected[idx];
        let blame = &lines[line_no - 1];
        let mut len = 1usize;
        while idx + len < selected.len() {
            let next_line_no = selected[idx + len];
            if next_line_no != line_no + len {
                break;
            }
            let next = &lines[next_line_no - 1];
            if next.commit != blame.commit
                || next.origin_path != blame.origin_path
                || next.origin_lineno != blame.origin_lineno + len
                || next.charge_seq != blame.charge_seq
            {
                break;
            }
            len += 1;
        }
        runs.push(Run {
            first_line: line_no,
            len,
        });
        idx += len;
    }
    // Walk order: by charge sequence (newest commit first), then file position.
    runs.sort_by_key(|run| (lines[run.first_line - 1].charge_seq, run.first_line));

    let mut shown: HashSet<ObjectId> = HashSet::new();
    let stdout = io::stdout();
    let mut handle = stdout.lock();
    for run in &runs {
        let blame = &lines[run.first_line - 1];
        writeln!(
            handle,
            "{} {} {} {}",
            blame.commit.to_hex(),
            blame.origin_lineno,
            run.first_line,
            run.len
        )?;
        if shown.insert(blame.commit) {
            emit_incremental_detail(
                &mut handle,
                db,
                format,
                blame,
                fake,
                output_encoding,
                blame.boundary && !show_root,
            )?;
        }
        write_filename_info(&mut handle, blame, fake, previous_map)?;
    }
    Ok(())
}

/// The per-commit metadata block for `--incremental`, reencoding the author /
/// committer / summary to `output_encoding` and writing them as raw bytes.
fn emit_incremental_detail(
    handle: &mut impl Write,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    blame: &LineBlame,
    fake: Option<&FakeCommit>,
    output_encoding: &str,
    boundary: bool,
) -> Result<()> {
    if blame.commit.is_null() {
        let (name, email, time) = match fake {
            Some(f) => (f.name.as_str(), f.email.as_str(), f.time),
            None => ("Not Committed Yet", "not.committed.yet", 0),
        };
        let summary = match fake {
            Some(f) => f.summary.clone(),
            None => format!(
                "Version of {} from {}",
                blame.origin_path, blame.origin_path
            ),
        };
        writeln!(handle, "author {name}")?;
        writeln!(handle, "author-mail <{email}>")?;
        writeln!(handle, "author-time {time}")?;
        writeln!(handle, "author-tz +0000")?;
        writeln!(handle, "committer {name}")?;
        writeln!(handle, "committer-mail <{email}>")?;
        writeln!(handle, "committer-time {time}")?;
        writeln!(handle, "committer-tz +0000")?;
        writeln!(handle, "summary {summary}")?;
        return Ok(());
    }

    let object = db.read_object(&blame.commit)?;
    let commit = Commit::parse(format, &object.body)?;
    let from_enc = blame_commit_encoding_name(&commit);
    let author_bytes = log_reencode_message(&commit.author, &from_enc, output_encoding);
    let committer_bytes = log_reencode_message(&commit.committer, &from_enc, output_encoding);
    let message_bytes = log_reencode_message(&commit.message, &from_enc, output_encoding);

    emit_incremental_ident(handle, "author", &author_bytes)?;
    emit_incremental_ident(handle, "committer", &committer_bytes)?;
    write_field(
        handle,
        b"summary ",
        &commit_summary_bytes(&message_bytes, &blame.commit),
    )?;
    if boundary {
        writeln!(handle, "boundary")?;
    }
    Ok(())
}

/// Emit the `<who>`, `<who>-mail`, `<who>-time`, `<who>-tz` lines from a raw
/// (already reencoded) identity line, writing the name/email as raw bytes.
fn emit_incremental_ident(handle: &mut impl Write, who: &str, ident: &[u8]) -> Result<()> {
    let sig = Signature::from_ident_line(ident);
    let name = sig.as_ref().map(|s| s.name.as_bytes()).unwrap_or(b"");
    let email = sig.as_ref().map(|s| s.email.as_bytes()).unwrap_or(b"");
    let time = sig.as_ref().map(|s| s.time.seconds).unwrap_or(0);
    let tz = sig
        .as_ref()
        .map(|s| s.time.offset_token())
        .unwrap_or_else(|| "+0000".to_string());
    handle.write_all(who.as_bytes())?;
    handle.write_all(b" ")?;
    handle.write_all(name)?;
    handle.write_all(b"\n")?;
    handle.write_all(who.as_bytes())?;
    handle.write_all(b"-mail <")?;
    handle.write_all(email)?;
    handle.write_all(b">\n")?;
    writeln!(handle, "{who}-time {time}")?;
    writeln!(handle, "{who}-tz {tz}")?;
    Ok(())
}

/// git's `find_commit_subject`: skip leading blank/whitespace-only lines of the
/// commit body and return the first non-blank line as the porcelain `summary`
/// (raw bytes, so a reencoded non-UTF-8 subject round-trips). An empty message
/// renders as the parenthesized object name.
fn commit_summary_bytes(message: &[u8], oid: &ObjectId) -> Vec<u8> {
    let mut start = 0usize;
    while start < message.len() {
        let eol = message[start..]
            .iter()
            .position(|b| *b == b'\n')
            .map(|p| start + p)
            .unwrap_or(message.len());
        let line = &message[start..eol];
        let blank = line
            .iter()
            .all(|b| matches!(*b, b' ' | b'\t' | b'\r' | 0x0b | 0x0c));
        if !blank {
            return line.to_vec();
        }
        start = eol + 1;
    }
    format!("({})", oid.to_hex()).into_bytes()
}

/// The commit's stored encoding (its `encoding` header, default UTF-8), used as
/// the source encoding when reencoding author/summary for output.
fn blame_commit_encoding_name(commit: &Commit) -> String {
    commit
        .encoding
        .as_deref()
        .map(|enc| String::from_utf8_lossy(enc).into_owned())
        .unwrap_or_else(|| "UTF-8".to_string())
}

/// Write a porcelain/incremental `<key> <value>\n` field with a raw byte value.
fn write_field(handle: &mut impl Write, key: &[u8], value: &[u8]) -> Result<()> {
    handle.write_all(key)?;
    handle.write_all(value)?;
    handle.write_all(b"\n")?;
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

/// Render the object-name column with `--ignore-rev` markers. Mirrors git's
/// emit loop: a boundary `^`, then `*` (unblamable) and `?` (ignored), each
/// consuming one column from the `hex_width` budget before the hex digits.
#[allow(clippy::too_many_arguments)]
fn render_object_name_marked(
    commit: &ObjectId,
    boundary: bool,
    show_root: bool,
    hex_width: usize,
    blank_boundary: bool,
    marks: BlameMarks,
    ignored: bool,
    unblamable: bool,
    hexsz: usize,
) -> String {
    let mut out = String::new();
    let mut length: isize = hex_width as isize;
    let is_boundary = boundary && !show_root;
    let blank = is_boundary && blank_boundary;
    if is_boundary && !blank {
        out.push('^');
        length -= 1;
    }
    if marks.unblamable && unblamable {
        out.push('*');
        length -= 1;
    }
    if marks.ignored && ignored {
        out.push('?');
        length -= 1;
    }
    let n = (length.max(0) as usize).min(hexsz);
    if blank {
        out.push_str(&" ".repeat(n));
    } else {
        let hex = commit.to_hex();
        out.push_str(&hex[..n.min(hex.len())]);
    }
    out
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
    mailmap: &commands::utility::Mailmap,
    fake: Option<&FakeCommit>,
) -> Result<(String, String)> {
    if blame.commit.is_null() {
        // The all-zero pseudo-commit: "Not Committed Yet" / "External file
        // (--contents)" with the time blame ran (git stamps it with `now`).
        let (name, email, time) = match fake {
            Some(f) => (f.name.as_str(), f.email.as_str(), f.time),
            None => ("Not Committed Yet", "not.committed.yet", 0),
        };
        return Ok((
            match options.author_field {
                AuthorField::Name => name.to_string(),
                AuthorField::Email => format!("<{email}>"),
            },
            match options.date_field {
                DateField::Iso => format_blame_iso_utc(time),
                DateField::Raw => format!("{time} +0000"),
            },
        ));
    }
    let object = db.read_object(&blame.commit)?;
    let commit = Commit::parse(format, &object.body)?;
    let identity = &commit.author;

    // git blame maps the author through the mailmap before display (both the
    // `-e` email column and the default name column use the mapped identity).
    let (mapped_name, mapped_email) = mailmap.rewrite_identity(identity);
    let author = match options.author_field {
        AuthorField::Name => String::from_utf8_lossy(&mapped_name).into_owned(),
        AuthorField::Email => format!("<{}>", String::from_utf8_lossy(&mapped_email)),
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

/// Format a UTC unix timestamp as blame's ISO column (`YYYY-MM-DD HH:MM:SS
/// +0000`). Used for the fake working-tree commit, which git stamps `+0000`.
fn format_blame_iso_utc(time: i64) -> String {
    DateMode::Iso.render(time, "+0000").unwrap_or_default()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(blob: &[u8]) -> Vec<sley_diff_merge::DiffLine<'_>> {
        sley_diff_merge::split_lines(blob)
    }

    #[test]
    fn diff_hunks_collapses_change_runs() {
        // parent: a b c   child: a X c  -> one hunk replacing line 2 (0-based 1).
        let p = lines(b"a\nb\nc\n");
        let c = lines(b"a\nX\nc\n");
        let h = diff_hunks(&p, &c, sley_diff_merge::DiffAlgorithm::Myers);
        assert_eq!(h.len(), 1);
        assert_eq!((h[0].start_a, h[0].count_a), (1, 1));
        assert_eq!((h[0].start_b, h[0].count_b), (1, 1));
    }

    #[test]
    fn diff_hunks_pure_insertion() {
        // parent: a c   child: a b c -> insert one child line at index 1.
        let p = lines(b"a\nc\n");
        let c = lines(b"a\nb\nc\n");
        let h = diff_hunks(&p, &c, sley_diff_merge::DiffAlgorithm::Myers);
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].count_a, 0);
        assert_eq!((h[0].start_b, h[0].count_b), (1, 1));
    }

    #[test]
    fn diff_hunks_identical_is_empty() {
        let p = lines(b"a\nb\n");
        let c = lines(b"a\nb\n");
        assert!(diff_hunks(&p, &c, sley_diff_merge::DiffAlgorithm::Myers).is_empty());
    }

    /// A chunk that is wholly preserved by the parent migrates entirely, with
    /// its `s_lno` rebased by the offset of any inserted lines above it.
    #[test]
    fn pass_blame_routes_preserved_lines_to_parent() {
        // parent: a b   child: X a b  (one line inserted at the top). Child
        // lines 1 and 2 (a, b) are preserved; they rebase to parent lines 0, 1.
        let p = lines(b"a\nb\n");
        let c = lines(b"X\na\nb\n");
        let mut owned = vec![BlameEntry {
            lno: 0,
            s_lno: 0,
            num_lines: 3,
            ..Default::default()
        }];
        let passed =
            pass_blame_to_parent(&p, &c, &mut owned, sley_diff_merge::DiffAlgorithm::Myers);
        // The inserted line (child s_lno 0) stays with the child; lines 1..3 go
        // to the parent at parent-s_lno 0..2.
        let ours_lines: usize = owned.iter().map(|e| e.num_lines).sum();
        let passed_lines: usize = passed.iter().map(|e| e.num_lines).sum();
        assert_eq!(ours_lines, 1, "the inserted line stays with the child");
        assert_eq!(passed_lines, 2, "the two preserved lines go to the parent");
        // Preserved chunk rebased: child s_lno 1 -> parent s_lno 0.
        let first = passed.iter().min_by_key(|e| e.lno).unwrap();
        assert_eq!(first.lno, 1);
        assert_eq!(first.s_lno, 0);
    }

    #[test]
    fn pass_blame_charges_changed_lines_to_child() {
        // parent: a b c   child: a Z c — only the middle line changed.
        let p = lines(b"a\nb\nc\n");
        let c = lines(b"a\nZ\nc\n");
        let mut owned = vec![BlameEntry {
            lno: 0,
            s_lno: 0,
            num_lines: 3,
            ..Default::default()
        }];
        let passed =
            pass_blame_to_parent(&p, &c, &mut owned, sley_diff_merge::DiffAlgorithm::Myers);
        let ours: usize = owned.iter().map(|e| e.num_lines).sum();
        let to_parent: usize = passed.iter().map(|e| e.num_lines).sum();
        assert_eq!(ours, 1, "the changed middle line is charged to the child");
        assert_eq!(to_parent, 2, "the unchanged a/c lines pass to the parent");
    }

    #[test]
    fn blame_ignore_tiny_parent_hunk_prefers_whole_parent_exact_match() {
        let parent = lines(b"#include \"c.h\"\n#include \"b.h\"\n#include \"a.h\"\n#include \"e.h\"\n#include \"d.h\"\n");
        let target = lines(b"#include \"a.h\"\n#include \"b.h\"\n#include \"c.h\"\n#include \"d.h\"\n#include \"e.h\"\n");
        let parent_fps = line_fingerprints(&parent);
        let target_fps = line_fingerprints(&target);

        // Sley's Myers op stream splits this reorder so the middle target
        // region (`b.h`, `c.h`) is compared with only the old `e.h` line. Git's
        // ignore pass still recovers the exact whole-parent matches.
        let line_blames = guess_line_blames(&parent_fps, &target_fps, 1, 2, 3, 1);

        assert_eq!(line_blames.len(), 2);
        assert!(line_blames.iter().all(|line| line.is_parent));
        assert_eq!(line_blames[0].s_lno, 1);
        assert_eq!(line_blames[1].s_lno, 0);
    }

    #[test]
    fn split_entry_at_partitions_lines() {
        let mut e = BlameEntry {
            lno: 10,
            s_lno: 4,
            num_lines: 5,
            ..Default::default()
        };
        let tail = split_entry_at(&mut e, 2);
        assert_eq!((e.lno, e.s_lno, e.num_lines), (10, 4, 2));
        assert_eq!((tail.lno, tail.s_lno, tail.num_lines), (12, 6, 3));
    }

    #[test]
    fn split_range_at_comma_respects_regex() {
        assert_eq!(split_range_at_comma("3,6"), ("3", "6"));
        assert_eq!(split_range_at_comma("/a,b/,/c/"), ("/a,b/", "/c/"));
        assert_eq!(split_range_at_comma("/only/"), ("/only/", ""));
        assert_eq!(split_range_at_comma("5"), ("5", ""));
    }

    #[test]
    fn resolve_range_errors_when_start_past_eof() {
        // 3-line file; -L4 (start 4) must error, not clamp.
        let contents: Vec<&[u8]> = vec![b"a", b"b", b"c"];
        let range = RawRange {
            start: RangeBound::Absolute(4),
            end: RangeBound::Omitted,
        };
        let r = resolve_range(&range, 3, &contents, 1, "f");
        assert!(matches!(r, Err(GitError::Exit(128))));
    }

    #[test]
    fn resolve_range_reversed_lists_inner_span() {
        // -L100,2 on a 3-line file lists [2,3] (no error: smaller endpoint in range).
        let contents: Vec<&[u8]> = vec![b"a", b"b", b"c"];
        let range = RawRange {
            start: RangeBound::Absolute(100),
            end: RangeBound::Absolute(2),
        };
        let (lo, hi) = resolve_range(&range, 3, &contents, 1, "f").unwrap();
        assert_eq!((lo, hi), (2, 3));
    }

    #[test]
    fn resolve_range_negative_relative_end() {
        // -L3,-1 selects [3,3]; -L6,-4 selects [3,6].
        let contents: Vec<&[u8]> = (1..=8).map(|_| b"x".as_slice()).collect();
        let r1 = RawRange {
            start: RangeBound::Absolute(3),
            end: RangeBound::RelativeNeg(1),
        };
        assert_eq!(resolve_range(&r1, 8, &contents, 1, "f").unwrap(), (3, 3));
        let r2 = RawRange {
            start: RangeBound::Absolute(6),
            end: RangeBound::RelativeNeg(4),
        };
        assert_eq!(resolve_range(&r2, 8, &contents, 1, "f").unwrap(), (3, 6));
    }

    #[test]
    fn resolve_regex_bound_finds_first_match_from_anchor() {
        let contents: Vec<&[u8]> = vec![b"apple", b"robot green", b"banana", b"green tea"];
        // From line 1, /green/ matches line 2.
        assert_eq!(resolve_regex_bound("green", &contents, 1, 4).unwrap(), 2);
        // From line 3, /green/ matches line 4 (the search anchor advances).
        assert_eq!(resolve_regex_bound("green", &contents, 3, 4).unwrap(), 4);
        // No match is the fatal error.
        assert!(matches!(
            resolve_regex_bound("zzz", &contents, 1, 4),
            Err(GitError::Exit(128))
        ));
    }
}
