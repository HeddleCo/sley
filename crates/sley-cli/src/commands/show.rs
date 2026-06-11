//! `git show` — display one or more objects (commits, tags, trees, blobs).
//!
//! A commit is rendered as its header (the `medium` pretty format by default:
//! `commit`/`Author:`/`Date:` plus the indented message) followed by the patch
//! between the commit and its first parent — the same diff machinery `git diff`
//! and `git stash show` use. Annotated tags print a `tag`/`Tagger:`/`Date:`
//! block and then recurse into the tagged object; trees list their immediate
//! entries; blobs are emitted verbatim.
//!
//! Output formatting is shared with the rest of the CLI through a glob of the
//! crate root, which (because a submodule can reach its ancestor module's
//! private items) brings every helper and type — `discover_git_dir`,
//! `repository_object_format`, `resolve_revision`, `read_repo_config`,
//! `FileObjectDatabase`, `FileRefStore`, the `write_diff_*` writers, the
//! `print_log_format` pretty-format engine, and so on — into scope without
//! re-listing them.

use crate::*;
use sley_object::TreeEntries;

/// How the per-object diff (for commits) is rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShowDiffMode {
    /// Default `git show` patch output (unified diff).
    Patch,
    /// `--name-only`: just the changed paths.
    NameOnly,
    /// `--name-status`: status letter + path.
    NameStatus,
    /// `-s`/`--no-patch`: suppress all diff output (header/message only).
    None,
}

/// How a commit's header/message is rendered.
#[derive(Debug, Clone)]
enum ShowCommitFormat {
    /// `medium` (the default): `commit`/`Author:`/`Date:` + indented message.
    Medium,
    /// `--oneline`: `<abbrev-oid> <subject>`.
    Oneline,
    /// `--pretty=oneline`/`--format=oneline`: `<full-oid> <subject>`.
    FullOneline,
    /// `--format=<string>` / `--pretty=format:<string>`. `final_newline`
    /// distinguishes `--format=` (trailing newline) from `--pretty=format:`
    /// (separator-style, newline only between entries).
    Custom {
        compiled: CompiledLogFormat,
        final_newline: bool,
    },
}

/// Parsed `git show` invocation state.
struct ShowOptions {
    commit_format: ShowCommitFormat,
    diff_mode: ShowDiffMode,
    /// Whether the `commit <oid>` line (medium/full-oneline) uses an abbreviated
    /// oid. `--oneline` always abbreviates regardless of this flag.
    abbrev_commit: bool,
    /// Abbreviation width for commit/tree/blob oids in headers and `%h`/`%t`/`%p`.
    abbrev_len: Option<usize>,
    /// Show stat output before/instead of the patch.
    stat: bool,
    /// Compact-summary variant of `--stat`.
    compact_summary: bool,
    /// `--numstat` machine-readable stat.
    numstat: bool,
    /// `--shortstat` one-line summary.
    shortstat: bool,
    /// `--summary` create/delete/rename summary lines.
    summary: bool,
    /// `--raw` diff output.
    raw: bool,
    /// Full 40/64-hex `index` lines in patches (`--full-index`).
    patch_full_index: bool,
    /// Explicit patch abbreviation width (`--abbrev=<n>` affects this too).
    patch_abbrev: Option<usize>,
    /// Rename detection toggle (on by default, `--no-renames` disables).
    detect_renames: bool,
    /// Copy detection (`-C`/`--find-copies`).
    detect_copies: bool,
    /// `-C` with `--find-copies-harder`.
    find_copies_harder: bool,
    /// Rename/copy similarity threshold.
    rename_threshold: u8,
    /// Copy similarity threshold.
    copy_threshold: u8,
    /// Date rendering mode for the `Date:` line and `%ad`/`%cd`.
    date_mode: ForEachRefDateMode,
    /// Ref decoration mode for the `commit` header and `%d`/`%D`. `git show`
    /// defaults to off; `--decorate`/`--decorate=<mode>` enables it.
    decorate: LogDecorationMode,
    /// Object/revision arguments to show.
    specs: Vec<String>,
}

struct ShowContext<'a> {
    git_dir: &'a Path,
    db: &'a FileObjectDatabase,
    format: ObjectFormat,
    config: &'a GitConfig,
    options: &'a ShowOptions,
    decorations: &'a HashMap<ObjectId, Vec<String>>,
}

struct CommitTrailerLayout {
    text_self_terminated: bool,
    blank_before_diff: bool,
    separator_mode: bool,
    is_merge: bool,
}

