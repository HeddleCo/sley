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
//! private items) brings every helper and type — `cli_git_dir`,
//! `repository_object_format`, `resolve_revision`, `read_repo_config`,
//! `FileObjectDatabase`, `FileRefStore`, the `write_diff_*` writers, the
//! `print_log_format` pretty-format engine, and so on — into scope without
//! re-listing them.
#![allow(clippy::expect_used)]

use crate::*;
use sley::plumbing::sley_object::TreeEntries;
use sley::plumbing::{sley_diff_merge, sley_rev, sley_worktree};
use std::cell::{Ref, RefCell};

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
    /// `--pretty=short`: `commit`/`Author:` + indented message.
    Short,
    /// `--pretty=full`: `commit`/`Author:`/`Commit:` + indented message.
    Full,
    /// `--pretty=fuller`: author + committer identities with dates.
    Fuller,
    /// `--pretty=raw`: the raw commit object headers + raw message.
    Raw,
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

/// How merge commits are diffed (`git show`'s `--diff-merges` resolution).
/// `git show` defaults a merge to [`ShowMergeMode::Combined`] (dense) when no
/// override is given; `--first-parent` flips the default to first-parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShowMergeMode {
    /// `--diff-merges=off`/`none`: no diff for merge commits.
    Off,
    /// `--first-parent` / `--diff-merges=first-parent`: diff against parent 1.
    FirstParent,
    /// `-m` / `--diff-merges=separate`: one diff per parent.
    Separate,
    /// `-c` (`dense=false`) / `--cc` (`dense=true`) / the default: combined.
    Combined { dense: bool },
}

/// Parsed `git show` invocation state.
struct ShowOptions {
    commit_format: ShowCommitFormat,
    diff_mode: ShowDiffMode,
    /// Explicit merge-diff mode, or `None` to use the default (dense combined,
    /// or first-parent under `--first-parent`).
    merge_mode: Option<ShowMergeMode>,
    /// `--first-parent`: flips the merge default to first-parent and restricts
    /// history (history restriction is a no-op for single-commit `show`).
    first_parent: bool,
    /// `--combined-all-paths` (only meaningful with `-c`/`--cc`).
    combined_all_paths: bool,
    /// Whether the `commit <oid>` line (medium/full-oneline) uses an abbreviated
    /// oid. `--oneline` always abbreviates regardless of this flag.
    abbrev_commit: bool,
    /// Abbreviation width for commit/tree/blob oids in headers and `%h`/`%t`/`%p`.
    abbrev_len: Option<usize>,
    /// Show stat output before/instead of the patch.
    stat: bool,
    /// `--stat=<w>[,<n>[,<c>]]` / `--stat-*-width` knobs (terminal-scaled,
    /// config-respecting defaults, like every porcelain command).
    stat_widths: DiffStatWidths,
    /// `--stat=,,<count>` / `--stat-count=<count>` display truncation.
    stat_count: Option<usize>,
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
    /// `--patch-with-stat` / `--patch-with-raw`: render the patch after the
    /// requested prefix output instead of replacing it.
    patch_with_extra: bool,
    /// Full 40/64-hex `index` lines in patches (`--full-index`).
    patch_full_index: bool,
    /// Emit applicable binary patches. `--binary` also implies full index
    /// lines and keeps the default patch body alongside stat/raw output.
    patch_binary: bool,
    /// Explicit patch abbreviation width (`--abbrev=<n>` affects this too).
    patch_abbrev: Option<usize>,
    /// Rename detection toggle (on by default, `--no-renames` disables).
    detect_renames: bool,
    /// Whether rename/copy detection was chosen explicitly by the command line.
    renames_explicit: bool,
    /// Copy detection (`-C`/`--find-copies`).
    detect_copies: bool,
    /// `-C` with `--find-copies-harder`.
    find_copies_harder: bool,
    /// Rename/copy similarity threshold.
    rename_threshold: u8,
    /// Copy similarity threshold.
    copy_threshold: u8,
    /// Date rendering mode for the `Date:` line and `%ad`/`%cd`.
    date_mode: DateMode,
    /// `--encoding=<encoding>` override for commit message output.
    output_encoding: Option<String>,
    /// Ref decoration mode for the `commit` header and `%d`/`%D`. `git show`
    /// defaults to off; `--decorate`/`--decorate=<mode>` enables it.
    decorate: LogDecorationMode,
    /// Whether notes are displayed after the commit message. git shows notes by
    /// default for the medium format (no explicit `--pretty`); an explicit
    /// pretty format or `--no-notes` suppresses them, `--notes`/`--show-notes`
    /// forces them on.
    show_notes: bool,
    /// Whether a notes flag (`--notes`/`--show-notes`/`--no-notes`) was given.
    /// git enables notes for a `%N` userformat only when no flag was passed
    /// (`!show_notes_given`), so a `--no-notes` still wins.
    notes_given: bool,
    /// Explicit `--root` / `--no-root` override. When unset, `log.showRoot`
    /// controls whether a root commit shows the empty-tree diff.
    show_root: Option<bool>,
    /// Whitespace-ignore flags (`-w`, `-b`, `--ignore-space-at-eol`,
    /// `--ignore-cr-at-eol`).
    ws_ignore: sley_diff_merge::WsIgnore,
    /// The line-diff algorithm (`--patience` / `--histogram` / Myers default).
    diff_algorithm: sley_diff_merge::DiffAlgorithm,
    /// `--anchored=<text>` prefixes (patience anchors); cleared by `--patience`.
    anchored: Vec<Vec<u8>>,
    /// `--textconv` / `--no-textconv`: `Some(true)`/`Some(false)` force or
    /// suppress `diff.<d>.textconv` (for both the commit diff and a directly
    /// shown blob); `None` (default) leaves textconv on for the diff path.
    textconv: Option<bool>,
    /// `--ignore-blank-lines`.
    ignore_blank_lines: bool,
    /// Compiled `-I<regex>` (`--ignore-matching-lines`) patterns.
    ignore_regexes: Vec<sley_grep::Regex>,
    /// `--word-diff` rendering mode.
    word_diff_mode: Option<commands::diff_words::WordDiffMode>,
    /// `--word-diff-regex` / `--color-words=<regex>` override.
    word_diff_regex: Option<String>,
    /// Force colored patch output for `--word-diff=color` / `--color-words`.
    color_always: bool,
    /// `--grep=<pattern>` commit-message filters.
    grep_patterns: Vec<String>,
    grep_pattern_kind: sley_grep::PatternKind,
    grep_pattern_kind_explicit: bool,
    grep_ignore_case: bool,
    grep_all_match: bool,
    grep_invert: bool,
    /// `--indent-heuristic` / `--no-indent-heuristic`: `None` falls back to
    /// `diff.indentHeuristic` config (default git-enabled).
    indent_heuristic: Option<bool>,
    /// Revision/pathspec arguments passed to the shared revision parser.
    setup_args: Vec<String>,
    show_signature: Option<bool>,
    /// `--expand-tabs[=<n>]` / `--no-expand-tabs`. `None` defers to the
    /// per-format default ([`show_default_expand_tabs`]).
    expand_tabs: Option<i32>,
}

