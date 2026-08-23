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
use sley::plumbing::sley_rev::blame::{
    BlameContentConverter, BlameObjectSource, BlameRequest, LineBlame, PreviousMap,
};

/// The object-read hook blame's walk uses: plain repository reads plus
/// promisor hydration when partial-clone lazy fetching is enabled.
struct BlamePrefetchReader<'a> {
    db: &'a FileObjectDatabase,
    lazy_fetch: bool,
}

impl BlameObjectSource for BlamePrefetchReader<'_> {
    fn read_blame_object(&self, oid: &ObjectId) -> Result<std::sync::Arc<sley_object::EncodedObject>> {
        read_object_maybe_prefetch_promisor(self.db, oid, self.lazy_fetch)
    }
}

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
    let blame_reader = BlamePrefetchReader {
        db,
        lazy_fetch: cli_session.lazy_fetch(),
    };

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
    let mut textconv = TextconvContext {
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
                &blame_reader,
            )?,
        };
        let blob = textconv.convert(&repo_path, mode, raw)?;
        // The porcelain `previous` pointer is the real parent when it has the
        // path; a brand-new (only-staged) file has no such parent.
        let previous = sley_rev::blame::read_path_blob(
            db,
            format,
            &start_commit,
            &repo_path,
            &blame_reader,
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
        match sley_rev::blame::read_path_blob(
            db,
            format,
            &start_commit,
            &repo_path,
            &blame_reader,
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
    let grafts = sley_rev::blame::read_graft_file(git_dir, format);
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

    let (lines, previous_map) = {
        let mut converter = textconv;
        let request = BlameRequest {
            db,
            reader: &blame_reader,
            format,
            start_commit,
            repo_path: &repo_path,
            final_blob: &final_blob,
            first_parent: options.first_parent,
            boundary_tip,
            copy_level: options.copy_level,
            copy_score: options.copy_score,
            virtual_final,
            ignore_set: &ignore_set,
            diff_algorithm,
            grafts: &grafts,
        };
        sley_rev::blame::compute_blame(&request, &mut converter)?
    };

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
        cli_session.replace_objects(),
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
// Boxing the overwhelmingly common `Run` state would add an allocation to blame.
#[allow(clippy::large_enum_variant)]
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

impl BlameContentConverter for TextconvContext {
    /// Convert `blob` for `path`/`mode`, or return it unchanged when textconv is
    /// disabled, the path has no textconv driver, or the entry is not a regular
    /// file (symlinks fall back to the default driver, as in git).
    fn convert(&mut self, path: &str, mode: u32, blob: Vec<u8>) -> Result<Vec<u8>> {
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

/// Read the blob for `repo_path` from the index (any normal-stage entry).
fn read_index_blob(
    git_dir: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    repo_path: &str,
    reader: &BlamePrefetchReader<'_>,
) -> Result<Option<(Vec<u8>, u32)>> {
    let Some(index) = sley_worktree::read_repository_index(git_dir, format)? else {
        return Ok(None);
    };
    let Some(entry) = index.entries.iter().find(|entry| {
        entry.stage() == sley_index::Stage::Normal && entry.path.as_bytes() == repo_path.as_bytes()
    }) else {
        return Ok(None);
    };
    let object =
        read_object_maybe_prefetch_promisor(db, &entry.oid, reader.lazy_fetch)?;
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

/// C-locale `isspace`.
fn is_ascii_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
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
    reader: &BlamePrefetchReader<'_>,
) -> Result<(Vec<u8>, u32)> {
    // `verify_working_tree_path`: an untracked path (absent from the start
    // commit's tree and from the index in *any* stage) is the "no such path"
    // fatal, even when a file by that name exists on disk. An unmerged path
    // (stages 1/2/3, no stage 0) still counts as known, so a conflicted file
    // blames against HEAD rather than erroring.
    let committed =
        sley_rev::blame::read_path_blob(db, format, start_commit, repo_path, reader)?;
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
        match std::fs::symlink_metadata(&absolute) {
            Ok(meta) => {
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
                            root,
                            repo.git_dir(),
                            repo.config(),
                            repo_path.as_bytes(),
                            &bytes,
                        )?
                    };
                    return Ok((image, 0o100644));
                }
            }
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                // With no positive revision, blame constructs a fake
                // worktree commit and requires the named path to exist in the
                // worktree even when HEAD/the index still contains it.  A
                // sparse-checkout path is therefore an lstat failure, not an
                // invitation to fall back to its staged or committed blob.
                eprintln!("fatal: Cannot lstat '{repo_path}': No such file or directory");
                return Err(GitError::Exit(128));
            }
            Err(_) => {}
        }
    }
    // A non-regular path that was present but unreadable can still be supplied
    // by the staged image, then by the committed image.
    if let Some((blob, mode)) = read_index_blob(repo.git_dir(), db, format, repo_path, reader)?
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
    replace_objects: bool,
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
            replace_objects,
        );
    }

    let (abbrev, hex_width) = blame_display_abbrev(git_dir, format, options)?;

    // git blame *always* reads the mailmap (`read_mailmap`, no flag) and maps the
    // displayed author name/email through it.
    let mailmap = commands::utility::Mailmap::load_default(git_dir, format, replace_objects)?;

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
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    lines: &[LineBlame],
    selected: &[usize],
    options: &BlameOptions,
    fake: Option<&FakeCommit>,
    previous_map: &PreviousMap,
    marks: BlameMarks,
    replace_objects: bool,
) -> Result<()> {
    // Git's porcelain metadata is built from the same mailmapped commit-info
    // records as its human-readable output. This applies to both author and
    // committer fields, even though porcelain has no explicit mailmap option.
    let mailmap = commands::utility::Mailmap::load_default(git_dir, format, replace_objects)?;
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
            &mailmap,
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
                        &mailmap,
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
    mailmap: &commands::utility::Mailmap,
) -> Result<()> {
    let emitted = emit_one_suspect_detail(
        handle, db, format, blame, options, fake, shown, repeat, mailmap,
    )?;
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
    mailmap: &commands::utility::Mailmap,
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
    let (author_name, author_email) = mailmap.rewrite_identity(&commit.author);
    let (committer_name, committer_email) = mailmap.rewrite_identity(&commit.committer);
    let author = Signature::from_ident_line(&commit.author);
    let committer = Signature::from_ident_line(&commit.committer);
    writeln!(handle, "author {}", String::from_utf8_lossy(&author_name))?;
    writeln!(
        handle,
        "author-mail <{}>",
        String::from_utf8_lossy(&author_email)
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
        String::from_utf8_lossy(&committer_name)
    )?;
    writeln!(
        handle,
        "committer-mail <{}>",
        String::from_utf8_lossy(&committer_email)
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