impl Default for ShowOptions {
    fn default() -> Self {
        Self {
            commit_format: ShowCommitFormat::Medium,
            diff_mode: ShowDiffMode::Patch,
            abbrev_commit: false,
            abbrev_len: Some(7),
            stat: false,
            compact_summary: false,
            numstat: false,
            shortstat: false,
            summary: false,
            raw: false,
            patch_full_index: false,
            patch_abbrev: None,
            detect_renames: true,
            detect_copies: false,
            find_copies_harder: false,
            rename_threshold: sley_diff_merge::DEFAULT_RENAME_THRESHOLD,
            copy_threshold: sley_diff_merge::DEFAULT_RENAME_THRESHOLD,
            date_mode: ForEachRefDateMode::Default,
            decorate: LogDecorationMode::Off,
            specs: Vec::new(),
        }
    }
}

impl ShowOptions {
    /// True when any non-patch diff sub-mode (stat/raw/etc.) is requested.
    fn has_diff_extras(&self) -> bool {
        self.stat
            || self.compact_summary
            || self.numstat
            || self.shortstat
            || self.summary
            || self.raw
    }

    /// `-s` / `--no-patch`: clear every diff-output selection. A later flag such
    /// as `--stat` can re-enable a specific sub-mode, matching real git's
    /// order-dependent behaviour.
    fn suppress_diff(&mut self) {
        self.diff_mode = ShowDiffMode::None;
        self.stat = false;
        self.compact_summary = false;
        self.numstat = false;
        self.shortstat = false;
        self.summary = false;
        self.raw = false;
    }

    /// Re-enable patch output after a diff sub-mode flag clears the `-s` state.
    fn restore_patch(&mut self) {
        if self.diff_mode == ShowDiffMode::None {
            self.diff_mode = ShowDiffMode::Patch;
        }
    }
}

pub(crate) fn cmd_show(args: &[String]) -> Result<()> {
    let options = parse_show_args(args)?;

    let repo = RepositoryContext::discover_current()?;
    let git_dir = repo.git_dir();
    let format = repo.format();
    let config = repo.config();
    let db = repo.objects();

    // Ref decorations feed the `commit`/oneline header and the `%d`/`%D`
    // placeholders. `git show` leaves them off unless `--decorate` is given, but a
    // custom format that references `%d`/`%D` auto-enables them (in short form
    // unless `--decorate=full`), mirroring `git log`.
    let decoration_mode = match &options.commit_format {
        ShowCommitFormat::Custom { compiled, .. }
            if options.decorate == LogDecorationMode::Off && compiled.uses_decorations() =>
        {
            LogDecorationMode::Short
        }
        _ => options.decorate,
    };
    let decorations: HashMap<ObjectId, Vec<String>> = if decoration_mode == LogDecorationMode::Off {
        HashMap::new()
    } else {
        log_decoration_map(git_dir, db, format, decoration_mode)?
    };

    let specs = if options.specs.is_empty() {
        vec!["HEAD".to_string()]
    } else {
        options.specs.clone()
    };

    let mut shown_one = false;
    let mut stdout = io::stdout();
    let context = ShowContext {
        git_dir,
        db,
        format,
        config,
        options: &options,
        decorations: &decorations,
    };
    for spec in &specs {
        let oid = match repo.resolve_revision(spec) {
            Ok(oid) => oid,
            Err(_) => return show_unknown_revision(spec),
        };
        show_object(&mut stdout, &context, spec, &oid, &mut shown_one, false)?;
    }
    stdout.flush()?;
    Ok(())
}

/// Resolve and display a single object reachable from `oid`, where `name` is the
/// literal command-line string used to reach it (echoed verbatim in `tree`/`tag`
/// headers, exactly like git's `obj->name`).
///
/// `shown_one` tracks whether a commit-or-tag header has already been emitted so
/// the inter-entry blank-line separator is inserted only before the second and
/// later such entries. `suppress_separator` is `true` when this object is the
/// immediate target of an annotated tag, whose block has already emitted the gap.
fn show_object(
    stdout: &mut io::Stdout,
    context: &ShowContext<'_>,
    name: &str,
    oid: &ObjectId,
    shown_one: &mut bool,
    suppress_separator: bool,
) -> Result<()> {
    let object = context.db.read_object(oid)?;
    match object.object_type {
        ObjectType::Commit => {
            let commit = Commit::parse(context.format, &object.body)?;
            show_commit(stdout, context, oid, &commit, shown_one, suppress_separator)
        }
        ObjectType::Tag => {
            let tag = Tag::parse(context.format, &object.body)?;
            show_tag_header(stdout, &tag, shown_one, suppress_separator)?;
            // Recurse into the tagged object, threading the *same* display name
            // through (git keeps `obj->name` from the original argument). The tag
            // block already supplied the gap line, so the target must not add its
            // own leading separator.
            show_object(stdout, context, name, &tag.object, shown_one, true)
        }
        ObjectType::Tree => show_tree(stdout, context.format, name, &object.body),
        ObjectType::Blob => {
            stdout.write_all(&object.body)?;
            Ok(())
        }
    }
}