/// The default tab-expansion width for a `git show` commit format, matching
/// upstream pretty.c's `builtin_formats[]` table (`medium`/`full`/`fuller`
/// expand to 8; everything else defaults off).
fn show_default_expand_tabs(format: &ShowCommitFormat) -> i32 {
    match format {
        ShowCommitFormat::Medium | ShowCommitFormat::Full | ShowCommitFormat::Fuller => 8,
        ShowCommitFormat::Short
        | ShowCommitFormat::Raw
        | ShowCommitFormat::Oneline
        | ShowCommitFormat::FullOneline
        | ShowCommitFormat::Custom { .. } => 0,
    }
}

struct ShowContext<'a> {
    git_dir: &'a Path,
    db: &'a FileObjectDatabase,
    format: ObjectFormat,
    config: &'a GitConfig,
    options: &'a ShowOptions,
    /// Resolves `diff.<d>.textconv` for `git show --textconv <rev>:<path>`.
    userdiff: &'a commands::userdiff::UserdiffResolver,
    decorations: &'a HashMap<ObjectId, Vec<String>>,
    diff_pathspec: Option<&'a DiffPathspec>,
    mailmap: RefCell<Option<commands::utility::Mailmap>>,
}

impl ShowContext<'_> {
    fn mailmap(&self) -> Result<Ref<'_, commands::utility::Mailmap>> {
        let needs_load = self.mailmap.borrow().is_none();
        if needs_load {
            *self.mailmap.borrow_mut() = Some(commands::utility::Mailmap::load_default(
                self.git_dir,
                self.format,
            )?);
        }
        Ok(Ref::map(self.mailmap.borrow(), |mailmap| {
            mailmap
                .as_ref()
                .expect("mailmap cache was just initialized")
        }))
    }
}

struct CommitTrailerLayout {
    text_self_terminated: bool,
    blank_before_diff: bool,
    separator_mode: bool,
    is_merge: bool,
    /// The resolved merge-diff mode for this commit (only meaningful when
    /// `is_merge`).
    merge_mode: ShowMergeMode,
}

/// Resolve the effective merge-diff mode for `git show`: the explicit
/// `--diff-merges`/`-c`/`--cc`/`-m` if given, otherwise dense-combined (or
/// first-parent under `--first-parent`). Mirrors git's
/// `diff_merges_default_to_*` in `cmd_show`'s setup tweak.
fn resolve_show_merge_mode(options: &ShowOptions) -> ShowMergeMode {
    match options.merge_mode {
        Some(mode) => mode,
        None if options.first_parent => ShowMergeMode::FirstParent,
        None => ShowMergeMode::Combined { dense: true },
    }
}

fn show_compiled_format_uses_mailmap(compiled: &CompiledLogFormat) -> bool {
    compiled.tokens.iter().any(|token| {
        matches!(
            token,
            FormatToken::AuthorNameMapped
                | FormatToken::AuthorEmailMapped
                | FormatToken::AuthorEmailLocalMapped
                | FormatToken::CommitterNameMapped
                | FormatToken::CommitterEmailMapped
                | FormatToken::CommitterEmailLocalMapped
        )
    })
}

fn show_commit_format_needs_mailmap(format: &ShowCommitFormat, use_mailmap: bool) -> bool {
    match format {
        ShowCommitFormat::Medium
        | ShowCommitFormat::Short
        | ShowCommitFormat::Full
        | ShowCommitFormat::Fuller => use_mailmap,
        ShowCommitFormat::Custom { compiled, .. } => show_compiled_format_uses_mailmap(compiled),
        // `--pretty=raw` shows the raw, un-mailmapped identity lines.
        ShowCommitFormat::Raw | ShowCommitFormat::Oneline | ShowCommitFormat::FullOneline => false,
    }
}