/// Print a commit: header (per `commit_format`) then, unless suppressed, the
/// diff against its first parent (or the empty tree for a root commit).
///
/// Spacing follows git's `format` vs `tformat` model precisely:
/// - tformat (medium, oneline, `--format=`/`tformat:`): each entry is terminated
///   by a newline; a non-empty diff (or a merge's empty diff) is introduced by a
///   blank line.
/// - format (`--pretty=format:`): entries are separated by a blank line and have
///   no trailing newline; a diff is introduced by a single newline (no blank).
///
/// Merge commits print a `Merge:` line and, under git's default
/// `--diff-merges=off`, show no patch — but the format still emits the trailing
/// gap as if a diff had run. `suppress_separator` is set when this commit is the
/// immediate target of an annotated tag, whose block already supplied the gap.
fn show_commit(
    stdout: &mut io::Stdout,
    context: &ShowContext<'_>,
    oid: &ObjectId,
    commit: &Commit,
    shown_one: &mut bool,
    suppress_separator: bool,
) -> Result<()> {
    let options = context.options;
    let decorations = context.decorations;
    let record = sley_rev::CommitRecord {
        oid: *oid,
        parents: commit.parents.clone(),
        commit: commit.clone(),
    };
    let is_merge = commit.parents.len() > 1;
    // Spacing classification, derived empirically from git for each pretty format:
    //
    // - `separator_mode`: a blank line is placed *before* the second and later
    //   entries (medium and `--pretty=format:`); the other formats instead
    //   terminate each entry, so consecutive entries abut.
    // - `text_self_terminated`: the commit-text block ends with its own newline
    //   (medium and the oneline arms use `writeln!`; the custom-format engine
    //   does not terminate).
    // - `blank_before_diff`: the diff is introduced by a blank line (medium and
    //   `--format=`/tformat); `--oneline` and `--pretty=format:` butt the diff
    //   straight against the text line.
    let separator_mode = matches!(
        options.commit_format,
        ShowCommitFormat::Medium
            | ShowCommitFormat::Custom {
                final_newline: false,
                ..
            }
    );
    let text_self_terminated = !matches!(options.commit_format, ShowCommitFormat::Custom { .. });
    let blank_before_diff = matches!(
        options.commit_format,
        ShowCommitFormat::Medium
            | ShowCommitFormat::Custom {
                final_newline: true,
                ..
            }
    );

    // Leading inter-entry separator (separator-mode only). A tag parent already
    // supplied the gap, so honour `suppress_separator`.
    if separator_mode && *shown_one && !suppress_separator {
        writeln!(stdout)?;
    }

    match &options.commit_format {
        ShowCommitFormat::Medium => {
            write!(
                stdout,
                "commit {}",
                format_log_commit_header_oid(oid, options.abbrev_commit, options.abbrev_len)
            )?;
            print_log_decorations(oid, decorations);
            writeln!(stdout)?;
            if is_merge {
                let abbrev = merge_line_abbrev(options);
                let parents = commit
                    .parents
                    .iter()
                    .map(|parent| format_log_oid(parent, abbrev))
                    .collect::<Vec<_>>()
                    .join(" ");
                writeln!(stdout, "Merge: {parents}")?;
            }
            writeln!(stdout, "Author: {}", commit_author_identity(&commit.author))?;
            writeln!(
                stdout,
                "Date:   {}",
                commit_identity_date(&commit.author, options.date_mode)
            )?;
            writeln!(stdout)?;
            for line in String::from_utf8_lossy(&commit.message).lines() {
                writeln!(stdout, "    {line}")?;
            }
        }
        ShowCommitFormat::Oneline => {
            write!(stdout, "{}", format_log_oid(oid, options.abbrev_len))?;
            print_log_decorations(oid, decorations);
            writeln!(stdout, " {}", commit_subject(&commit.message))?;
        }
        ShowCommitFormat::FullOneline => {
            write!(
                stdout,
                "{}",
                format_log_commit_header_oid(oid, options.abbrev_commit, options.abbrev_len)
            )?;
            print_log_decorations(oid, decorations);
            writeln!(stdout, " {}", commit_subject(&commit.message))?;
        }
        ShowCommitFormat::Custom { compiled, .. } => {
            print_log_format(
                &record,
                compiled,
                LogFormatContext {
                    abbrev_len: options.abbrev_len,
                    decorations,
                    marker: '>',
                    dialect: LogFormatDialect::Log,
                    source: None,
                    date_mode: options.date_mode,
                    source_oid: None,
                    describe: None,
                    color: false,
                    output_encoding: "UTF-8",
                },
            )?;
        }
    }
    *shown_one = true;

    // Every format — including `--oneline` — still shows the patch (this is
    // `git show`, which defaults to a diff). The first-parent diff (empty-tree for
    // a root) is computed for merges too, because git's default renders the stat
    // family for them even though the patch/raw/name listings are suppressed.
    let entries = commit_diff_entries(context.db, context.format, options, commit)?;

    write_commit_trailer(
        stdout,
        context,
        CommitTrailerLayout {
            text_self_terminated,
            blank_before_diff,
            separator_mode,
            is_merge,
        },
        &entries,
    )
}

/// Emit the gap and diff body after a commit's text, implementing the precise
/// per-format spacing observed from git (see [`show_commit`] for the meaning of
/// `text_self_terminated`, `blank_before_diff`, and `separator_mode`).
///
/// `is_merge` selects git's merge handling, where the default `--diff-merges=off`
/// suppresses the patch / raw / name listings but still renders the `--stat`
/// family against the first parent.
fn write_commit_trailer(
    stdout: &mut io::Stdout,
    context: &ShowContext<'_>,
    layout: CommitTrailerLayout,
    entries: &[sley_diff_merge::NameStatusEntry],
) -> Result<()> {
    let options = context.options;
    let diff_active = options.diff_mode != ShowDiffMode::None;
    // For a merge, only the stat family renders; for an ordinary commit every
    // mode renders when there are changes.
    let body_renders = if layout.is_merge {
        diff_active && merge_renders_stat(options) && !entries.is_empty()
    } else {
        diff_active && !entries.is_empty()
    };
    // `--pretty=format:` is the only mode that leaves its text line unterminated
    // when no diff follows.
    let format_unterminated = !layout.text_self_terminated && layout.separator_mode;

    if body_renders {
        // Close the commit-text line if the format left it open, then, for the
        // formats that use one, add the blank line separating message from diff.
        if !layout.text_self_terminated {
            writeln!(stdout)?;
        }
        if layout.blank_before_diff {
            writeln!(stdout)?;
        }
        return if layout.is_merge {
            write_merge_stat(stdout, context.db, context.config, options, entries)
        } else {
            write_commit_diff(
                stdout,
                context.git_dir,
                context.db,
                context.format,
                context.config,
                options,
                entries,
            )
        };
    }

    // No diff body. A merge whose diff is active still prints the trailing gap as
    // if a diff had run: a blank line for every format except `--pretty=format:`,
    // which only closes its text line.
    if layout.is_merge && diff_active {
        if !layout.text_self_terminated {
            writeln!(stdout)?;
        }
        if !format_unterminated {
            writeln!(stdout)?;
        }
        return Ok(());
    }

    // Otherwise there is nothing after the text. Terminator-mode formats whose
    // text was left open (`--format=`) still need a closing newline; medium and
    // oneline already ended their line, and `--pretty=format:` keeps none.
    if !layout.text_self_terminated && !layout.separator_mode {
        writeln!(stdout)?;
    }
    Ok(())
}

/// Whether the active diff mode is one of the stat-family outputs git still
/// renders for merge commits (`--stat`, `--shortstat`, `--numstat`, `--summary`,
/// and the compact-summary variant).
fn merge_renders_stat(options: &ShowOptions) -> bool {
    options.diff_mode == ShowDiffMode::Patch
        && (options.stat
            || options.compact_summary
            || options.shortstat
            || options.numstat
            || options.summary)
}

/// Render the stat-family output for a merge commit's first-parent diff. The
/// patch and raw listings git suppresses for merges are intentionally omitted.
fn write_merge_stat(
    stdout: &mut io::Stdout,
    db: &FileObjectDatabase,
    config: &GitConfig,
    options: &ShowOptions,
    entries: &[sley_diff_merge::NameStatusEntry],
) -> Result<()> {
    let color = diff_color_enabled(config);
    if options.numstat {
        for entry in entries {
            write_diff_numstat_entry(stdout, entry, false, db, None, false)?;
        }
    }
    if options.stat || options.compact_summary {
        write_diff_stat(
            stdout,
            entries,
            db,
            None,
            false,
            DiffStatOptions {
                compact_summary: options.compact_summary,
                stat_count: None,
                color,
            },
        )?;
    }
    if options.shortstat {
        write_diff_shortstat(stdout, entries, db, None, false)?;
    }
    if options.summary {
        for entry in entries {
            write_diff_summary_entry(stdout, entry)?;
        }
    }
    Ok(())
}