impl Default for ShowOptions {
    fn default() -> Self {
        Self {
            commit_format: ShowCommitFormat::Medium,
            diff_mode: ShowDiffMode::Patch,
            merge_mode: None,
            first_parent: false,
            combined_all_paths: false,
            abbrev_commit: false,
            abbrev_len: Some(7),
            stat: false,
            stat_widths: DiffStatWidths::terminal(),
            stat_count: None,
            compact_summary: false,
            numstat: false,
            shortstat: false,
            summary: false,
            raw: false,
            patch_with_extra: false,
            patch_full_index: false,
            patch_binary: false,
            patch_abbrev: None,
            detect_renames: true,
            renames_explicit: false,
            detect_copies: false,
            find_copies_harder: false,
            rename_threshold: sley_diff_merge::DEFAULT_RENAME_THRESHOLD,
            copy_threshold: sley_diff_merge::DEFAULT_RENAME_THRESHOLD,
            date_mode: DateMode::Default,
            output_encoding: None,
            decorate: LogDecorationMode::Off,
            // Default `git show` (medium, no `--pretty`) displays notes.
            show_notes: true,
            notes_given: false,
            show_root: None,
            ws_ignore: sley_diff_merge::WsIgnore::default(),
            diff_algorithm: sley_diff_merge::DiffAlgorithm::Myers,
            anchored: Vec::new(),
            textconv: None,
            ignore_blank_lines: false,
            ignore_regexes: Vec::new(),
            word_diff_mode: None,
            word_diff_regex: None,
            color_always: false,
            grep_patterns: Vec::new(),
            grep_pattern_kind: sley_grep::PatternKind::Basic,
            grep_pattern_kind_explicit: false,
            grep_ignore_case: false,
            grep_all_match: false,
            grep_invert: false,
            indent_heuristic: None,
            setup_args: Vec::new(),
            show_signature: None,
            expand_tabs: None,
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

    fn shows_patch_body(&self) -> bool {
        self.patch_with_extra || !self.has_diff_extras()
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
        self.patch_with_extra = false;
    }

    /// Re-enable patch output after a diff sub-mode flag clears the `-s` state.
    fn restore_patch(&mut self) {
        if self.diff_mode == ShowDiffMode::None {
            self.diff_mode = ShowDiffMode::Patch;
        }
    }
}

impl super::grep_args::GrepArgOptions for ShowOptions {
    fn grep_patterns_mut(&mut self) -> &mut Vec<String> {
        &mut self.grep_patterns
    }

    fn grep_pattern_kind_mut(&mut self) -> &mut sley_grep::PatternKind {
        &mut self.grep_pattern_kind
    }

    fn grep_pattern_kind_explicit_mut(&mut self) -> &mut bool {
        &mut self.grep_pattern_kind_explicit
    }

    fn grep_ignore_case_mut(&mut self) -> &mut bool {
        &mut self.grep_ignore_case
    }

    fn grep_all_match_mut(&mut self) -> &mut bool {
        &mut self.grep_all_match
    }

    fn grep_invert_mut(&mut self) -> &mut bool {
        &mut self.grep_invert
    }
}

pub(crate) fn cmd_show(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
    let profile_enabled = show_profile_enabled();
    let profile_start = std::time::Instant::now();
    let mut profile_last = profile_start;
    let mut options = parse_show_args(args)?;
    show_profile_mark(
        profile_enabled,
        "parse_args",
        profile_start,
        &mut profile_last,
    );

    let repo = RepositoryContext::from_session(cli_session)?;
    let repository = repo.repository();
    let git_dir = repository.git_dir();
    let format = repository.object_format();
    let config = repo.config();
    let db = repository.object_database();
    show_profile_mark(
        profile_enabled,
        "discover",
        profile_start,
        &mut profile_last,
    );
    if options.show_signature.is_none() {
        options.show_signature = Some(
            config
                .get_bool("log", None, "showsignature")
                .or_else(|| config.get_bool("log", None, "showSignature"))
                .unwrap_or(false),
        );
    }
    crate::repository::warn_graft_file_deprecated(git_dir, config);

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
    show_profile_mark(
        profile_enabled,
        "signature_decor_mode",
        profile_start,
        &mut profile_last,
    );
    let decorations: HashMap<ObjectId, Vec<String>> = if decoration_mode == LogDecorationMode::Off {
        HashMap::new()
    } else {
        log_decoration_map(
            git_dir,
            db,
            format,
            decoration_mode,
            &crate::DecorationFilter::default(),
        )?
    };
    show_profile_mark(
        profile_enabled,
        "decorations",
        profile_start,
        &mut profile_last,
    );

    let mut setup_args = vec!["--default".to_string(), "HEAD".to_string()];
    setup_args.extend(options.setup_args.iter().cloned());
    let setup = sley_rev::setup_revisions(
        &setup_args,
        &sley_rev::RevisionSetupContext {
            git_dir,
            worktree_root: repo.worktree_root().ok(),
            cwd: repo.cwd(),
            format,
            reader: db,
            config: Some(config),
        },
    )?;
    show_profile_mark(
        profile_enabled,
        "setup_revisions",
        profile_start,
        &mut profile_last,
    );
    if let Some(leftover) = setup.leftovers.first() {
        return Err(GitError::Command(format!(
            "unsupported show option {leftover}"
        )));
    }
    let diff_pathspec = if setup.pathspecs.is_empty() {
        None
    } else {
        let worktree_root = repo.worktree_root()?;
        Some(DiffPathspec::new(
            repo.cwd(),
            worktree_root,
            &setup.pathspecs,
        )?)
    };

    let mut shown_one = false;
    let mut stdout = io::stdout();
    show_profile_mark(
        profile_enabled,
        "userdiff",
        profile_start,
        &mut profile_last,
    );
    let show_userdiff_attributes = worktree_root_for_git_dir(git_dir)
        .ok()
        .map(sley_worktree::StandardAttributeMatcher::from_worktree_root)
        .transpose()?;
    let show_userdiff = commands::userdiff::UserdiffResolver::with_attributes(
        show_userdiff_attributes,
        Some(config.clone()),
    );
    let context = ShowContext {
        git_dir,
        db,
        format,
        config,
        options: &options,
        userdiff: &show_userdiff,
        decorations: &decorations,
        diff_pathspec: diff_pathspec.as_ref(),
        mailmap: RefCell::new(None),
    };
    let grep_kind = log_grep_pattern_kind_from_config(
        config,
        options.grep_pattern_kind,
        options.grep_pattern_kind_explicit,
    );
    let grep_matcher = compile_log_message_grep_matcher(
        &options.grep_patterns,
        grep_kind,
        options.grep_ignore_case,
    )?;
    show_profile_mark(
        profile_enabled,
        "grep_compile",
        profile_start,
        &mut profile_last,
    );
    for tip in &setup.options.positives {
        if !show_tip_matches_grep(
            db,
            format,
            &tip.oid,
            grep_matcher.as_ref(),
            options.grep_all_match,
            options.grep_invert,
        )? {
            continue;
        }
        show_object(
            &mut stdout,
            &context,
            &tip.rev,
            &tip.oid,
            &mut shown_one,
            false,
        )?;
    }
    show_profile_mark(
        profile_enabled,
        "show_objects",
        profile_start,
        &mut profile_last,
    );
    stdout.flush()?;
    show_profile_mark(profile_enabled, "flush", profile_start, &mut profile_last);
    Ok(())
}

fn show_profile_enabled() -> bool {
    std::env::var_os("SLEY_SHOW_PROFILE").is_some_and(|value| value != "0")
}

fn show_profile_mark(
    enabled: bool,
    label: &str,
    start: std::time::Instant,
    last: &mut std::time::Instant,
) {
    if !enabled {
        return;
    }
    let now = std::time::Instant::now();
    eprintln!(
        "{{\"schema\":\"sley.show.profile.v1\",\"stage\":\"{label}\",\"delta_us\":{},\"total_us\":{}}}",
        now.duration_since(*last).as_micros(),
        now.duration_since(start).as_micros()
    );
    *last = now;
}

fn show_tip_matches_grep(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    oid: &ObjectId,
    matcher: Option<&sley_grep::GrepMatcher>,
    all_match: bool,
    invert: bool,
) -> Result<bool> {
    let Some(matcher) = matcher else {
        return Ok(true);
    };
    let object = db.read_object(oid)?;
    let message = match object.object_type {
        ObjectType::Commit => Commit::parse(format, &object.body)?.message,
        ObjectType::Tag => {
            let tag = Tag::parse(format, &object.body)?;
            let target = db.read_object(&tag.object)?;
            if target.object_type != ObjectType::Commit {
                return Ok(false != invert);
            }
            Commit::parse(format, &target.body)?.message
        }
        _ => return Ok(false != invert),
    };
    let matched = if all_match {
        matcher.matches_all(&message)
    } else {
        matcher.matches_any(&message)
    };
    Ok(matched != invert)
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
            // `git show --textconv <rev>:<path>` runs the path's textconv driver
            // over the blob (git's `cmd_show` textconv path); the path comes from
            // the `<rev>:<path>` argument, so a bare-oid blob has none. Without
            // `--textconv` (or with `--no-textconv`) the blob is emitted verbatim.
            if context.options.textconv == Some(true)
                && let Some((_, path)) = name.split_once(':')
                && let Some(driver) = context.userdiff.driver_for_path(path.as_bytes())?
                && let Some(command) = driver.textconv.as_deref()
                && let Some(converted) = commands::userdiff::run_textconv(command, &object.body)?
            {
                stdout.write_all(&converted)?;
                return Ok(());
            }
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
    let profile_enabled = show_profile_enabled();
    let profile_start = std::time::Instant::now();
    let mut profile_last = profile_start;
    let options = context.options;
    let decorations = context.decorations;
    // Resolve `--expand-tabs` for the message body: an explicit CLI value wins,
    // otherwise use the per-format default (medium/full/fuller expand to 8).
    let expand_tabs = options
        .expand_tabs
        .unwrap_or_else(|| show_default_expand_tabs(&options.commit_format));
    let output_encoding = options
        .output_encoding
        .clone()
        .unwrap_or_else(|| log_output_encoding(context.config));
    // `git show` is a log variant: `log.mailmap` (default true) controls whether
    // the default `Author:` line and lower-case identity atoms are mapped; the
    // upper-case `%aN`/… atoms always map.
    let use_mailmap = context
        .config
        .get_bool("log", None, "mailmap")
        .unwrap_or(true);
    let needs_mailmap = show_commit_format_needs_mailmap(&options.commit_format, use_mailmap);
    let mailmap_guard = if needs_mailmap {
        Some(context.mailmap()?)
    } else {
        None
    };
    let mailmap = mailmap_guard.as_deref();
    let empty_mailmap = commands::utility::Mailmap::default();
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
            | ShowCommitFormat::Short
            | ShowCommitFormat::Custom {
                final_newline: false,
                ..
            }
    );
    let text_self_terminated = !matches!(options.commit_format, ShowCommitFormat::Custom { .. });
    let blank_before_diff = matches!(
        options.commit_format,
        ShowCommitFormat::Medium
            | ShowCommitFormat::Short
            | ShowCommitFormat::Custom {
                final_newline: true,
                ..
            }
    );
    let merge_mode = resolve_show_merge_mode(options);

    if is_merge
        && merge_mode == ShowMergeMode::Separate
        && matches!(options.commit_format, ShowCommitFormat::Medium)
    {
        return show_commit_separate_merge(
            stdout,
            context,
            oid,
            commit,
            shown_one,
            suppress_separator,
            mailmap,
            text_self_terminated,
            blank_before_diff,
            separator_mode,
        );
    }

    // Leading inter-entry separator (separator-mode only). A tag parent already
    // supplied the gap, so honour `suppress_separator`.
    if separator_mode && *shown_one && !suppress_separator {
        writeln!(stdout)?;
    }

    if options.show_signature == Some(true) {
        write_show_signature(stdout, context, oid)?;
    }

    match &options.commit_format {
        ShowCommitFormat::Raw => {
            // `--pretty=raw` emits the raw object headers + raw message; no
            // decoration or notes are shown.
            let raw = crate::commands::log::render_log_raw_pretty(&record, expand_tabs);
            stdout.write_all(&raw)?;
        }
        ShowCommitFormat::Medium
        | ShowCommitFormat::Short
        | ShowCommitFormat::Full
        | ShowCommitFormat::Fuller => {
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
            if matches!(options.commit_format, ShowCommitFormat::Fuller) {
                writeln!(
                    stdout,
                    "Author:     {}",
                    commit_identity_mailmapped(&commit.author, mailmap)
                )?;
                writeln!(
                    stdout,
                    "AuthorDate: {}",
                    commit_identity_date_or_sentinel(&commit.author, &options.date_mode)
                )?;
                writeln!(
                    stdout,
                    "Commit:     {}",
                    commit_identity_mailmapped(&commit.committer, mailmap)
                )?;
                writeln!(
                    stdout,
                    "CommitDate: {}",
                    commit_identity_date_or_sentinel(&commit.committer, &options.date_mode)
                )?;
            } else {
                writeln!(
                    stdout,
                    "Author: {}",
                    commit_identity_mailmapped(&commit.author, mailmap)
                )?;
                if matches!(options.commit_format, ShowCommitFormat::Full) {
                    writeln!(
                        stdout,
                        "Commit: {}",
                        commit_identity_mailmapped(&commit.committer, mailmap)
                    )?;
                }
                if matches!(options.commit_format, ShowCommitFormat::Medium) {
                    writeln!(
                        stdout,
                        "Date:   {}",
                        commit_identity_date_or_sentinel(&commit.author, &options.date_mode)
                    )?;
                }
            }
            writeln!(stdout)?;
            let display_message = commit_message_for_commit_encoding(commit, &output_encoding);
            for line in commit_message_lines(&display_message) {
                stdout.write_all(b"    ")?;
                stdout.write_all(&crate::commands::log::log_expand_tabs(line, expand_tabs))?;
                stdout.write_all(b"\n")?;
            }
            if options.show_notes {
                let notes = crate::commands::log::render_standard_notes(
                    context.git_dir,
                    context.format,
                    oid,
                )?;
                stdout.write_all(&notes)?;
            }
        }
        ShowCommitFormat::Oneline => {
            write!(stdout, "{}", format_log_oid(oid, options.abbrev_len))?;
            print_log_decorations(oid, decorations);
            let display_message = commit_message_for_commit_encoding(commit, &output_encoding);
            stdout.write_all(b" ")?;
            stdout.write_all(commit_subject_bytes(&display_message))?;
            stdout.write_all(b"\n")?;
        }
        ShowCommitFormat::FullOneline => {
            write!(
                stdout,
                "{}",
                format_log_commit_header_oid(oid, options.abbrev_commit, options.abbrev_len)
            )?;
            print_log_decorations(oid, decorations);
            let display_message = commit_message_for_commit_encoding(commit, &output_encoding);
            stdout.write_all(b" ")?;
            stdout.write_all(commit_subject_bytes(&display_message))?;
            stdout.write_all(b"\n")?;
        }
        ShowCommitFormat::Custom { compiled, .. } => {
            let source_tag_signatures = HashMap::new();
            let signature_ctx = CliLogSignatureContext {
                git_dir: context.git_dir,
                db: context.db,
                config: context.config,
                source_tag_signatures: &source_tag_signatures,
            };
            // `git show --format=…` is a userformat: `%N` injects the note
            // (raw), so route through the notes-aware emitter when notes are
            // enabled. A format without `%N` is unaffected.
            crate::commands::log::print_log_custom_format_with_notes(
                context.git_dir,
                context.format,
                &record,
                compiled,
                &LogFormatContext {
                    abbrev_len: options.abbrev_len,
                    decorations,
                    marker: '>',
                    dialect: LogFormatDialect::Log,
                    source: None,
                    date_mode: &options.date_mode,
                    source_oid: None,
                    describe: None,
                    signature: Some(&CliLogSignatureAdapter(&signature_ctx)),
                    mailmap: &CliMailmapAdapter(mailmap.unwrap_or(&empty_mailmap)),
                    use_mailmap,
                    color: false,
                    output_encoding: &output_encoding,
                },
                // git enables notes for a `%N` userformat unless a notes flag
                // explicitly turned them off; otherwise honour the flag.
                !options.notes_given || options.show_notes,
            )?;
        }
    }
    *shown_one = true;
    show_profile_mark(
        profile_enabled,
        "commit_header",
        profile_start,
        &mut profile_last,
    );

    // Every format — including `--oneline` — still shows the patch (this is
    // `git show`, which defaults to a diff). The first-parent diff (empty-tree for
    // a root) is computed for merges too, because git's default renders the stat
    // family for them even though the patch/raw/name listings are suppressed.
    let show_root = options.show_root.unwrap_or_else(|| {
        context
            .config
            .get_bool("log", None, "showroot")
            .unwrap_or(true)
    });
    let needs_first_parent_entries =
        show_commit_needs_first_parent_entries(options, is_merge, merge_mode);
    let entries = if !needs_first_parent_entries || (commit.parents.is_empty() && !show_root) {
        Vec::new()
    } else {
        commit_diff_entries(
            context.db,
            context.format,
            context.config,
            options,
            context.diff_pathspec,
            commit,
        )?
    };
    show_profile_mark(
        profile_enabled,
        "commit_diff_entries",
        profile_start,
        &mut profile_last,
    );

    let result = write_commit_trailer(
        stdout,
        context,
        CommitTrailerLayout {
            text_self_terminated,
            blank_before_diff,
            separator_mode,
            is_merge,
            merge_mode,
        },
        commit,
        &entries,
    );
    show_profile_mark(
        profile_enabled,
        "commit_trailer",
        profile_start,
        &mut profile_last,
    );
    result
}

fn show_commit_needs_first_parent_entries(
    options: &ShowOptions,
    is_merge: bool,
    merge_mode: ShowMergeMode,
) -> bool {
    if !is_merge {
        return true;
    }
    // Combined merge patch/name output is derived from all parents in
    // `write_show_combined`. The first-parent entry list only feeds the stat
    // family, so avoid flattening and diffing two full trees for plain
    // `git show --oneline <merge>`.
    !matches!(merge_mode, ShowMergeMode::Combined { .. }) || merge_renders_stat(options)
}

fn show_commit_separate_merge(
    stdout: &mut io::Stdout,
    context: &ShowContext<'_>,
    oid: &ObjectId,
    commit: &Commit,
    shown_one: &mut bool,
    suppress_separator: bool,
    mailmap: Option<&commands::utility::Mailmap>,
    text_self_terminated: bool,
    blank_before_diff: bool,
    separator_mode: bool,
) -> Result<()> {
    let options = context.options;
    for (idx, parent) in commit.parents.iter().enumerate() {
        if separator_mode && (*shown_one || idx > 0) && !(suppress_separator && idx == 0) {
            writeln!(stdout)?;
        }
        write_show_commit_header(stdout, context, oid, commit, Some(parent), mailmap)?;
        *shown_one = true;

        let mut parent_commit = commit.clone();
        parent_commit.parents = vec![*parent];
        let entries = commit_diff_entries(
            context.db,
            context.format,
            context.config,
            options,
            context.diff_pathspec,
            &parent_commit,
        )?;
        write_commit_trailer(
            stdout,
            context,
            CommitTrailerLayout {
                text_self_terminated,
                blank_before_diff,
                separator_mode,
                is_merge: false,
                merge_mode: ShowMergeMode::FirstParent,
            },
            commit,
            &entries,
        )?;
    }
    Ok(())
}

fn write_show_signature(
    stdout: &mut io::Stdout,
    context: &ShowContext<'_>,
    oid: &ObjectId,
) -> Result<()> {
    let object = context.db.read_object(oid)?;
    let Some((payload, signature)) = commands::signing::commit_signature_payload(&object.body)
    else {
        return Ok(());
    };
    let verification = commands::signing::verify_payload(
        context.git_dir,
        Some(context.config),
        &payload,
        &signature,
    )?;
    stdout.write_all(&verification.human_output)?;
    Ok(())
}

fn write_show_commit_header(
    stdout: &mut io::Stdout,
    context: &ShowContext<'_>,
    oid: &ObjectId,
    commit: &Commit,
    from_parent: Option<&ObjectId>,
    mailmap: Option<&commands::utility::Mailmap>,
) -> Result<()> {
    let options = context.options;
    match &options.commit_format {
        ShowCommitFormat::Medium => {
            write!(
                stdout,
                "commit {}",
                format_log_commit_header_oid(oid, options.abbrev_commit, options.abbrev_len)
            )?;
            if let Some(parent) = from_parent {
                write!(
                    stdout,
                    " (from {})",
                    format_log_commit_header_oid(parent, options.abbrev_commit, options.abbrev_len)
                )?;
            }
            print_log_decorations(oid, context.decorations);
            writeln!(stdout)?;
            let abbrev = merge_line_abbrev(options);
            let parents = commit
                .parents
                .iter()
                .map(|parent| format_log_oid(parent, abbrev))
                .collect::<Vec<_>>()
                .join(" ");
            writeln!(stdout, "Merge: {parents}")?;
            writeln!(
                stdout,
                "Author: {}",
                commit_identity_mailmapped(&commit.author, mailmap)
            )?;
            writeln!(
                stdout,
                "Date:   {}",
                commit_identity_date_or_sentinel(&commit.author, &options.date_mode)
            )?;
            writeln!(stdout)?;
            for line in String::from_utf8_lossy(&commit.message).lines() {
                writeln!(stdout, "    {line}")?;
            }
            if options.show_notes {
                let notes = crate::commands::log::render_standard_notes(
                    context.git_dir,
                    context.format,
                    oid,
                )?;
                stdout.write_all(&notes)?;
            }
        }
        _ => unreachable!("separate merge header is only used for medium format"),
    }
    Ok(())
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
    commit: &Commit,
    entries: &[sley_diff_merge::NameStatusEntry],
) -> Result<()> {
    let options = context.options;
    let diff_active = options.diff_mode != ShowDiffMode::None;
    // A merge in combined mode renders the combined patch (plus the stat family,
    // which is always first-parent). In any other merge mode (off / first-parent
    // default) only the stat family renders.
    let combined_merge =
        layout.is_merge && matches!(layout.merge_mode, ShowMergeMode::Combined { .. });
    // `--first-parent` (and `--diff-merges=first-parent`) renders the full
    // first-parent diff for a merge — the same body an ordinary commit gets.
    let first_parent_merge = layout.is_merge && layout.merge_mode == ShowMergeMode::FirstParent;
    // For an off-mode merge only the stat family renders; for a combined merge,
    // a first-parent merge, and an ordinary commit the body renders fully.
    let body_renders = if combined_merge {
        // A combined merge renders a body for every active diff mode (the
        // combined patch, the first-parent stat family, or the combined
        // name/name-status listing).
        diff_active && (!merge_renders_stat(options) || !entries.is_empty())
    } else if layout.is_merge && !first_parent_merge {
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
        // A combined merge separates the commit header from its diff with a
        // blank line (git's diff_tree_combined `line_termination` separator),
        // even for `--oneline` which abuts the diff for ordinary commits. The
        // exception is `--pretty=format:` (text not self-terminated): there the
        // text line's own newline above is the only separator, matching git.
        if options.patch_with_extra
            && (options.stat || options.compact_summary)
            && layout.blank_before_diff
            && !combined_merge
        {
            writeln!(stdout, "---")?;
        } else if layout.blank_before_diff || (combined_merge && layout.text_self_terminated) {
            writeln!(stdout)?;
        }
        return if combined_merge {
            write_show_combined(stdout, context, &layout, commit, entries)
        } else if layout.is_merge && !first_parent_merge {
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
    let stat_entries = collect_diff_stat_entries(entries, db, None, false)?;
    if options.numstat {
        for entry in &stat_entries {
            write_diff_numstat_materialized_entry(stdout, entry.entry, entry.stats, false)?;
        }
    }
    if options.stat || options.compact_summary {
        let mut stat_widths = options.stat_widths;
        stat_widths.resolve_config(config);
        write_diff_stat_materialized_with_widths(
            stdout,
            &stat_entries,
            DiffStatOptions {
                compact_summary: options.compact_summary,
                stat_count: options.stat_count,
                color,
                quote_path_fully: true,
            },
            stat_widths,
        )?;
    }
    if options.shortstat {
        write_diff_shortstat_materialized(stdout, &stat_entries)?;
    }
    if options.summary {
        for entry in entries {
            write_diff_summary_entry(stdout, entry)?;
        }
    }
    Ok(())
}

/// Render a merge commit's combined diff for `git show` (`-c`/`--cc`/default):
/// the first-parent stat family (when requested) followed by the combined patch
/// (when patch output is active). `entries` is the first-parent name-status
/// list reused for the stat family.
fn write_show_combined(
    stdout: &mut io::Stdout,
    context: &ShowContext<'_>,
    layout: &CommitTrailerLayout,
    commit: &Commit,
    entries: &[sley_diff_merge::NameStatusEntry],
) -> Result<()> {
    let options = context.options;
    let db = context.db;
    let stat_entries =
        if options.numstat || options.stat || options.compact_summary || options.shortstat {
            collect_diff_stat_entries(entries, db, None, false)?
        } else {
            Vec::new()
        };
    let format = context.format;
    let dense = matches!(layout.merge_mode, ShowMergeMode::Combined { dense: true });

    let parent_trees = commit
        .parents
        .iter()
        .map(|parent| {
            let object = db.read_object(parent)?;
            let parent_commit = Commit::parse_ref(format, &object.body)?;
            Ok(parent_commit.tree)
        })
        .collect::<Result<Vec<_>>>()?;
    let paths = commands::combined::combined_paths(db, format, &commit.tree, &parent_trees)?;
    let render_ctx = commands::combined::CombinedRenderCtx {
        db,
        format,
        dense,
        all_paths: options.combined_all_paths,
        context: 3,
        ws_ignore: options.ws_ignore,
        diff_algorithm: options.diff_algorithm,
        src_prefix: "a/",
        dst_prefix: "b/",
        patch_abbrev: options.patch_abbrev.unwrap_or(7).min(format.hex_len()),
        raw_abbrev: None,
    };

    // `--name-only`/`--name-status` print the combined name (status) listing.
    match options.diff_mode {
        ShowDiffMode::NameOnly => {
            for path in &paths {
                writeln!(stdout, "{}", status_quote_path(&path.path, false))?;
            }
            return Ok(());
        }
        ShowDiffMode::NameStatus => {
            for path in &paths {
                commands::combined::write_combined_name_status(stdout, path, false)?;
            }
            return Ok(());
        }
        ShowDiffMode::None => return Ok(()),
        ShowDiffMode::Patch => {}
    }

    // Patch mode: the stat family (first-parent) renders first; the combined
    // patch renders only when no stat/raw extra replaced it (git's `show_patch
    // = !has_diff_extras()`).
    let stat_active = merge_renders_stat(options);
    if stat_active {
        write_merge_stat(stdout, db, context.config, options, entries)?;
    }
    if options.has_diff_extras() && !options.patch_with_extra {
        return Ok(());
    }

    // git separates a preceding stat block from the combined patch with a blank
    // line (the `--patch-with-stat` separator).
    if stat_active && !paths.is_empty() {
        writeln!(stdout)?;
    }
    for path in &paths {
        commands::combined::write_combined_patch(stdout, &render_ctx, path)?;
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
    config: &GitConfig,
    options: &ShowOptions,
    pathspec: Option<&DiffPathspec>,
    commit: &Commit,
) -> Result<Vec<sley_diff_merge::NameStatusEntry>> {
    let (detect_renames, detect_copies) = show_effective_rename_detection(options, config);
    let options = sley_diff_merge::DiffNameStatusOptions {
        detect_renames,
        detect_copies,
        find_copies_harder: options.find_copies_harder,
        rename_empty: true,
        detect_inexact: true,
        rename_threshold: options.rename_threshold,
        copy_threshold: options.copy_threshold,
        rename_limit: 0,
        ..Default::default()
    };
    let entries = match commit.parents.first() {
        Some(parent_oid) => {
            let parent_object = db.read_object(parent_oid)?;
            let parent_commit = Commit::parse_ref(format, &parent_object.body)?;
            sley_diff_merge::diff_name_status_trees_with_options(
                db,
                format,
                &parent_commit.tree,
                &commit.tree,
                options,
            )
        }
        None => sley_diff_merge::diff_name_status_empty_tree_with_options(
            db,
            format,
            &commit.tree,
            options,
        ),
    }?;
    Ok(match pathspec {
        Some(pathspec) => apply_diff_pathspec(entries, pathspec),
        None => entries,
    })
}

fn show_effective_rename_detection(options: &ShowOptions, config: &GitConfig) -> (bool, bool) {
    if options.renames_explicit {
        return (options.detect_renames, options.detect_copies);
    }
    match config.get("diff", None, "renames").map(str::trim) {
        Some("false" | "no" | "off" | "0") => (false, false),
        Some("copies" | "copy") => (true, true),
        Some("true" | "yes" | "on" | "1" | "renames") | None => {
            (options.detect_renames, options.detect_copies)
        }
        Some(_) => (options.detect_renames, options.detect_copies),
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

    let show_patch = options.shows_patch_body();
    let stat_entries =
        if options.numstat || options.stat || options.compact_summary || options.shortstat {
            collect_diff_stat_entries(entries, db, None, false)?
        } else {
            Vec::new()
        };

    if entries.is_empty() {
        return Ok(());
    }

    let mut stat_widths = options.stat_widths;
    stat_widths.resolve_config(config);
    if show_patch {
        let userdiff_attributes = worktree_root_for_git_dir(git_dir)
            .ok()
            .map(sley_worktree::StandardAttributeMatcher::from_worktree_root)
            .transpose()?;
        let userdiff = commands::userdiff::UserdiffResolver::with_attributes(
            userdiff_attributes,
            Some(config.clone()),
        );
        let colors = options
            .color_always
            .then(|| commands::diff_words::DiffColors::enabled(Some(config)));
        let word_request = options.word_diff_mode.map(|mode| WordDiffRequest {
            mode,
            cli_regex: options.word_diff_regex.as_deref(),
        });
        render_diff_entries(
            stdout,
            entries,
            DiffEntryRenderModes {
                raw: options.raw,
                numstat: options.numstat,
                stat: options.stat || options.compact_summary,
                shortstat: options.shortstat,
                summary: options.summary,
                patch: true,
            },
            DiffEntryRenderContext {
                raw: DiffEntryRawRenderOptions {
                    z: false,
                    abbrev: raw_abbrev,
                    format,
                },
                stat: DiffEntryStatRenderOptions {
                    source: Some(DiffEntryStatSource::Materialized(&stat_entries)),
                    z: false,
                    options: DiffStatOptions {
                        compact_summary: options.compact_summary,
                        stat_count: options.stat_count,
                        color,
                        quote_path_fully: true,
                    },
                    widths: Some(stat_widths),
                },
                prefix_already_written: false,
                after_stat: None,
            },
            |_| false,
            |stdout, entry| {
                let patch_options = DiffRenderOptions {
                    line_indicators: sley_diff_merge::render::LineIndicators::default(),
                    suppress_blank_empty: config
                        .get_bool("diff", None, "suppressblankempty")
                        .unwrap_or(false),
                    binary: options.patch_binary,
                    anchors: &options.anchored,
                    allow_textconv: options.textconv != Some(false),
                    db,
                    worktree_root: None,
                    use_worktree_new: false,
                    format,
                    abbrev: patch_abbrev,
                    src_prefix: "a/",
                    dst_prefix: "b/",
                    context: 3,
                    userdiff: Some(&userdiff),
                    funcname: None,
                    colors: colors.as_ref(),
                    word_diff: word_request.as_ref(),
                    no_index_contents: None,
                    submodule_format: sley_rev::diff_options::SubmoduleDiffFormat::Short,
                    submodule_dirt: None,
                    ws_error: None,
                    color_moved: None,
                    interhunk: 0,
                    ws_ignore: options.ws_ignore,
                    diff_algorithm: options.diff_algorithm,
                    ignore_blank_lines: options.ignore_blank_lines,
                    ignore_regexes: &options.ignore_regexes,
                    line_ranges: None,
                    indent_heuristic: options.indent_heuristic.unwrap_or_else(|| {
                        config
                            .get_bool("diff", None, "indentheuristic")
                            .unwrap_or(true)
                    }),
                };
                write_diff_patch_entry(stdout, entry, patch_options)
            },
        )?;
    } else {
        render_diff_entries(
            stdout,
            entries,
            DiffEntryRenderModes {
                raw: options.raw,
                numstat: options.numstat,
                stat: options.stat || options.compact_summary,
                shortstat: options.shortstat,
                summary: options.summary,
                patch: false,
            },
            DiffEntryRenderContext {
                raw: DiffEntryRawRenderOptions {
                    z: false,
                    abbrev: raw_abbrev,
                    format,
                },
                stat: DiffEntryStatRenderOptions {
                    source: Some(DiffEntryStatSource::Materialized(&stat_entries)),
                    z: false,
                    options: DiffStatOptions {
                        compact_summary: options.compact_summary,
                        stat_count: options.stat_count,
                        color,
                        quote_path_fully: true,
                    },
                    widths: Some(stat_widths),
                },
                prefix_already_written: false,
                after_stat: None,
            },
            |_| false,
            |_, _| Ok(()),
        )?;
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
        let date = commit_identity_date(tagger, &DateMode::Default);
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
/// formatting and diff-control flags; `--` is preserved for the shared
/// revision/pathspec parser.
fn parse_show_args(args: &[String]) -> Result<ShowOptions> {
    let mut options = ShowOptions::default();
    let mut positional_only = false;
    let mut ignore_regex_patterns: Vec<String> = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if positional_only {
            options.setup_args.push(arg.clone());
            continue;
        }
        if super::grep_args::parse_grep_args(arg, &mut iter, &mut options)? {
            continue;
        }
        match arg.as_str() {
            "--" => {
                options.setup_args.push(arg.clone());
                positional_only = true;
            }
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
            // --- merge diff selection (git's diff-merges parsing) --------------
            "-c" => {
                options.merge_mode = Some(ShowMergeMode::Combined { dense: false });
                options.restore_patch();
            }
            "--cc" => {
                options.merge_mode = Some(ShowMergeMode::Combined { dense: true });
                options.restore_patch();
            }
            "-m" => options.merge_mode = Some(ShowMergeMode::Separate),
            "--first-parent" => {
                options.first_parent = true;
            }
            "--combined-all-paths" => options.combined_all_paths = true,
            "--no-diff-merges" => options.merge_mode = Some(ShowMergeMode::Off),
            "--diff-merges" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--diff-merges requires a value".into()))?;
                options.merge_mode = Some(show_parse_diff_merges(value)?);
                options.restore_patch();
            }
            value if let Some(rest) = value.strip_prefix("--diff-merges=") => {
                options.merge_mode = Some(show_parse_diff_merges(rest)?);
                options.restore_patch();
            }
            "--stat" => {
                options.stat = true;
                options.restore_patch();
            }
            value
                if value.starts_with("--stat=")
                    || value.starts_with("--stat-width=")
                    || value.starts_with("--stat-name-width=")
                    || value.starts_with("--stat-graph-width=")
                    || value.starts_with("--stat-count=") =>
            {
                options.stat = true;
                options.restore_patch();
                diff_stat_parse_width_option(value, &mut options.stat_widths)?;
                if let Some(count) = diff_stat_count_option(value)? {
                    options.stat_count = count;
                }
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
            "--patch-with-stat" => {
                options.stat = true;
                options.patch_with_extra = true;
                options.restore_patch();
            }
            "--patch-with-raw" => {
                options.raw = true;
                options.patch_with_extra = true;
                options.restore_patch();
            }
            // --- pretty / format -------------------------------------------------
            "--oneline" => {
                options.commit_format = ShowCommitFormat::Oneline;
                options.abbrev_commit = true;
                // An explicit pretty format suppresses the default notes block.
                options.show_notes = false;
            }
            // A bare `--pretty`/`--format` (no value) selects the default medium
            // format, exactly like `--pretty=medium`.
            "--pretty" | "--format" => {
                options.commit_format = ShowCommitFormat::Medium;
                options.show_notes = false;
            }
            value if let Some(spec) = value.strip_prefix("--pretty=") => {
                options.commit_format = parse_pretty_value(spec)?;
                options.show_notes = false;
            }
            value if let Some(spec) = value.strip_prefix("--format=") => {
                // `--format=<x>` is exactly `--pretty=<x>`: a known name selects a
                // built-in layout, an explicit `format:`/`tformat:` prefix sets the
                // separator semantics, and a bare user string with `%` behaves as
                // tformat (trailing newline).
                options.commit_format = parse_pretty_value(spec)?;
                options.show_notes = false;
            }
            value if let Some(encoding) = value.strip_prefix("--encoding=") => {
                options.output_encoding = Some(encoding.to_string());
            }
            "--expand-tabs" => options.expand_tabs = Some(8),
            "--no-expand-tabs" => options.expand_tabs = Some(0),
            value if let Some(raw) = value.strip_prefix("--expand-tabs=") => {
                let n: i32 = raw.parse().map_err(|_| {
                    GitError::Command(format!("could not parse expand-tabs value '{raw}'"))
                })?;
                options.expand_tabs = Some(n.max(0));
            }
            "--notes" | "--show-notes" => {
                options.show_notes = true;
                options.notes_given = true;
            }
            "--no-notes" => {
                options.show_notes = false;
                options.notes_given = true;
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
            "--binary" => {
                options.patch_binary = true;
                options.patch_full_index = true;
                options.patch_with_extra = true;
                options.restore_patch();
            }
            "--no-binary" => options.patch_binary = false,
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
            "--no-renames" => {
                options.detect_renames = false;
                options.renames_explicit = true;
            }
            "-M" | "--find-renames" => {
                options.detect_renames = true;
                options.renames_explicit = true;
            }
            value if let Some(rest) = value.strip_prefix("--find-renames=") => {
                options.detect_renames = true;
                options.renames_explicit = true;
                options.rename_threshold = show_parse_similarity(rest)?;
            }
            value if value.starts_with("-M") => {
                options.detect_renames = true;
                options.renames_explicit = true;
                options.rename_threshold = show_parse_similarity(&value[2..])?;
            }
            "-C" | "--find-copies" => {
                options.detect_renames = true;
                options.detect_copies = true;
                options.renames_explicit = true;
            }
            value if let Some(rest) = value.strip_prefix("--find-copies=") => {
                options.detect_renames = true;
                options.detect_copies = true;
                options.renames_explicit = true;
                options.copy_threshold = show_parse_similarity(rest)?;
            }
            value if value.starts_with("-C") => {
                options.detect_renames = true;
                options.detect_copies = true;
                options.renames_explicit = true;
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
            "--minimal" => options.diff_algorithm = sley_diff_merge::DiffAlgorithm::Minimal,
            "--patience" => {
                options.diff_algorithm = sley_diff_merge::DiffAlgorithm::Patience;
                options.anchored.clear();
            }
            "--histogram" => options.diff_algorithm = sley_diff_merge::DiffAlgorithm::Histogram,
            "--textconv" => options.textconv = Some(true),
            "--no-textconv" => options.textconv = Some(false),
            value if let Some(text) = value.strip_prefix("--anchored=") => {
                options.diff_algorithm = sley_diff_merge::DiffAlgorithm::Patience;
                options.anchored.push(text.as_bytes().to_vec());
            }
            "--ignore-all-space" | "-w" => options.ws_ignore.all_space = true,
            "--ignore-space-change" | "-b" => options.ws_ignore.space_change = true,
            "--ignore-space-at-eol" => options.ws_ignore.space_at_eol = true,
            "--ignore-cr-at-eol" => options.ws_ignore.cr_at_eol = true,
            "--ignore-blank-lines" => options.ignore_blank_lines = true,
            "--word-diff" => {
                if options.word_diff_mode.is_none() {
                    options.word_diff_mode = Some(commands::diff_words::WordDiffMode::Plain);
                }
            }
            value if let Some(mode) = value.strip_prefix("--word-diff=") => {
                options.word_diff_mode = match mode {
                    "plain" => Some(commands::diff_words::WordDiffMode::Plain),
                    "porcelain" => Some(commands::diff_words::WordDiffMode::Porcelain),
                    "color" => {
                        options.color_always = true;
                        Some(commands::diff_words::WordDiffMode::Color)
                    }
                    "none" => None,
                    _ => {
                        eprintln!("error: bad --word-diff argument: {mode}");
                        return Err(GitError::Exit(129));
                    }
                };
            }
            "--word-diff-regex" => {
                let value = iter.next().ok_or_else(|| {
                    GitError::Command("--word-diff-regex requires a value".into())
                })?;
                options.word_diff_regex = Some(value.clone());
                if options.word_diff_mode.is_none() {
                    options.word_diff_mode = Some(commands::diff_words::WordDiffMode::Plain);
                }
            }
            value if let Some(regex) = value.strip_prefix("--word-diff-regex=") => {
                options.word_diff_regex = Some(regex.to_string());
                if options.word_diff_mode.is_none() {
                    options.word_diff_mode = Some(commands::diff_words::WordDiffMode::Plain);
                }
            }
            "--color-words" => {
                options.color_always = true;
                options.word_diff_mode = Some(commands::diff_words::WordDiffMode::Color);
            }
            value if let Some(regex) = value.strip_prefix("--color-words=") => {
                options.color_always = true;
                options.word_diff_mode = Some(commands::diff_words::WordDiffMode::Color);
                options.word_diff_regex = Some(regex.to_string());
            }
            "-I" | "--ignore-matching-lines" => {
                let value = iter.next().ok_or_else(|| {
                    GitError::Command("--ignore-matching-lines requires a value".into())
                })?;
                ignore_regex_patterns.push(value.clone());
            }
            value if let Some(rest) = value.strip_prefix("--ignore-matching-lines=") => {
                ignore_regex_patterns.push(rest.to_string());
            }
            value if value.starts_with("-I") && value.len() > 2 => {
                ignore_regex_patterns.push(value[2..].to_string());
            }
            "--indent-heuristic" => options.indent_heuristic = Some(true),
            "--no-indent-heuristic" => options.indent_heuristic = Some(false),
            "--show-signature" => options.show_signature = Some(true),
            "--no-show-signature" => options.show_signature = Some(false),
            "--no-color"
            | "--color"
            | "--no-prefix"
            | "--text"
            | "-a"
            | "--no-ext-diff"
            | "--ext-diff"
            | "--color-moved"
            | "--no-color-moved"
            | "--color-moved-ws"
            | "--no-color-moved-ws" => {}
            "--root" => options.show_root = Some(true),
            "--no-root" => options.show_root = Some(false),
            value if value.starts_with("--color=") => {}
            value
                if value.starts_with("--color-moved=")
                    || value.starts_with("--color-moved-ws=")
                    || value.starts_with("--no-color-moved=")
                    || value.starts_with("--no-color-moved-ws=") => {}
            value if value.starts_with('-') && value != "-" => {
                return Err(GitError::Command(format!(
                    "unsupported show option {value}"
                )));
            }
            value => options.setup_args.push(value.to_string()),
        }
    }
    options.ignore_regexes = crate::compile_ignore_matching_regexes(&ignore_regex_patterns)?;
    if options.combined_all_paths
        && !matches!(options.merge_mode, Some(ShowMergeMode::Combined { .. }))
    {
        return Err(GitError::Command(
            "--combined-all-paths makes no sense without -c or --cc".into(),
        ));
    }
    Ok(options)
}

/// Resolve a `--diff-merges=<value>` argument into a [`ShowMergeMode`]
/// (git's `func_by_opt`).
fn show_parse_diff_merges(value: &str) -> Result<ShowMergeMode> {
    match value {
        "off" | "none" => Ok(ShowMergeMode::Off),
        "1" | "first-parent" => Ok(ShowMergeMode::FirstParent),
        "separate" | "m" | "on" => Ok(ShowMergeMode::Separate),
        "c" | "combined" => Ok(ShowMergeMode::Combined { dense: false }),
        "cc" | "dense-combined" => Ok(ShowMergeMode::Combined { dense: true }),
        _ => {
            eprintln!("fatal: invalid value for '--diff-merges': '{value}'");
            Err(GitError::Exit(128))
        }
    }
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
        "short" => Ok(ShowCommitFormat::Short),
        "full" => Ok(ShowCommitFormat::Full),
        "fuller" => Ok(ShowCommitFormat::Fuller),
        "raw" => Ok(ShowCommitFormat::Raw),
        "oneline" => Ok(ShowCommitFormat::FullOneline),
        // `reference`: `<abbrev-hash> (<subject>, <short-author-date>)`.
        "reference" => Ok(ShowCommitFormat::Custom {
            compiled: CompiledLogFormat::compile("%h (%s, %as)", LogFormatDialect::Log)?,
            final_newline: true,
        }),
        // Built-in named layouts sley does not yet render. Reject explicitly
        // rather than mis-formatting them as literal text.
        "email" | "mboxrd" => Err(GitError::Unsupported(format!(
            "show does not support --pretty={value}"
        ))),
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
/// [`DateMode`].
fn show_date_mode(value: &str) -> Result<DateMode> {
    log_date_mode(value)
}