/// Abbreviation width for the `Merge:` parent list. git abbreviates these to the
/// repository's default abbreviation length regardless of `--abbrev-commit`.
fn merge_line_abbrev(options: &ShowOptions) -> Option<usize> {
    options.abbrev_len.or(Some(7))
}

/// Build the name-status entry list for a commit's diff against its first parent
/// (or the empty tree for a root commit), honouring rename/copy detection flags.
fn commit_diff_entries(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    options: &ShowOptions,
    commit: &Commit,
) -> Result<Vec<sley_diff_merge::NameStatusEntry>> {
    let base = sley_diff_merge::DiffNameStatusOptions {
        detect_renames: options.detect_renames,
        detect_copies: options.detect_copies,
        find_copies_harder: options.find_copies_harder,
        rename_empty: true,
    };
    let rename_options = sley_diff_merge::RenameDetectionOptions {
        base,
        detect_inexact: true,
        rename_threshold: options.rename_threshold,
        copy_threshold: options.copy_threshold,
    };
    match commit.parents.first() {
        Some(parent_oid) => {
            let parent_object = db.read_object(parent_oid)?;
            let parent_commit = Commit::parse_ref(format, &parent_object.body)?;
            sley_diff_merge::diff_name_status_trees_with_rename_options(
                db,
                format,
                &parent_commit.tree,
                &commit.tree,
                rename_options,
            )
        }
        None => sley_diff_merge::diff_name_status_empty_tree_with_rename_options(
            db,
            format,
            &commit.tree,
            rename_options,
        ),
    }
}

/// Emit the diff body (stat/raw/summary/numstat/shortstat/patch or the
/// name-only / name-status listings) for a commit. The caller
/// ([`write_commit_trailer`]) has already written the gap line that precedes
/// this body. Reads both old and new blob content from the ODB (tree-to-tree
/// diff, never the worktree).
fn write_commit_diff(
    stdout: &mut io::Stdout,
    git_dir: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    config: &GitConfig,
    options: &ShowOptions,
    entries: &[sley_diff_merge::NameStatusEntry],
) -> Result<()> {
    match options.diff_mode {
        ShowDiffMode::None => Ok(()),
        ShowDiffMode::NameOnly => {
            for entry in entries {
                writeln!(stdout, "{}", status_quote_path(&entry.path, false))?;
            }
            Ok(())
        }
        ShowDiffMode::NameStatus => {
            for entry in entries {
                write!(stdout, "{}", entry.status.label())?;
                if let Some(old_path) = &entry.old_path {
                    let old_path = status_quote_path(old_path, false);
                    write!(stdout, "\t{old_path}")?;
                }
                let path = status_quote_path(&entry.path, false);
                writeln!(stdout, "\t{path}")?;
            }
            Ok(())
        }
        ShowDiffMode::Patch => {
            write_commit_diff_patch(stdout, git_dir, db, format, config, options, entries)
        }
    }
}

/// The stat/raw/summary/patch family, mirroring `git diff`'s ordering: any
/// requested machine/stat sub-modes first, then a blank line, then the unified
/// patch (unless an explicit sub-mode replaced it).
fn write_commit_diff_patch(
    stdout: &mut io::Stdout,
    git_dir: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    config: &GitConfig,
    options: &ShowOptions,
    entries: &[sley_diff_merge::NameStatusEntry],
) -> Result<()> {
    let repository_abbrev = repository_abbrev(git_dir, format)?;
    let raw_abbrev = repository_abbrev;
    let patch_abbrev = if options.patch_full_index {
        format.hex_len()
    } else {
        options
            .patch_abbrev
            .or(repository_abbrev)
            .unwrap_or(7)
            .min(format.hex_len())
    };
    let color = diff_color_enabled(config);

    let show_stat = options.stat || options.compact_summary;
    let show_patch = !options.has_diff_extras();
    let mut wrote_prefix = false;

    if entries.is_empty() {
        return Ok(());
    }

    if options.raw {
        for entry in entries {
            write_diff_raw_entry(stdout, entry, false, false, raw_abbrev, format)?;
        }
        wrote_prefix = true;
    }
    if options.numstat {
        for entry in entries {
            write_diff_numstat_entry(stdout, entry, false, db, None, false)?;
        }
        wrote_prefix = true;
    }
    if show_stat {
        write_diff_stat(
            stdout,
            entries,
            db,
            None,
            false,
            DiffStatOptions {
                compact_summary: options.compact_summary,
                stat_count: None,
                color,
            },
        )?;
        wrote_prefix = true;
    }
    if options.shortstat {
        write_diff_shortstat(stdout, entries, db, None, false)?;
        wrote_prefix = true;
    }
    if options.summary {
        for entry in entries {
            write_diff_summary_entry(stdout, entry)?;
        }
        wrote_prefix = true;
    }
    if show_patch {
        if wrote_prefix {
            writeln!(stdout)?;
        }
        for entry in entries {
            let patch_options = DiffPatchOptions {
                db,
                worktree_root: None,
                use_worktree_new: false,
                format,
                abbrev: patch_abbrev,
                src_prefix: "a/",
                dst_prefix: "b/",
            };
            write_diff_patch_entry(stdout, entry, patch_options)?;
        }
    }
    Ok(())
}

/// Print an annotated tag's header block: `tag <name>` (the name stored in the
/// tag object), an optional `Tagger:`/`Date:` pair, a blank line, the tag
/// message verbatim, and a single trailing blank line — the gap before the
/// recursed target object, which is therefore shown with its own separator
/// suppressed. Sets `shown_one` so any *later* top-level entry is separated.
fn show_tag_header(
    stdout: &mut io::Stdout,
    tag: &Tag,
    shown_one: &mut bool,
    suppress_separator: bool,
) -> Result<()> {
    if *shown_one && !suppress_separator {
        writeln!(stdout)?;
    }
    writeln!(stdout, "tag {}", String::from_utf8_lossy(&tag.name))?;
    if let Some(tagger) = &tag.tagger {
        writeln!(stdout, "Tagger: {}", commit_author_identity(tagger))?;
        let date = commit_identity_date(tagger, ForEachRefDateMode::Default);
        if !date.is_empty() {
            writeln!(stdout, "Date:   {date}")?;
        }
    }
    writeln!(stdout)?;
    // The tag message is printed verbatim (no 4-space indent, unlike commits).
    stdout.write_all(&tag.message)?;
    if !tag.message.ends_with(b"\n") {
        writeln!(stdout)?;
    }
    writeln!(stdout)?;
    *shown_one = true;
    Ok(())
}

/// Print a tree the way `git show <tree-ish>` does: a `tree <name>` header
/// (echoing the literal argument), a blank line, and the immediate entry names
/// in tree order, each directory suffixed with `/`. Not recursive.
///
/// Entry names are written verbatim (no C-style `core.quotePath` escaping), which
/// is what git's tree display does — unlike `ls-tree`, special bytes such as a
/// literal tab are passed through untouched.
fn show_tree(stdout: &mut io::Stdout, format: ObjectFormat, name: &str, body: &[u8]) -> Result<()> {
    writeln!(stdout, "tree {name}")?;
    writeln!(stdout)?;
    for entry in TreeEntries::new(format, body) {
        let entry = entry?;
        stdout.write_all(entry.name)?;
        if tree_entry_object_type(entry.mode) == ObjectType::Tree {
            stdout.write_all(b"/")?;
        }
        stdout.write_all(b"\n")?;
    }
    Ok(())
}

/// Emit git's "unknown revision or path" fatal error and exit 128, matching the
/// stderr `git show <bad-rev>` produces.
fn show_unknown_revision(spec: &str) -> Result<()> {
    eprintln!(
        "fatal: ambiguous argument '{spec}': unknown revision or path not in the working tree."
    );
    eprintln!(
        "Use '--' to separate paths from revisions, like this:\n'git <command> [<revision>...] -- [<file>...]'"
    );
    Err(GitError::Exit(128))
}

/// Whether colorized diff output is enabled. `git show` to a non-tty (the
/// interop scenario, and the only context this CLI runs in) defaults to no
/// color; color is only forced when `color.diff`/`color.ui` is set to `always`.
fn diff_color_enabled(config: &GitConfig) -> bool {
    matches!(
        config
            .get("color", None, "diff")
            .or_else(|| config.get("color", None, "ui"))
            .map(str::trim),
        Some("always")
    )
}

/// Parse `git show` arguments into [`ShowOptions`]. Recognises the common
/// formatting and diff-control flags; `--` forces the remaining arguments to be
/// treated as object specs.
fn parse_show_args(args: &[String]) -> Result<ShowOptions> {
    let mut options = ShowOptions::default();
    let mut positional_only = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if positional_only {
            options.specs.push(arg.clone());
            continue;
        }
        match arg.as_str() {
            "--" => positional_only = true,
            // --- output selection ------------------------------------------------
            "-s" | "--no-patch" => options.suppress_diff(),
            "-p" | "-u" | "--patch" => {
                options.diff_mode = ShowDiffMode::Patch;
                options.stat = false;
                options.compact_summary = false;
                options.numstat = false;
                options.shortstat = false;
                options.summary = false;
                options.raw = false;
            }
            "--name-only" => {
                options.diff_mode = ShowDiffMode::NameOnly;
            }
            "--name-status" => {
                options.diff_mode = ShowDiffMode::NameStatus;
            }
            "--stat" => {
                options.stat = true;
                options.restore_patch();
            }
            "--compact-summary" => {
                options.compact_summary = true;
                options.restore_patch();
            }
            "--numstat" => {
                options.numstat = true;
                options.restore_patch();
            }
            "--shortstat" => {
                options.shortstat = true;
                options.restore_patch();
            }
            "--summary" => {
                options.summary = true;
                options.restore_patch();
            }
            "--raw" => {
                options.raw = true;
                options.restore_patch();
            }
            // --- pretty / format -------------------------------------------------
            "--oneline" => {
                options.commit_format = ShowCommitFormat::Oneline;
                options.abbrev_commit = true;
            }
            // A bare `--pretty`/`--format` (no value) selects the default medium
            // format, exactly like `--pretty=medium`.
            "--pretty" | "--format" => {
                options.commit_format = ShowCommitFormat::Medium;
            }
            value if let Some(spec) = value.strip_prefix("--pretty=") => {
                options.commit_format = parse_pretty_value(spec)?;
            }
            value if let Some(spec) = value.strip_prefix("--format=") => {
                // `--format=<x>` is exactly `--pretty=<x>`: a known name selects a
                // built-in layout, an explicit `format:`/`tformat:` prefix sets the
                // separator semantics, and a bare user string with `%` behaves as
                // tformat (trailing newline).
                options.commit_format = parse_pretty_value(spec)?;
            }
            // --- oid abbreviation ------------------------------------------------
            "--abbrev-commit" => options.abbrev_commit = true,
            "--no-abbrev-commit" => options.abbrev_commit = false,
            "--abbrev" => options.abbrev_len = Some(7),
            // `--no-abbrev` makes the header/`%h` oids full, but leaves the patch
            // `index` lines at their default abbreviation (only `--full-index`
            // widens those).
            "--no-abbrev" => options.abbrev_len = None,
            value if let Some(width) = value.strip_prefix("--abbrev=") => {
                let parsed = show_parse_abbrev_width(width);
                options.abbrev_len = Some(parsed);
                options.patch_abbrev = Some(parsed);
            }
            "--full-index" => options.patch_full_index = true,
            // --- date ------------------------------------------------------------
            "--date" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--date requires a value".into()))?;
                options.date_mode = show_date_mode(value)?;
            }
            value if let Some(value) = value.strip_prefix("--date=") => {
                options.date_mode = show_date_mode(value)?;
            }
            // --- rename / copy detection ----------------------------------------
            "--no-renames" => options.detect_renames = false,
            "-M" | "--find-renames" => options.detect_renames = true,
            value if let Some(rest) = value.strip_prefix("--find-renames=") => {
                options.detect_renames = true;
                options.rename_threshold = show_parse_similarity(rest)?;
            }
            value if value.starts_with("-M") => {
                options.detect_renames = true;
                options.rename_threshold = show_parse_similarity(&value[2..])?;
            }
            "-C" | "--find-copies" => {
                options.detect_renames = true;
                options.detect_copies = true;
            }
            value if let Some(rest) = value.strip_prefix("--find-copies=") => {
                options.detect_renames = true;
                options.detect_copies = true;
                options.copy_threshold = show_parse_similarity(rest)?;
            }
            value if value.starts_with("-C") => {
                options.detect_renames = true;
                options.detect_copies = true;
                options.copy_threshold = show_parse_similarity(&value[2..])?;
            }
            "--find-copies-harder" => {
                options.detect_copies = true;
                options.find_copies_harder = true;
            }
            "--no-find-copies-harder" => options.find_copies_harder = false,
            // --- ref decoration --------------------------------------------------
            "--decorate" | "--decorate=short" => options.decorate = LogDecorationMode::Short,
            "--decorate=full" => options.decorate = LogDecorationMode::Full,
            "--decorate=auto" => {
                // No tty plumbing here, so `auto` resolves to off (git's piped
                // default), matching the comparison environment.
                options.decorate = LogDecorationMode::Off;
            }
            "--no-decorate" | "--decorate=no" => options.decorate = LogDecorationMode::Off,
            value if let Some(rest) = value.strip_prefix("--decorate=") => {
                eprintln!("fatal: invalid --decorate option: {rest}");
                return Err(GitError::Exit(128));
            }
            // --- accepted-but-inert diff knobs ----------------------------------
            // These influence rendering details sley does not yet model; accept
            // them so common invocations parse, matching how cmd_log treats them.
            "--no-color"
            | "--color"
            | "--minimal"
            | "--patience"
            | "--histogram"
            | "--indent-heuristic"
            | "--no-indent-heuristic"
            | "--ignore-space-at-eol"
            | "--ignore-space-change"
            | "-b"
            | "--ignore-all-space"
            | "-w"
            | "--ignore-blank-lines"
            | "--no-prefix"
            | "--text"
            | "-a"
            | "--no-ext-diff"
            | "--ext-diff"
            | "--no-textconv"
            | "--textconv"
            | "--no-notes"
            | "--no-show-signature"
            | "--root" => {}
            value if value.starts_with("--color=") => {}
            value if value.starts_with('-') && value != "-" => {
                return Err(GitError::Command(format!(
                    "unsupported show option {value}"
                )));
            }
            value => options.specs.push(value.to_string()),
        }
    }
    Ok(options)
}

/// Resolve a `--pretty=<value>` / `--format=<value>` argument into a
/// [`ShowCommitFormat`].
///
/// Matches git's `get_commit_format`: a recognised name selects a built-in
/// layout; an explicit `format:`/`tformat:` prefix sets the user format with
/// separator (no trailing newline) vs. terminator (trailing newline) semantics;
/// and a bare user string is a `tformat`. A bare string that is neither a known
/// name nor contains a `%` placeholder is rejected with git's exact error.
fn parse_pretty_value(value: &str) -> Result<ShowCommitFormat> {
    if let Some(spec) = value.strip_prefix("format:") {
        return Ok(ShowCommitFormat::Custom {
            compiled: CompiledLogFormat::compile(spec, LogFormatDialect::Log)?,
            final_newline: false,
        });
    }
    if let Some(spec) = value.strip_prefix("tformat:") {
        return Ok(ShowCommitFormat::Custom {
            compiled: CompiledLogFormat::compile(spec, LogFormatDialect::Log)?,
            final_newline: true,
        });
    }
    match value {
        "" | "medium" | "default" => Ok(ShowCommitFormat::Medium),
        "oneline" => Ok(ShowCommitFormat::FullOneline),
        // Built-in named layouts sley does not yet render. Reject explicitly
        // rather than mis-formatting them as literal text.
        "short" | "full" | "fuller" | "reference" | "email" | "mboxrd" | "raw" => Err(
            GitError::Unsupported(format!("show does not support --pretty={value}")),
        ),
        other if other.contains('%') => Ok(ShowCommitFormat::Custom {
            compiled: CompiledLogFormat::compile(other, LogFormatDialect::Log)?,
            final_newline: true,
        }),
        other => {
            eprintln!("fatal: invalid --pretty format: {other}");
            Err(GitError::Exit(128))
        }
    }
}

/// Parse an `--abbrev=<n>` width, clamping to git's minimum of 4 like the log
/// implementation does.
fn show_parse_abbrev_width(value: &str) -> usize {
    value.parse::<usize>().unwrap_or(0).max(4)
}

/// Parse an `-M`/`-C`/`--find-renames=`/`--find-copies=` similarity value into a
/// 0..=100 percentage. Accepts a bare integer percentage or a trailing `%`.
fn show_parse_similarity(value: &str) -> Result<u8> {
    if value.is_empty() {
        return Ok(sley_diff_merge::DEFAULT_RENAME_THRESHOLD);
    }
    let digits = value.strip_suffix('%').unwrap_or(value);
    let parsed = digits
        .parse::<u32>()
        .map_err(|_| GitError::Command(format!("invalid similarity value {value}")))?;
    Ok(parsed.min(100) as u8)
}

/// Translate a `--date=<mode>` value into the shared date renderer's mode. Mirrors
/// the subset of formats `git log --date=` supports that map onto
/// [`ForEachRefDateMode`].
fn show_date_mode(value: &str) -> Result<ForEachRefDateMode> {
    log_date_mode(value)
}
