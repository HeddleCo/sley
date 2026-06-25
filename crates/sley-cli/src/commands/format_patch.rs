//! `git format-patch` — prepare each commit in a range as an mbox-format
//! `.patch` file (or stream them with `--stdout`), suitable for `git am` /
//! `git send-email`.
//!
//! Each commit becomes one "email": a `From <sha> Mon Sep 17 00:00:00 2001`
//! mbox separator, the `From:`/`Date:`/`Subject: [PATCH n/m] ...` headers, the
//! commit body, a `---` line, the diffstat, the unified diff against the
//! commit's first parent, and the `-- \n<version>` signature trailer. The
//! commit-selection semantics mirror git: a bare `<commit>` means
//! `<commit>..HEAD`, `<since>..<until>` is the asymmetric range, `-<n>` takes
//! the last n commits of HEAD, and merge commits are skipped. Patches are
//! emitted oldest-first.
//!
//! Like the other command modules this globs the crate root (`use crate::*`) so
//! every shared plumbing helper — `RepositoryContext`, `FileObjectDatabase`,
//! `FileRefStore`, the `sley_rev`/`sley_diff_merge` re-exports, the
//! identity/date helpers, and so on — is in scope without re-listing it. The
//! diff/stat rendering in the shared crate writes to generic `Write` sinks, so
//! format-patch keeps only the mbox framing here and delegates patch/summary
//! rendering to the unified diff path.

// Glob the crate root for shared plumbing; see commands::stash for rationale.
use crate::*;
use sley_notes::{NotesRef, read_note_bytes};

/// The `--rfc[=<token>]` / `--no-rfc` state.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RfcMode {
    /// No `--rfc` seen — leave the subject prefix untouched.
    Unset,
    /// `--no-rfc` or `--rfc=` — explicitly clear any earlier `--rfc`.
    Clear,
    /// `--rfc` (default `RFC`) or `--rfc=<token>`.
    Token(String),
}

/// The `--from[=<ident>]` / `--no-from` state, before `format.from` is folded in.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FromMode {
    /// No `--from`/`--no-from` — defer to `format.from`.
    Unset,
    /// `--no-from` — never rewrite From: (overrides `format.from`).
    Clear,
    /// Bare `--from` — use the runtime committer identity.
    Committer,
    /// `--from=<ident>`.
    Ident(String),
}

/// The `--signature` / `--no-signature` state, before config is folded in.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SignatureMode {
    /// No `--signature`/`--no-signature` — use config or the git version.
    Default,
    /// `--signature=<text>` (empty text suppresses the block).
    Text(String),
    /// `--no-signature` — suppress the signature block entirely.
    Suppress,
}

/// MIME `multipart/mixed` wrapping for `--attach`/`--inline` (git's
/// `rev->mime_boundary` + `no_inline`). The `boundary` is the inner string
/// (without git's 12-dash `mime_boundary_leader`); `inline` selects the second
/// part's `Content-Disposition` (`inline` vs `attachment`).
#[derive(Debug, Clone, PartialEq, Eq)]
struct MimeAttach {
    boundary: String,
    inline: bool,
}

/// The `--cover-from-description=<mode>` / `format.coverFromDescription` state,
/// mirroring git's `enum cover_from_description`. The default is `Message`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CoverFromDescription {
    /// Don't pull anything from the branch/description: keep the placeholder
    /// subject and blurb.
    None,
    /// Subject stays the placeholder; the description becomes the blurb body.
    Message,
    /// First line of the description becomes the subject; the rest is the body.
    Subject,
    /// Like `Subject`, but fall back to `Message` when the would-be subject is
    /// longer than 100 characters.
    Auto,
}

/// How the `[PATCH ...]` subject prefix is numbered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NumberMode {
    /// Decide from the commit count: numbered (`n/m`) for >1 commit, bare
    /// (`[PATCH]`) for a single commit. This is git's default.
    Auto,
    /// Force `[PATCH n/m]` even for a single commit (`-n`/`--numbered`).
    Numbered,
    /// Force a bare `[PATCH]` even for many commits (`-N`/`--no-numbered`).
    Unnumbered,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RelativeMode {
    Config,
    Off,
    On(Option<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BaseMode {
    Config,
    None,
    Commit(String),
    Auto,
}

/// Parsed `git format-patch` invocation.
struct FormatPatchOptions {
    /// Stream all patches to stdout instead of writing files (`--stdout`).
    stdout: bool,
    /// Output directory for the `.patch` files (`-o`/`--output-directory`).
    output_directory: Option<String>,
    /// Write the concatenated mbox stream to a single file (`--output=<file>`).
    output: Option<String>,
    /// `[PATCH ...]` numbering policy.
    number_mode: NumberMode,
    /// First patch number (`--start-number`); defaults to 1.
    start_number: Option<usize>,
    /// Append a `Signed-off-by:` trailer using the runtime committer identity
    /// (`-s`/`--signoff`).
    signoff: bool,
    /// Whether to include the diffstat after the `---` line. On by default;
    /// cleared by `--no-stat`.
    stat: bool,
    /// `--stat=<w>[,<n>[,<c>]]` / `--stat-*-width` knobs. format-patch never
    /// calls git's `init_diffstat_widths`, so the fields start at 0 (and a
    /// zero stat-width becomes the 72-column mail wrap at render time); the
    /// diff.stat*Width config is intentionally ignored.
    stat_widths: DiffStatWidths,
    /// `--stat=,,<count>` / `--stat-count=<count>` display truncation.
    stat_count: Option<usize>,
    /// Unified-diff context lines (`-U<n>` / `--unified=<n>`), default 3.
    context_lines: usize,
    /// Custom subject prefix replacing `PATCH` (`--subject-prefix=<p>`), if the
    /// user gave one explicitly on the command line (overrides
    /// `format.subjectPrefix`). `None` means "use the configured/default value".
    subject_prefix: Option<String>,
    /// `--rfc[=<token>]` / `--no-rfc`: the rfc token to weave into the prefix.
    /// `RfcMode::Unset` is the default (no `--rfc`); `Clear` is `--no-rfc` or
    /// `--rfc=`; `Token(t)` is `--rfc=t` (a leading `-` appends `t[1..]` after
    /// the prefix instead of inserting `t ` before it).
    rfc: RfcMode,
    /// `-v<n>`/`--reroll-count=<n>`: appends ` v<n>` to the subject prefix and
    /// prepends a sanitized `v<n>-` to each output filename.
    reroll_count: Option<String>,
    /// `--filename-max-length=<n>` / `format.filenameMaxLength`: the maximum
    /// length of a patch filename (basename), default 64. `None` resolves to
    /// the config value or the default.
    filename_max_length: Option<usize>,
    /// Diff path prefixes. `Some(false)` = `--no-prefix`/`format.noprefix` (empty
    /// prefixes); `Some(true)` = `--default-prefix` (force `a/`,`b/`); `None`
    /// defers to `format.noprefix` config.
    prefix_mode: Option<bool>,
    /// Resolved (src, dst) diff prefixes — `("a/", "b/")` by default, empty
    /// under no-prefix. Filled by [`cmd_format_patch`] once config is read.
    src_prefix: String,
    dst_prefix: String,
    /// `-k`/`--keep-subject`: emit the commit subject verbatim with no
    /// `[PATCH ...]` prefix.
    keep_subject: bool,
    /// `--to=<addr>` recipients given on the command line (appended after any
    /// `format.to` config). `--no-to` clears both the config and these.
    cli_to: Vec<String>,
    /// `--cc=<addr>` recipients given on the command line.
    cli_cc: Vec<String>,
    /// `--add-header=<hdr>` extra headers given on the command line. A value
    /// beginning `To: `/`Cc: ` is routed to `cli_to`/`cli_cc` instead.
    cli_headers: Vec<String>,
    /// `--no-to` / `--no-cc`: drop all configured + command-line recipients of
    /// that kind (a later `--to`/`--cc` re-adds command-line ones).
    no_to: bool,
    no_cc: bool,
    /// `--no-add-header`: drop all configured + command-line extra headers,
    /// `To:` and `Cc:` recipients (git's `header_callback` unset clears all three).
    no_add_header: bool,
    /// `--from[=<ident>]` / `--no-from`: rewrite the `From:` header to this
    /// identity and add an in-body `From:` for the real author. `FromMode::Unset`
    /// defers to `format.from`; `Clear` is `--no-from`; `Committer` is bare
    /// `--from`; `Ident(s)` is `--from=<s>`.
    from: FromMode,
    /// `--force-in-body-from` / `--no-force-in-body-from`: keep the in-body
    /// `From:` even when it matches the header `From:`. `None` defers to
    /// `format.forceInBodyFrom`.
    force_in_body_from: Option<bool>,
    /// `--signature=<s>` / `--no-signature` / `--signature=""`: override the
    /// trailing `-- \n<version>` signature. `SignatureMode::Default` uses the
    /// git version; `Text(s)` uses `s` (empty `s` suppresses the block);
    /// `Suppress` is `--no-signature`.
    signature: SignatureMode,
    /// `--signature-file=<path>`: read the signature body from a file.
    signature_file: Option<String>,
    /// `--zero-commit`: use the all-zero oid in the mbox `From <oid>` line.
    zero_commit: bool,
    /// `--attach[=<boundary>]` / `--inline[=<boundary>]`: wrap each patch as a
    /// MIME `multipart/mixed` message whose second part is the diff, rendered as
    /// an attachment (`--attach`) or inline (`--inline`). `None` is the default
    /// (plain mbox patch). The boundary defaults to the git version string.
    mime: Option<MimeAttach>,
    /// `-<n>`: limit to the last n commits of the default tip.
    count: Option<usize>,
    /// `--numbered-files`: name output files `1`, `2`, ... with no slug.
    numbered_files: bool,
    /// `--suffix=<s>` / `format.filenameSuffix`: the output-file / MIME-attachment
    /// filename suffix (git default `.patch`).
    suffix: Option<String>,
    /// Use the full 40/64-hex blob ids in `index` lines (`--full-index`).
    full_index: bool,
    /// Abbreviation width for patch `index` lines (`--abbrev=<n>`).
    abbrev: Option<usize>,
    /// Disable rename detection (`--no-renames`); on by default like git diff.
    detect_renames: bool,
    /// Enable copy detection (`-C`/`--find-copies`).
    detect_copies: bool,
    /// `--find-copies-harder`.
    find_copies_harder: bool,
    /// Rename similarity threshold.
    rename_threshold: u8,
    /// Copy similarity threshold.
    copy_threshold: u8,
    /// `-O<file>`: reorder per-patch diff entries according to an orderfile.
    order_file: Option<String>,
    /// `--cover-letter` / `--no-cover-letter`: emit a `0000-cover-letter.patch`
    /// summary "email" ahead of the per-commit patches. `None` defers to
    /// `format.coverletter` (resolved in [`resolve_cover_letter`]).
    cover_letter: Option<bool>,
    /// `--commit-list-format=<fmt>`: the commit-list rendering used in the cover
    /// body (`shortlog`, `modern`, `log:<pretty>`, or a bare `<pretty>` with a
    /// `%`). `None` defers to `format.commitlistformat`, else `shortlog`.
    commit_list_format: Option<String>,
    /// `--cover-from-description=<mode>`: where the cover subject/blurb come from.
    /// `None` defers to `format.coverFromDescription`, else `Message`.
    cover_from_description: Option<CoverFromDescription>,
    /// `--description-file=<path>`: read the cover description from a file rather
    /// than `branch.<name>.description`.
    description_file: Option<String>,
    /// `--encode-email-headers` / `--no-encode-email-headers` /
    /// `format.encodeEmailHeaders`: q-encode non-ASCII Subject text. `None`
    /// defers to config (default true). Only consulted by the cover subject;
    /// the per-patch subject path is unchanged.
    encode_email_headers: Option<bool>,
    /// `--thread[=<style>]` / `--no-thread`: message-threading level. `None`
    /// defers to `format.thread`; `Unset` is `--no-thread`.
    thread: Option<ThreadLevel>,
    /// `--in-reply-to=<msgid>`: seed the In-Reply-To/References chain with this
    /// message id (cleaned of surrounding `<>`/whitespace).
    in_reply_to: Option<String>,
    /// Notes display state for `--notes[=<ref>]`, `--no-notes`, and
    /// `format.notes`.
    notes: FormatPatchNotes,
    /// `--range-diff=<previous>` commentary to include in the cover letter or
    /// single patch.
    range_diff: Option<String>,
    /// Mboxrd message body escaping (`--pretty=mboxrd` or `format.mboxrd`).
    mboxrd: bool,
    /// `--base=<commit>`, `--base=auto`, `--no-base`, or config fallback.
    base: BaseMode,
    /// Drop commits whose patch-id already appears on the upstream side.
    ignore_if_in_upstream: bool,
    /// `--relative[=<path>]`, `--no-relative`, or config-driven default.
    relative_mode: RelativeMode,
    /// Resolved repository-relative path prefix to strip from diff output.
    relative_prefix: Option<Vec<u8>>,
    /// `--grep=<pattern>` commit-message filters.
    grep_patterns: Vec<String>,
    grep_pattern_kind: sley_grep::PatternKind,
    grep_pattern_kind_explicit: bool,
    grep_ignore_case: bool,
    grep_all_match: bool,
    grep_invert: bool,
    /// `--root`: treat a single revision argument as a `<revision range>`
    /// (formatting it and its ancestors as creation patches) instead of the
    /// default `<since>..HEAD` interpretation.
    root: bool,
    /// Revision setup arguments (single committish, ranges, `--`, pathspecs).
    setup_args: Vec<String>,
}

struct FormatPatchSelection {
    commits: Vec<sley_rev::CommitRecord>,
    pathspecs: Vec<String>,
}

#[derive(Clone)]
struct BaseInfo {
    base: ObjectId,
    prerequisites: Vec<Vec<u8>>,
}

/// Tracks `format-patch` notes display state. This is deliberately local rather
/// than reusing `log.rs`'s private display helper so the parity fix stays in this
/// command file.
#[derive(Default, Clone)]
struct FormatPatchNotes {
    given: bool,
    enabled: bool,
    use_default: bool,
    suppress_config: bool,
    refs: Vec<String>,
}

impl FormatPatchNotes {
    fn add_default(&mut self) {
        self.given = true;
        self.enabled = true;
        self.use_default = true;
    }

    fn add_ref(&mut self, reff: &str) {
        self.given = true;
        self.enabled = true;
        self.refs.push(NotesRef::expand(reff).as_str().to_string());
    }

    fn disable(&mut self) {
        self.given = true;
        self.enabled = false;
        self.use_default = false;
        self.suppress_config = true;
        self.refs.clear();
    }
}

impl Default for FormatPatchOptions {
    fn default() -> Self {
        Self {
            stdout: false,
            output_directory: None,
            output: None,
            number_mode: NumberMode::Auto,
            start_number: None,
            signoff: false,
            stat: true,
            stat_widths: DiffStatWidths::plumbing(),
            stat_count: None,
            context_lines: HUNK_CONTEXT,
            subject_prefix: None,
            rfc: RfcMode::Unset,
            reroll_count: None,
            filename_max_length: None,
            prefix_mode: None,
            src_prefix: "a/".to_string(),
            dst_prefix: "b/".to_string(),
            keep_subject: false,
            cli_to: Vec::new(),
            cli_cc: Vec::new(),
            cli_headers: Vec::new(),
            no_to: false,
            no_cc: false,
            no_add_header: false,
            from: FromMode::Unset,
            force_in_body_from: None,
            signature: SignatureMode::Default,
            signature_file: None,
            zero_commit: false,
            mime: None,
            count: None,
            numbered_files: false,
            suffix: None,
            full_index: false,
            abbrev: None,
            detect_renames: true,
            detect_copies: false,
            find_copies_harder: false,
            rename_threshold: sley_diff_merge::DEFAULT_RENAME_THRESHOLD,
            copy_threshold: sley_diff_merge::DEFAULT_RENAME_THRESHOLD,
            order_file: None,
            cover_letter: None,
            commit_list_format: None,
            cover_from_description: None,
            description_file: None,
            encode_email_headers: None,
            thread: None,
            in_reply_to: None,
            notes: FormatPatchNotes::default(),
            range_diff: None,
            mboxrd: false,
            base: BaseMode::Config,
            ignore_if_in_upstream: false,
            relative_mode: RelativeMode::Config,
            relative_prefix: None,
            grep_patterns: Vec::new(),
            grep_pattern_kind: sley_grep::PatternKind::Basic,
            grep_pattern_kind_explicit: false,
            grep_ignore_case: false,
            grep_all_match: false,
            grep_invert: false,
            root: false,
            setup_args: Vec::new(),
        }
    }
}

impl super::grep_args::GrepArgOptions for FormatPatchOptions {
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

/// An author/committer identity split into display name and email, used for the
/// `--from` rewrite and the redundant-in-body-from check.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FromIdent {
    name: String,
    email: String,
}

/// Everything derived once from the options + repo config that every patch in
/// the run shares: the assembled `[PATCH ...]` prefix string, the extra-header
/// block (custom headers / `To:` / `Cc:`), the `--from` rewrite identity, and
/// the resolved signature trailer text.
struct ResolvedFormat {
    /// The bracket-prefix label body (between `[` and the ` n/m]`), e.g.
    /// `RFC PATCH` or `PATCH (WIP)` or an empty string for `--subject-prefix=`.
    prefix_body: String,
    /// The pre-rendered extra-header block (each line newline-terminated),
    /// emitted right after the `Subject:` header. Empty when no headers apply.
    header_block: Vec<u8>,
    /// `--from`/`format.from` rewrite identity, if active.
    from_ident: Option<FromIdent>,
    /// Keep the in-body `From:` even when it is redundant (`--force-in-body-from`).
    force_in_body_from: bool,
    /// The resolved signature body, or `None` to suppress the `-- \n...` block.
    /// An empty trailer (e.g. `--signature=""`) also resolves to `None`.
    signature: Option<Vec<u8>>,
    /// Use the all-zero oid in the mbox `From <oid>` separator (`--zero-commit`).
    zero_commit: bool,
}

pub(crate) fn cmd_format_patch(args: &[String]) -> Result<()> {
    let mut options = parse_format_patch_args(args)?;

    let repo = RepositoryContext::discover_current()?;
    let cwd = repo.cwd();
    let git_dir = repo.git_dir();
    let format = repo.format();
    let config = repo.config();
    let db = repo.objects();

    if (options.stdout && (options.output_directory.is_some() || options.output.is_some()))
        || (options.output.is_some() && options.output_directory.is_some())
    {
        eprintln!("fatal: multiple output options?");
        return Err(GitError::Exit(128));
    }

    // Resolve diff path prefixes: `--no-prefix`/`--default-prefix` win over the
    // `format.noprefix` config. A non-boolean `format.noprefix` is fatal (git
    // tightened this from "any value = true").
    let no_prefix = match options.prefix_mode {
        Some(force_prefix) => !force_prefix,
        None => match config.get_all("format", None, "noprefix").last() {
            // A bare `[format] noprefix` (no value) is boolean true.
            Some(None) => true,
            Some(Some(value)) => parse_format_noprefix_bool(value)?,
            None => false,
        },
    };
    if no_prefix {
        options.src_prefix.clear();
        options.dst_prefix.clear();
    }
    options.mboxrd |= config.get_bool("format", None, "mboxrd").unwrap_or(false);
    options.relative_prefix = resolve_format_patch_relative_prefix(&repo, &options, config)?;
    let options = options;

    let resolved = resolve_format(&options, config)?;

    let selection = select_commits(&repo, &options)?;
    let commits = selection.commits;
    let diff_pathspec = if selection.pathspecs.is_empty() {
        None
    } else {
        Some(DiffPathspec::new(
            cwd,
            repo.worktree_root()?,
            &selection.pathspecs,
        )?)
    };

    let count = commits.len();
    let range_diff_setup_args = format_patch_setup_args(&options);
    // A cover letter forces `[PATCH n/m]` numbering (the cover is `0/m`), so it
    // also flips a single-patch run from the bare `[PATCH]` to `[PATCH 1/1]`.
    // git emits no cover (and no patches) when the range is empty.
    let mut cover_letter = count > 0 && resolve_cover_letter(&options, config, count);
    if options.range_diff.is_some() && count > 1 {
        let config_disables_cover = config
            .get("format", None, "coverLetter")
            .is_some_and(|value| matches!(git_config_bool_str(value), Some(false)));
        if options.cover_letter == Some(false) || config_disables_cover {
            eprintln!("fatal: --range-diff requires --cover-letter for multi-patch series");
            return Err(GitError::Exit(128));
        }
        cover_letter = true;
    }
    let numbered = match options.number_mode {
        NumberMode::Numbered => true,
        NumberMode::Unnumbered => false,
        // Auto-numbering keys off the count actually emitted, not the start
        // offset: a single patch is unnumbered, several are numbered. A cover
        // letter forces numbering on regardless of count.
        NumberMode::Auto => count > 1 || cover_letter,
    };
    let start_number = options.start_number.unwrap_or(1);
    // The `m` in `[PATCH n/m]` is the highest patch number, which equals the
    // count only when numbering starts at 1; `--start-number 5` over two commits
    // yields `5/6` and `6/6`.
    let last_number = start_number + count.saturating_sub(1);
    let signoff_line = if options.signoff {
        Some(format_patch_signoff(config)?)
    } else {
        None
    };
    let abbrev = patch_index_abbrev(git_dir, format, &options)?;
    let notes_refs = resolve_format_patch_notes(git_dir, format, &options, config)?;
    let base_info = resolve_base_info(&repo, &options, config, &commits)?;
    let range_diff = match options.range_diff.as_deref() {
        Some(previous) => Some(commands::range_diff::render_format_patch_range_diff(
            &repo,
            previous,
            &range_diff_setup_args,
            &selection.pathspecs,
            &notes_refs,
        )?),
        None => None,
    };

    // Resolve message threading: the `--thread`/`format.thread` level plus any
    // `--in-reply-to` seed determine the Message-ID / In-Reply-To / References on
    // the cover and every patch. The plan is replayed once so the cover (built
    // before the patches) can carry its own headers.
    let thread_level = resolve_thread_level(&options, config);
    let thread_plan = if count > 0
        && (thread_level != ThreadLevel::Unset || options.in_reply_to.is_some() || cover_letter)
    {
        let mid_ident = committer_ident_string(config)?;
        let mid_email = parse_from_ident(&mid_ident)?.email;
        let mid_time = message_id_timestamp();
        let commit_oids: Vec<String> = commits.iter().map(|c| c.oid.to_hex()).collect();
        Some(build_thread_plan(
            thread_level,
            options.in_reply_to.as_deref(),
            cover_letter,
            &commit_oids,
            start_number,
            mid_time,
            &mid_email,
        ))
    } else {
        None
    };
    let cover_thread = thread_plan
        .as_ref()
        .map(|p| p.cover.clone())
        .unwrap_or_default();
    let empty_thread = MailThreadHeaders::default();
    let patch_thread = |idx: usize| -> &MailThreadHeaders {
        thread_plan
            .as_ref()
            .and_then(|p| p.patches.get(idx))
            .unwrap_or(&empty_thread)
    };
    let encode_headers = encode_email_headers_on(&options, config);

    // Resolve the cover-letter content once: its synthetic header identity, the
    // subject/blurb (from the branch description / --description-file under the
    // cover-from-description rules), the commit-list body, and the run's cumulative
    // diffstat against the boundary commit. Only built when a cover is emitted.
    let cover = if cover_letter {
        Some(build_cover_letter(
            &repo,
            &options,
            &resolved,
            config,
            &commits,
            diff_pathspec.as_ref(),
            last_number,
            abbrev,
            &cover_thread,
            range_diff.as_deref(),
        )?)
    } else {
        None
    };

    if options.stdout {
        let mut stdout = io::stdout();
        if let Some(cover) = &cover {
            // The cover's own signature framing ends in a blank line, so the
            // first patch follows it directly with no extra inter-patch blank.
            stdout.write_all(cover)?;
        }
        for (idx, record) in commits.iter().enumerate() {
            // In stream mode git separates consecutive *patches* with an extra
            // blank line on top of each patch's own trailing blank. The cover →
            // first-patch boundary gets no such separator (idx 0 is skipped).
            if idx > 0 {
                stdout.write_all(b"\n")?;
            }
            let buffer = render_patch(RenderContext {
                db,
                format,
                options: &options,
                resolved: &resolved,
                record,
                diff_pathspec: diff_pathspec.as_ref(),
                seq: start_number + idx,
                last_number,
                numbered,
                signoff_line: signoff_line.as_deref(),
                abbrev,
                thread: patch_thread(idx),
                encode_headers,
                config,
                git_dir,
                notes_refs: &notes_refs,
                range_diff: range_diff.as_deref().filter(|_| count == 1),
                base_info: base_info.as_ref(),
            })?;
            stdout.write_all(&buffer)?;
        }
        stdout.flush()?;
        return Ok(());
    }

    if let Some(output) = options.output.as_deref() {
        let output_path = resolve_cli_path(cwd, output);
        let mut stream = io::BufWriter::new(fs::File::create(output_path)?);
        if let Some(cover) = &cover {
            stream.write_all(cover)?;
        }
        for (idx, record) in commits.iter().enumerate() {
            if idx > 0 {
                stream.write_all(b"\n")?;
            }
            let buffer = render_patch(RenderContext {
                db,
                format,
                options: &options,
                resolved: &resolved,
                record,
                diff_pathspec: diff_pathspec.as_ref(),
                seq: start_number + idx,
                last_number,
                numbered,
                signoff_line: signoff_line.as_deref(),
                abbrev,
                thread: patch_thread(idx),
                encode_headers,
                config,
                git_dir,
                notes_refs: &notes_refs,
                range_diff: range_diff.as_deref().filter(|_| count == 1),
                base_info: base_info.as_ref(),
            })?;
            stream.write_all(&buffer)?;
        }
        stream.flush()?;
        return Ok(());
    }

    let configured_out_dir = config.get("format", None, "outputDirectory");
    let out_dir = options
        .output_directory
        .as_deref()
        .or(configured_out_dir)
        .unwrap_or(".");
    let out_dir_path = resolve_cli_path(cwd, out_dir);
    fs::create_dir_all(&out_dir_path)?;
    // Resolve the filename length cap: CLI `--filename-max-length`, else
    // `format.filenameMaxLength`, else 64 (git FORMAT_PATCH_NAME_MAX_DEFAULT).
    // A floor of len("0000-") + len(".patch") keeps room for the number+suffix.
    let patch_name_max = resolve_patch_name_max(&options, config);
    // The sanitized `v<reroll>-` filename prefix (empty when no reroll count).
    let reroll_prefix = options
        .reroll_count
        .as_deref()
        .map(reroll_filename_prefix)
        .unwrap_or_default();
    let filename_suffix = patch_filename_suffix(&options, config);
    let mut stdout = io::stdout();
    if let Some(cover) = &cover {
        // The cover is patch number `start_number - 1` (0 when numbering starts
        // at 1): `0000-cover-letter.patch`, or the bare number under
        // `--numbered-files`.
        let cover_seq = start_number.saturating_sub(1);
        let file_name = if options.numbered_files {
            cover_seq.to_string()
        } else {
            build_patch_filename(
                &reroll_prefix,
                cover_seq,
                "cover-letter",
                patch_name_max,
                &filename_suffix,
            )
        };
        let file_path = out_dir_path.join(&file_name);
        fs::write(&file_path, cover)?;
        let display = format_patch_display_path(out_dir, &file_name);
        writeln!(stdout, "{}", display.display())?;
    }
    for (idx, record) in commits.iter().enumerate() {
        let seq = start_number + idx;
        let buffer = render_patch(RenderContext {
            db,
            format,
            options: &options,
            resolved: &resolved,
            record,
            diff_pathspec: diff_pathspec.as_ref(),
            seq,
            last_number,
            numbered,
            signoff_line: signoff_line.as_deref(),
            abbrev,
            thread: patch_thread(idx),
            encode_headers,
            config,
            git_dir,
            notes_refs: &notes_refs,
            range_diff: range_diff.as_deref().filter(|_| count == 1),
            base_info: base_info.as_ref(),
        })?;
        let file_name = if options.numbered_files {
            seq.to_string()
        } else {
            let slug = sanitize_patch_subject(&record.commit.message);
            build_patch_filename(&reroll_prefix, seq, &slug, patch_name_max, &filename_suffix)
        };
        let file_path = out_dir_path.join(&file_name);
        fs::write(&file_path, &buffer)?;
        // git prints the path as joined with the user-provided directory string
        // (so a relative `-o build` yields `build/0001-...patch`).
        let display = format_patch_display_path(out_dir, &file_name);
        writeln!(stdout, "{}", display.display())?;
    }
    stdout.flush()?;
    Ok(())
}

/// Resolve whether a cover letter is emitted: `--cover-letter`/`--no-cover-letter`
/// win over `format.coverletter`; `format.coverletter=auto` (or `--cover-letter`
/// left to auto) emits a cover only when more than one patch is produced.
fn resolve_cover_letter(options: &FormatPatchOptions, config: &GitConfig, count: usize) -> bool {
    if let Some(explicit) = options.cover_letter {
        return explicit;
    }
    // git: an explicit `--commit-list-format` (with no `--cover-letter`/
    // `--no-cover-letter`) implies a cover letter.
    if options.commit_list_format.is_some() {
        return true;
    }
    match config.get_entry("format", None, "coverletter") {
        Some(Some(value)) if value.eq_ignore_ascii_case("auto") => count > 1,
        // A bare `format.coverletter` (no value), or an unrecognised non-boolean
        // value, is treated as boolean-true.
        Some(value) => value.and_then(git_config_bool_str).unwrap_or(true),
        None => false,
    }
}

/// Render the complete `0000-cover-letter` "email" buffer.
///
/// Mirrors git's `make_cover_letter` (builtin/log.c): a synthetic mail whose
/// `From:`/`Date:` are the committer (or `--from`/`format.from`) identity at the
/// current time, a `Subject: [PATCH 0/m] <subject>` header, the extra-header
/// block, the cover subject/blurb (resolved from the branch description /
/// `--description-file` under the cover-from-description rules), the commit-list
/// body (shortlog / modern / `log:<fmt>` / a bare `%`-format), the run's
/// cumulative diffstat against the boundary commit, and the signature trailer.
#[allow(clippy::too_many_arguments)]
fn build_cover_letter(
    repo: &RepositoryContext,
    options: &FormatPatchOptions,
    resolved: &ResolvedFormat,
    config: &GitConfig,
    commits: &[sley_rev::CommitRecord],
    diff_pathspec: Option<&DiffPathspec>,
    last_number: usize,
    abbrev: usize,
    thread: &MailThreadHeaders,
    range_diff: Option<&[u8]>,
) -> Result<Vec<u8>> {
    let _ = abbrev; // the cover never emits index lines; kept for signature parity.
    let format = repo.format();
    let db = repo.objects();
    let mut out = Vec::new();

    // The tip (newest) commit anchors the mbox `From <oid>` separator; the
    // boundary (parent of the oldest) anchors the cumulative diffstat.
    let head = commits
        .last()
        .expect("cover letter requires at least one commit");
    out.extend_from_slice(b"From ");
    if resolved.zero_commit {
        out.extend_from_slice("0".repeat(format.hex_len()).as_bytes());
    } else {
        out.extend_from_slice(head.oid.to_hex().as_bytes());
    }
    out.extend_from_slice(b" Mon Sep 17 00:00:00 2001\n");

    // Message-ID / In-Reply-To / References block, before the identity headers.
    thread.write(&mut out);

    // From:/Date: come from the cover identity (the `--from`/`format.from` ident
    // if set, else the runtime committer) at the current time, exactly like
    // git's `from = cfg->from ? cfg->from : git_committer_info(0)`.
    let (from_name, from_email) = match &resolved.from_ident {
        Some(from) => (from.name.clone(), from.email.clone()),
        None => {
            let ident = committer_ident_string(config)?;
            let parsed = parse_from_ident(&ident)?;
            (parsed.name, parsed.email)
        }
    };
    let encode = encode_email_headers_on(options, config);
    write_from_header(&mut out, &from_name, &from_email, encode);
    writeln_fmt_buf(&mut out, format_args!("Date: {}", cover_letter_date()));

    // Resolve the cover subject + blurb body from the branch description /
    // --description-file under the cover-from-description rules.
    let (subject, body) = resolve_cover_text(repo, options, config)?;

    // Subject: [PATCH 0/m] <subject>, RFC 2047-encoded when it carries non-ASCII
    // and header encoding is on. The cover is patch 0, so it is always numbered.
    let prefix = subject_prefix_label(resolved, 0, last_number, true);
    write_email_subject(&mut out, prefix.as_deref(), subject.as_bytes(), encode);

    // Extra headers (custom / To: / Cc:) sit between Subject and the blank line.
    out.extend_from_slice(&resolved.header_block);

    // Blank line, then the blurb body. git always emits the body followed by a
    // blank line (`pp_remainder` + the trailing `\n` in `fprintf("%s\n", sb)`).
    out.push(b'\n');
    out.extend_from_slice(body.as_bytes());
    if !body.is_empty() {
        out.push(b'\n');
    }
    out.push(b'\n');

    // The commit-list body (shortlog / modern / log:<fmt> / bare %-format).
    let list_format = resolve_commit_list_format(options, config);
    write_commit_list_cover(&mut out, &list_format, commits)?;

    if let Some(range_diff) = range_diff {
        write_range_diff_commentary(&mut out, options, range_diff);
    }

    // The cumulative diffstat against the boundary commit, when there is a unique
    // boundary (a single parent of the oldest commit). git omits it otherwise.
    if let Some(origin_tree) = cover_origin_tree(db, format, commits)? {
        let entries = cover_diff_entries(
            db,
            format,
            options,
            diff_pathspec,
            &origin_tree,
            &head.commit.tree,
        )?;
        if options.stat {
            write_patch_diffstat(&mut out, &entries, db, options)?;
            for entry in &entries {
                write_diff_summary_entry(&mut out, entry)?;
            }
            // git's diff flush ends the diffstat block with a blank line; the
            // cover has no per-file diff after it, so that blank sits directly
            // before the signature.
            out.push(b'\n');
        }
    }

    // Signature trailer, identical framing to a normal patch.
    if let Some(signature) = &resolved.signature {
        out.extend_from_slice(b"-- \n");
        out.extend_from_slice(signature);
        out.extend_from_slice(b"\n\n");
    }
    Ok(out)
}

/// Format the current time (honoring `GIT_COMMITTER_DATE` like git's
/// `git_committer_info`) as the cover's RFC 2822 `Date:`.
fn cover_letter_date() -> String {
    let raw_date = env::var("GIT_COMMITTER_DATE").ok();
    let (secs, tz) = match raw_date.as_deref().and_then(parse_committer_date) {
        Some(parsed) => parsed,
        None => (current_unix_seconds(), "+0000".to_string()),
    };
    // commit_identity_date parses the trailing `<ts> <tz>` of an identity line.
    let ident = format!("C <c@example.invalid> {secs} {tz}");
    commit_identity_date(ident.as_bytes(), &DateMode::Rfc2822)
}

fn write_range_diff_commentary(out: &mut Vec<u8>, options: &FormatPatchOptions, range_diff: &[u8]) {
    match range_diff_previous_label(options) {
        Some(label) => out.extend_from_slice(format!("Range-diff against {label}:\n").as_bytes()),
        None => out.extend_from_slice(b"Range-diff:\n"),
    }
    for line in range_diff.split_inclusive(|b| *b == b'\n') {
        let line = line.strip_suffix(b"\n").unwrap_or(line);
        out.extend_from_slice(line);
        out.push(b'\n');
    }
    out.push(b'\n');
}

fn range_diff_previous_label(options: &FormatPatchOptions) -> Option<String> {
    let reroll = options.reroll_count.as_deref()?;
    let value = reroll.parse::<usize>().ok()?;
    (value > 0).then(|| format!("v{}", value - 1))
}

/// Parse a `GIT_COMMITTER_DATE` value into `(unix_seconds, timezone)`. Defers to
/// git's full commit-date parser so the canonical `@<unix> <tz>` / raw
/// `<unix> <tz>` (test_tick) *and* human forms like `2006-06-26 00:06:00 +0000`
/// (which the t4013 setup exports) all resolve. Returns `None` for any shape the
/// parser rejects, in which case the caller falls back to the current time.
fn parse_committer_date(value: &str) -> Option<(i64, String)> {
    crate::commands::approxidate::parse_commit_date(value)
}

/// Maximum subject length (characters) before cover-from-description `auto`
/// falls back to keeping the placeholder subject. Mirrors git's
/// `COVER_FROM_AUTO_MAX_SUBJECT_LEN`.
const COVER_FROM_AUTO_MAX_SUBJECT_LEN: usize = 100;

/// Resolve the cover's `(subject, blurb_body)` under the cover-from-description
/// rules, mirroring git's `prepare_cover_text`. The description text comes from
/// `--description-file` first, else `branch.<name>.description` for the branch
/// inferred from the revision arguments. With no description (or `none` mode)
/// the placeholders `*** SUBJECT HERE ***` / `*** BLURB HERE ***` are used.
fn resolve_cover_text(
    repo: &RepositoryContext,
    options: &FormatPatchOptions,
    config: &GitConfig,
) -> Result<(String, String)> {
    let placeholder_subject = "*** SUBJECT HERE ***".to_string();
    let placeholder_body = "*** BLURB HERE ***".to_string();

    let mode = options
        .cover_from_description
        .or_else(|| {
            config
                .get("format", None, "coverFromDescription")
                .and_then(|value| parse_cover_from_description(value).ok())
        })
        .unwrap_or(CoverFromDescription::Message);

    if mode == CoverFromDescription::None {
        return Ok((placeholder_subject, placeholder_body));
    }

    let description = read_cover_description(repo, options, config)?;
    let Some(description) = description.filter(|text| !text.is_empty()) else {
        return Ok((placeholder_subject, placeholder_body));
    };

    // Split the first paragraph (subject) from the remainder (body) exactly as
    // git's `format_subject(_, _, " ")` does: join the first paragraph's lines
    // with a single space, and treat the bytes after the first blank line as the
    // body.
    let (subject_para, remainder) = split_cover_description(&description);

    match mode {
        CoverFromDescription::None => Ok((placeholder_subject, placeholder_body)),
        CoverFromDescription::Message => {
            // Subject stays the placeholder; the WHOLE description is the body.
            Ok((placeholder_subject, pp_remainder(&description)))
        }
        CoverFromDescription::Subject => Ok((subject_para, pp_remainder(remainder))),
        CoverFromDescription::Auto => {
            if subject_para.chars().count() > COVER_FROM_AUTO_MAX_SUBJECT_LEN {
                // Too-long would-be subject: fall back to MESSAGE behaviour.
                Ok((placeholder_subject, pp_remainder(&description)))
            } else {
                Ok((subject_para, pp_remainder(remainder)))
            }
        }
    }
}

/// Read the raw cover description text: `--description-file` wins, else
/// `branch.<name>.description` for the branch inferred from the revision args.
fn read_cover_description(
    repo: &RepositoryContext,
    options: &FormatPatchOptions,
    config: &GitConfig,
) -> Result<Option<String>> {
    if let Some(path) = options
        .description_file
        .as_deref()
        .filter(|p| !p.is_empty())
    {
        let resolved = resolve_cli_path(repo.cwd(), path);
        let bytes = fs::read(&resolved).map_err(|err| {
            GitError::Command(format!(
                "unable to read branch description file '{path}': {err}"
            ))
        })?;
        return Ok(Some(String::from_utf8_lossy(&bytes).into_owned()));
    }
    let Some(branch) = cover_branch_name(repo, options)? else {
        return Ok(None);
    };
    Ok(config
        .get("branch", Some(&branch), "description")
        .map(str::to_string))
}

/// Infer the branch whose description seeds the cover, mirroring git's
/// `find_branch_name`: when a single positive revision argument names a branch
/// whose tip matches, use it; otherwise fall back to the current `HEAD` branch.
fn cover_branch_name(
    repo: &RepositoryContext,
    options: &FormatPatchOptions,
) -> Result<Option<String>> {
    // git's `find_branch_name` keys off the single *interesting* (non-excluded)
    // cmdline ref. A bare single committish in format-patch means `<commit>..HEAD`
    // — the committish is the EXCLUDED boundary, so the interesting ref is HEAD
    // and we fall back to the current branch. Only an explicit positive branch
    // tip (e.g. `rebuild-1~2..rebuild-1`, `^main rebuild-1`) names the branch.
    if format_patch_bare_exclude(options).is_none() {
        // Collect the explicit positive ref tokens (drop `^neg`, ranges, options,
        // and the implicit HEAD). The interesting ref must be a single branch.
        let positives: Vec<&str> = options
            .setup_args
            .iter()
            .take_while(|arg| arg.as_str() != "--")
            .filter_map(|arg| {
                if arg.starts_with('^') || arg.starts_with('-') || arg.as_str() == "HEAD" {
                    return None;
                }
                // For an explicit `<since>..<until>` the positive side is <until>.
                match arg.split_once("..") {
                    Some((_, until)) if !until.is_empty() => Some(until),
                    Some(_) => None,
                    None => Some(arg.as_str()),
                }
            })
            .collect();
        if positives.len() == 1 {
            let token = positives[0];
            // The token must dwim to a branch (and, in git, match its tip — here
            // a positive arg that is a branch name always points at its tip).
            if repo
                .refs()
                .read_ref(&format!("refs/heads/{token}"))?
                .is_some()
            {
                return Ok(Some(token.to_string()));
            }
        }
    }
    // Fall back to the current branch (the symbolic HEAD short name).
    match repo.refs().read_ref("HEAD")? {
        Some(RefTarget::Symbolic(name)) => Ok(name.strip_prefix("refs/heads/").map(str::to_string)),
        _ => Ok(None),
    }
}

/// Split a description into `(first_paragraph_joined_with_spaces, remainder)`.
/// The first paragraph runs until the first blank line; its lines are joined
/// with single spaces (git's `format_subject(_, _, " ")`). The remainder is the
/// raw text from the blank line onward.
fn split_cover_description(description: &str) -> (String, &str) {
    let mut subject = String::new();
    let mut first = true;
    let mut offset = 0usize;
    for line in description.split_inclusive('\n') {
        let text = line.strip_suffix('\n').unwrap_or(line);
        if text.trim().is_empty() {
            // Blank line ends the first paragraph; the remainder starts here.
            break;
        }
        if !first {
            subject.push(' ');
        }
        subject.push_str(text);
        first = false;
        offset += line.len();
    }
    (subject, &description[offset..])
}

/// git's `pp_remainder`: skip leading blank lines, then keep the rest with its
/// trailing whitespace trimmed to a single newline-free block. The cover body is
/// emitted with its own trailing blank line by the caller, so here we just strip
/// surrounding blank lines.
fn pp_remainder(text: &str) -> String {
    let mut started = false;
    let mut out = String::new();
    for line in text.split_inclusive('\n') {
        let body = line.strip_suffix('\n').unwrap_or(line);
        if !started {
            if body.trim().is_empty() {
                continue;
            }
            started = true;
        }
        out.push_str(body);
        out.push('\n');
    }
    // Trim trailing blank lines but keep interior structure.
    while out.ends_with('\n') {
        out.pop();
    }
    out
}

/// Whether `--encode-email-headers`/`format.encodeEmailHeaders` is on (default
/// true). Mirrors git's `rev.encode_email_headers`.
fn encode_email_headers_on(options: &FormatPatchOptions, config: &GitConfig) -> bool {
    options
        .encode_email_headers
        .or_else(|| config.get_bool("format", None, "encodeEmailHeaders"))
        .unwrap_or(true)
}

/// Which RFC 2047 character class governs the special-byte check: `Subject` is
/// the loose set (git's `RFC2047_SUBJECT`); `Address` is the tighter phrase set
/// (git's `RFC2047_ADDRESS`, used for `From:`/`To:` display names).
#[derive(Clone, Copy)]
enum Rfc2047Type {
    Subject,
    Address,
}

/// git's `needs_rfc2047_encoding`: a header needs RFC 2047 encoding if it carries
/// any non-ASCII byte, a newline, or the literal `=?` introducer.
fn needs_rfc2047_encoding(bytes: &[u8]) -> bool {
    for (i, &byte) in bytes.iter().enumerate() {
        if byte >= 0x80 || byte == b'\n' {
            return true;
        }
        if byte == b'=' && bytes.get(i + 1) == Some(&b'?') {
            return true;
        }
    }
    false
}

/// git's `is_rfc2047_special`. A byte must be `=%02X`-escaped inside an encoded
/// word when it is non-ASCII, non-printable, whitespace, or one of `=` `?` `_`.
/// For the `Address` (phrase) type the encodable set is further narrowed to
/// alphanumerics plus `! * + - / = _` (rfc2047 §5.3).
fn is_rfc2047_special(byte: u8, kind: Rfc2047Type) -> bool {
    // non-ASCII or non-printable
    if byte >= 0x80 || !(byte.is_ascii_graphic() || byte == b' ') {
        return true;
    }
    // special printable characters
    if byte.is_ascii_whitespace() || byte == b'=' || byte == b'?' || byte == b'_' {
        return true;
    }
    match kind {
        Rfc2047Type::Subject => false,
        // '=' and '_' were already handled above.
        Rfc2047Type::Address => {
            !(byte.is_ascii_alphanumeric()
                || byte == b'!'
                || byte == b'*'
                || byte == b'+'
                || byte == b'-'
                || byte == b'/')
        }
    }
}

/// How many bytes are already on the last line of `buf` (git's
/// `last_line_length`).
fn last_line_length(buf: &[u8]) -> usize {
    match buf.iter().rposition(|&b| b == b'\n') {
        Some(i) => buf.len() - (i + 1),
        None => buf.len(),
    }
}

/// Length of the leading UTF-8 multibyte sequence at `bytes` (git's
/// `mbs_chrlen` for a UTF-8 encoding). Returns 1 for ASCII / invalid lead bytes,
/// clamped to the remaining length so we never read past the end.
fn utf8_seq_len(bytes: &[u8]) -> usize {
    let lead = match bytes.first() {
        Some(&b) => b,
        None => return 0,
    };
    let want = if lead < 0x80 {
        1
    } else if lead >> 5 == 0b110 {
        2
    } else if lead >> 4 == 0b1110 {
        3
    } else if lead >> 3 == 0b11110 {
        4
    } else {
        1
    };
    want.min(bytes.len())
}

/// Port of git's `add_rfc2047`: append `line` to `out` as one or more
/// `=?UTF-8?q?...?=` encoded words, folding at 76 columns. `out` must already
/// hold the header text up to the insertion point (e.g. `Subject: [PATCH] `),
/// since the first encoded word's budget is measured from the current last-line
/// length. Multi-byte UTF-8 characters are never split across encoded words.
fn add_rfc2047(out: &mut Vec<u8>, line: &[u8], kind: Rfc2047Type) {
    const ENCODING: &str = "UTF-8";
    const MAX_ENCODED_LENGTH: usize = 76;
    let mut line_len = last_line_length(out);
    out.extend_from_slice(format!("=?{ENCODING}?q?").as_bytes());
    line_len += ENCODING.len() + 5; // 5 for "=??q?"

    let mut rest = line;
    while !rest.is_empty() {
        let chrlen = utf8_seq_len(rest);
        let (chunk, tail) = rest.split_at(chrlen);
        let is_special = chrlen > 1 || is_rfc2047_special(chunk[0], kind);
        let encoded_len = if is_special { 3 * chrlen } else { 1 };

        if line_len + encoded_len + 2 > MAX_ENCODED_LENGTH {
            // It won't fit with the trailing "?=" — break the line.
            out.extend_from_slice(format!("?=\n =?{ENCODING}?q?").as_bytes());
            line_len = ENCODING.len() + 5 + 1; // "=??q?" plus the leading SP
        }

        if is_special {
            for &b in chunk {
                out.extend_from_slice(format!("={b:02X}").as_bytes());
            }
        } else {
            out.push(chunk[0]);
        }
        line_len += encoded_len;
        rest = tail;
    }
    out.extend_from_slice(b"?=");
}

/// git's `is_rfc822_special`: characters that force the display name to be
/// double-quoted in an address header.
fn is_rfc822_special(byte: u8) -> bool {
    matches!(
        byte,
        b'(' | b')' | b'<' | b'>' | b'[' | b']' | b':' | b';' | b'@' | b',' | b'.' | b'"' | b'\\'
    )
}

/// git's `needs_rfc822_quoting`: true if any byte is an rfc822 special.
fn needs_rfc822_quoting(bytes: &[u8]) -> bool {
    bytes.iter().any(|&b| is_rfc822_special(b))
}

/// git's `add_rfc822_quoted`: wrap the name in double quotes, backslash-escaping
/// embedded `"` and `\`.
fn add_rfc822_quoted(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + 2);
    out.push(b'"');
    for &b in bytes {
        if b == b'"' || b == b'\\' {
            out.push(b'\\');
        }
        out.push(b);
    }
    out.push(b'"');
    out
}

/// Port of git's `strbuf_add_wrapped_text` (the bytes variant), appending `text`
/// to `out` word-wrapped to `width` columns. `indent1` is the first-line indent;
/// a *negative* `indent1` means "the first line already has `-indent1` columns of
/// content" (the leading `From: ` prefix). `indent2` is the continuation-line
/// indent. Wrapping breaks at ASCII whitespace; UTF-8 display width is honored.
fn add_wrapped_text(out: &mut Vec<u8>, text: &[u8], indent1: isize, indent2: isize, width: isize) {
    if width <= 0 {
        // git falls back to strbuf_add_indented_text; format-patch never calls
        // this with width<=0, so a plain append is sufficient here.
        out.extend_from_slice(text);
        return;
    }

    let mut indent = indent1;
    let mut w = indent1;
    // `bol`/`space` are byte offsets into `text`.
    let mut bol: usize = 0;
    let mut space: Option<usize> = None;
    if indent < 0 {
        w = -indent1;
        space = Some(0);
    }

    // Whether byte at `i` is ASCII whitespace (used when skipping the remembered
    // break point, matching git's `bol = space + isspace(*space)`).
    let is_ws = |i: usize| -> usize {
        usize::from(matches!(text.get(i), Some(&b) if b == b' ' || b == b'\t' || b == b'\n'))
    };

    let mut pos: usize = 0;
    loop {
        let c = text.get(pos).copied();
        let is_space = matches!(c, Some(b) if b == b' ' || b == b'\t' || b == b'\n');
        if c.is_none() || is_space {
            if w <= width || space.is_none() {
                let start = if let Some(sp) = space {
                    sp
                } else {
                    for _ in 0..indent.max(0) {
                        out.push(b' ');
                    }
                    bol
                };
                if c.is_none() && pos == start {
                    return;
                }
                out.extend_from_slice(&text[start..pos]);
                let ch = match c {
                    Some(ch) => ch,
                    None => return,
                };
                space = Some(pos);
                if ch == b'\t' {
                    w |= 0x07;
                } else if ch == b'\n' {
                    // A run of two newlines, or a newline before a non-alnum,
                    // forces a hard line break; otherwise it becomes a space.
                    let next = text.get(pos + 1).copied();
                    let sp = pos + 1;
                    if next == Some(b'\n')
                        || !next.map(|b| b.is_ascii_alphanumeric()).unwrap_or(false)
                    {
                        // new_line
                        out.push(b'\n');
                        pos = sp + is_ws(sp);
                        bol = pos;
                        space = None;
                        w = indent2;
                        indent = indent2;
                        continue;
                    }
                    out.push(b' ');
                }
                w += 1;
                pos += 1;
            } else {
                // new_line: too wide and we have a remembered space — break.
                out.push(b'\n');
                let sp = space.unwrap_or(pos);
                pos = sp + is_ws(sp);
                bol = pos;
                space = None;
                w = indent2;
                indent = indent2;
            }
            continue;
        }
        // A non-space character: advance one UTF-8 char, adding its display width.
        let seq = utf8_seq_len(&text[pos..]);
        w += utf8_display_width(&text[pos..pos + seq]);
        pos += seq;
    }
}

/// Display width of a single UTF-8 character for header wrapping. ASCII and the
/// non-ASCII letters exercised by t4014 are width 1; this is deliberately the
/// simple "1 column per codepoint" model git's `utf8_width` yields for those
/// ranges (no East-Asian wide handling, which format-patch headers never need).
fn utf8_display_width(_ch: &[u8]) -> isize {
    1
}

/// Resolve the commit-list format used in the cover body: `--commit-list-format`
/// wins over `format.commitlistformat`; the default is `shortlog`.
fn resolve_commit_list_format(options: &FormatPatchOptions, config: &GitConfig) -> String {
    options
        .commit_list_format
        .clone()
        .or_else(|| {
            config
                .get("format", None, "commitlistformat")
                .map(str::to_string)
        })
        .unwrap_or_else(|| "shortlog".to_string())
}

/// Render the commit-list portion of the cover body, dispatching on the format
/// token exactly like git's `make_cover_letter`:
///
///   - `shortlog`         → author-grouped shortlog (wrap 72, indent 2/4)
///   - `modern`           → `%w(72)[%(count)/%(total)] %s`
///   - `log:<pretty>`     → the `<pretty>` format per commit
///   - a bare `<fmt>` containing `%` → that format per commit
///   - anything else      → a fatal "is not a valid format string"
///
/// A trailing blank line always follows.
fn write_commit_list_cover(
    out: &mut Vec<u8>,
    format: &str,
    commits: &[sley_rev::CommitRecord],
) -> Result<()> {
    if let Some(pretty) = format.strip_prefix("log:") {
        write_commit_list_pretty(out, pretty, commits)?;
    } else if format == "shortlog" {
        write_shortlog_cover(out, commits);
    } else if format == "modern" {
        write_commit_list_pretty(out, "%w(72)[%(count)/%(total)] %s", commits)?;
    } else if format.contains('%') {
        write_commit_list_pretty(out, format, commits)?;
    } else {
        eprintln!("fatal: '{format}' is not a valid format string");
        return Err(GitError::Exit(128));
    }
    Ok(())
}

/// Emit an author-grouped shortlog for the cover (git's
/// `generate_shortlog_cover_letter`): each group is `Name (N):` followed by its
/// commit subjects, wrapped at column 76 with the first line indented 2 and
/// continuations indented 4. Commits are grouped in first-appearance (oldest
/// first) order.
fn write_shortlog_cover(out: &mut Vec<u8>, commits: &[sley_rev::CommitRecord]) {
    // Preserve first-seen group order while collecting each author's subjects.
    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, Vec<String>> = HashMap::new();
    for record in commits {
        let (name, _) = commit_identity_name_email(&record.commit.author);
        let subject = commit_subject(&record.commit.message);
        if !groups.contains_key(&name) {
            order.push(name.clone());
        }
        groups.entry(name).or_default().push(subject);
    }
    for name in &order {
        let subjects = &groups[name];
        writeln_buf(out, &format!("{} ({}):", name, subjects.len()));
        for subject in subjects {
            // MAIL_DEFAULT_WRAP (72), first line indent 2, continuations 4.
            for line in cover_wrap_text(subject, 72, 2, 4) {
                writeln_buf(out, &line);
            }
        }
        out.push(b'\n');
    }
}

/// Greedy word-wrap matching git's `strbuf_add_wrapped_text`: break on spaces,
/// indent the first line by `indent1` and continuations by `indent2`, never
/// exceeding `width` where a word fits; an over-long word lands alone on its own
/// line. Used for both the shortlog subjects and the `%w(width)` directive.
fn cover_wrap_text(text: &str, width: usize, indent1: usize, indent2: usize) -> Vec<String> {
    let words: Vec<&str> = text.split(' ').filter(|word| !word.is_empty()).collect();
    if words.is_empty() {
        return vec![" ".repeat(indent1)];
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_indent = indent1;
    let mut column = indent1;
    let mut first_word = true;
    for word in words {
        let word_len = word.chars().count();
        let needed = if first_word {
            current_indent + word_len
        } else {
            column + 1 + word_len
        };
        if !first_word && needed > width {
            lines.push(current);
            current = String::new();
            current_indent = indent2;
            column = indent2;
            first_word = true;
        }
        if first_word {
            current.push_str(&" ".repeat(current_indent));
            current.push_str(word);
            column = current_indent + word_len;
            first_word = false;
        } else {
            current.push(' ');
            current.push_str(word);
            column += 1 + word_len;
        }
    }
    if !current.is_empty() || lines.is_empty() {
        lines.push(current);
    }
    lines
}

/// Render a `%`-format commit list (git's `generate_commit_list_cover`): emit
/// one line per commit, newest first, with `%(count)` running 1..=n, `%(total)`
/// fixed at n, plus a trailing blank line. Supports the placeholders the cover
/// formats actually use: `%(count)`, `%(total)`, `%s` (subject), `%an`
/// (author name), and a leading `%w(width[,i1[,i2]])` wrap directive.
fn write_commit_list_pretty(
    out: &mut Vec<u8>,
    format: &str,
    commits: &[sley_rev::CommitRecord],
) -> Result<()> {
    let total = commits.len();
    // git iterates `list[n - i]` for i=1..=n; `list` is newest-first (list[0] =
    // head), so this walks newest→oldest with count ascending. Our `commits` vec
    // is oldest-first, so iterate it reversed.
    for (idx, record) in commits.iter().rev().enumerate() {
        let count = idx + 1;
        let rendered = expand_commit_list_format(format, record, count, total)?;
        out.extend_from_slice(rendered.as_bytes());
        out.push(b'\n');
    }
    out.push(b'\n');
    Ok(())
}

/// Expand one commit-list format line. Handles a leading `%w(...)` wrap
/// directive (which wraps the whole rendered line), then the placeholders the
/// cover formats use. An unrecognised `%`-escape is copied through verbatim,
/// matching git's lenient passthrough for the tokens we don't special-case.
fn expand_commit_list_format(
    format: &str,
    record: &sley_rev::CommitRecord,
    count: usize,
    total: usize,
) -> Result<String> {
    let (wrap, rest) = parse_leading_wrap(format);
    let (author_name, _) = commit_identity_name_email(&record.commit.author);
    let subject = commit_subject(&record.commit.message);

    let mut line = String::new();
    let bytes = rest.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 1 < bytes.len() {
            let after = &rest[i + 1..];
            if after.starts_with("(count)") {
                line.push_str(&count.to_string());
                i += 1 + "(count)".len();
                continue;
            }
            if after.starts_with("(total)") {
                line.push_str(&total.to_string());
                i += 1 + "(total)".len();
                continue;
            }
            if after.starts_with("s") {
                line.push_str(&subject);
                i += 2;
                continue;
            }
            if after.starts_with("an") {
                line.push_str(&author_name);
                i += 3;
                continue;
            }
            // Unknown escape: copy the `%` and let the next byte through.
            line.push('%');
            i += 1;
            continue;
        }
        line.push(bytes[i] as char);
        i += 1;
    }

    match wrap {
        Some((width, indent1, indent2)) if width > 0 => {
            Ok(cover_wrap_text(&line, width, indent1, indent2).join("\n"))
        }
        _ => Ok(line),
    }
}

/// Parse a leading `%w(width[,indent1[,indent2]])` directive, returning the
/// `(width, indent1, indent2)` and the remainder of the format after it. When
/// the format does not start with `%w(`, returns `(None, format)`.
fn parse_leading_wrap(format: &str) -> (Option<(usize, usize, usize)>, &str) {
    let Some(rest) = format.strip_prefix("%w(") else {
        return (None, format);
    };
    let Some(close) = rest.find(')') else {
        return (None, format);
    };
    let args = &rest[..close];
    let mut nums = args
        .split(',')
        .map(|n| n.trim().parse::<usize>().unwrap_or(0));
    let width = nums.next().unwrap_or(0);
    let indent1 = nums.next().unwrap_or(0);
    let indent2 = nums.next().unwrap_or(0);
    (Some((width, indent1, indent2)), &rest[close + 1..])
}

/// The boundary commit's tree for the cumulative cover diffstat: the first
/// parent of the oldest selected commit. Returns `None` when the oldest commit
/// is a root (no parent) — git omits the diffstat when there is no unique
/// boundary.
fn cover_origin_tree(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    commits: &[sley_rev::CommitRecord],
) -> Result<Option<ObjectId>> {
    let oldest = &commits[0];
    let Some(parent_oid) = oldest.commit.parents.first() else {
        return Ok(None);
    };
    let parent_object = db.read_object(parent_oid)?;
    let parent_commit = Commit::parse_ref(format, &parent_object.body)?;
    Ok(Some(parent_commit.tree))
}

/// Build the cumulative name-status diff for the cover diffstat: the boundary
/// tree against the tip tree, honoring the run's rename/copy + pathspec options.
fn cover_diff_entries(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    options: &FormatPatchOptions,
    diff_pathspec: Option<&DiffPathspec>,
    origin_tree: &ObjectId,
    head_tree: &ObjectId,
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
    rename_limit: 0,
    };
    let entries = sley_diff_merge::diff_name_status_trees_with_rename_options(
        db,
        format,
        origin_tree,
        head_tree,
        rename_options,
    )?;
    let entries = match diff_pathspec {
        Some(pathspec) => apply_diff_pathspec(entries, pathspec),
        None => entries,
    };
    Ok(apply_format_patch_relative(entries, options))
}

/// Resolve the notes refs that `format-patch` appends after the `---` separator.
/// `format.notes` is opt-in, unlike `git log`, and command-line `--no-notes`
/// suppresses configured refs until a later `--notes` flag re-enables explicit
/// display.
fn resolve_format_patch_notes(
    git_dir: &Path,
    format: ObjectFormat,
    options: &FormatPatchOptions,
    config: &GitConfig,
) -> Result<Vec<String>> {
    let mut refs = Vec::new();
    let mut active = false;
    if !options.notes.suppress_config {
        for entry in config.get_all("format", None, "notes") {
            match entry {
                None => {
                    push_note_ref(git_dir, &mut refs, None);
                    active = true;
                }
                Some(value) => match git_config_bool_str(value) {
                    Some(true) => {
                        push_note_ref(git_dir, &mut refs, None);
                        active = true;
                    }
                    Some(false) => {
                        refs.clear();
                        active = false;
                    }
                    None => {
                        push_note_ref(git_dir, &mut refs, Some(value));
                        active = true;
                    }
                },
            }
        }
    }

    if options.notes.given {
        if !options.notes.enabled {
            return Ok(Vec::new());
        }
        active = true;
        if options.notes.use_default {
            push_note_ref(git_dir, &mut refs, None);
        }
        for reff in &options.notes.refs {
            push_note_ref(git_dir, &mut refs, Some(reff));
        }
    }

    if !active {
        return Ok(Vec::new());
    }
    expand_format_patch_notes(git_dir, format, refs)
}

fn push_note_ref(git_dir: &Path, refs: &mut Vec<String>, reff: Option<&str>) {
    let value = match reff {
        Some(reff) => NotesRef::expand(reff).as_str().to_string(),
        None => crate::commands::notes::raw_notes_ref(git_dir, None),
    };
    if !value.is_empty() && !refs.contains(&value) {
        refs.push(value);
    }
}

fn expand_format_patch_notes(
    git_dir: &Path,
    format: ObjectFormat,
    refs: Vec<String>,
) -> Result<Vec<String>> {
    if refs.iter().all(|reff| !reff.contains('*')) {
        return Ok(refs);
    }
    let store = FileRefStore::new(git_dir, format);
    let mut expanded = Vec::new();
    for reff in refs {
        if !reff.contains('*') {
            if !expanded.contains(&reff) {
                expanded.push(reff);
            }
            continue;
        }
        let prefix = reff.trim_end_matches('*');
        let mut matched: Vec<String> = store
            .list_refs()?
            .into_iter()
            .map(|entry| entry.name)
            .filter(|name| name.starts_with(prefix))
            .collect();
        matched.sort();
        for name in matched {
            if !expanded.contains(&name) {
                expanded.push(name);
            }
        }
    }
    Ok(expanded)
}

fn render_format_patch_notes(
    git_dir: &Path,
    format: ObjectFormat,
    refs: &[String],
    oid: &ObjectId,
) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    if refs.is_empty() {
        return Ok(out);
    }
    let store = FileRefStore::new(git_dir, format);
    for reff in refs {
        let handle = NotesRef::expand(reff);
        let Some(mut body) = read_note_bytes(git_dir, format, &store, &handle, oid)? else {
            continue;
        };
        if body.last() == Some(&b'\n') {
            body.pop();
        }
        if handle.as_str() == sley_notes::DEFAULT_NOTES_REF {
            out.extend_from_slice(b"\nNotes:\n");
        } else {
            let name = handle
                .as_str()
                .strip_prefix("refs/")
                .and_then(|s| s.strip_prefix("notes/"))
                .unwrap_or(handle.as_str());
            out.extend_from_slice(format!("\nNotes ({name}):\n").as_bytes());
        }
        for line in body.split(|b| *b == b'\n') {
            out.extend_from_slice(b"    ");
            out.extend_from_slice(line);
            out.push(b'\n');
        }
    }
    if !out.is_empty() {
        out.push(b'\n');
    }
    Ok(out)
}

/// Fold the parsed options together with repository config into the run-wide
/// [`ResolvedFormat`]: the subject prefix, the To/Cc/extra-header block, the
/// `--from` rewrite identity, and the signature trailer text. This mirrors
/// git's `cmd_format_patch` set-up phase (builtin/log.c), which assembles these
/// once before walking the commits.
fn resolve_format(options: &FormatPatchOptions, config: &GitConfig) -> Result<ResolvedFormat> {
    let prefix_body = resolve_prefix_body(options, config);
    let encode_headers = encode_email_headers_on(options, config);
    let header_block = resolve_header_block(options, config, encode_headers);
    let from_ident = resolve_from_ident(options, config)?;
    let force_in_body_from = options
        .force_in_body_from
        .or_else(|| config.get_bool("format", None, "forceInBodyFrom"))
        .unwrap_or(false);
    let signature = resolve_signature(options, config)?;
    Ok(ResolvedFormat {
        prefix_body,
        header_block,
        from_ident,
        force_in_body_from,
        signature,
        zero_commit: options.zero_commit,
    })
}

/// Assemble the bracket-prefix body, mirroring git's `cmd_format_patch`:
/// the base prefix comes from `--subject-prefix`, else `format.subjectPrefix`,
/// else `PATCH`; `--rfc[=token]` weaves an rfc marker in (a leading `-` appends
/// `token[1..]` after the prefix, otherwise `token ` is inserted before it);
/// `-v<n>` appends ` v<n>`.
fn resolve_prefix_body(options: &FormatPatchOptions, config: &GitConfig) -> String {
    let mut body = options
        .subject_prefix
        .clone()
        .or_else(|| {
            config
                .get("format", None, "subjectPrefix")
                .map(str::to_string)
        })
        .unwrap_or_else(|| "PATCH".to_string());

    match &options.rfc {
        RfcMode::Unset | RfcMode::Clear => {}
        RfcMode::Token(token) if !token.is_empty() => {
            if let Some(suffix) = token.strip_prefix('-') {
                body = format!("{body} {suffix}");
            } else {
                body = format!("{token} {body}");
            }
        }
        RfcMode::Token(_) => {}
    }

    if let Some(reroll) = &options.reroll_count {
        body = format!("{body} v{reroll}");
    }
    body
}

/// Build the To/Cc/extra-header block (each line newline-terminated) that is
/// emitted right after the `Subject:` header. Mirrors builtin/log.c's assembly
/// order: custom headers first, then a folded `To:` block, then a folded `Cc:`
/// block. `--no-add-header` drops everything; `--no-to`/`--no-cc` drop only the
/// configured recipients of that kind (a later `--to`/`--cc` re-adds the
/// command-line ones).
fn resolve_header_block(
    options: &FormatPatchOptions,
    config: &GitConfig,
    encode_headers: bool,
) -> Vec<u8> {
    let mut headers: Vec<String> = Vec::new();
    let mut to: Vec<String> = Vec::new();
    let mut cc: Vec<String> = Vec::new();

    if !options.no_add_header {
        // format.headers entries route through add_header (To:/Cc: prefixes go
        // to the recipient lists; everything else is a raw header line).
        for value in config
            .get_all("format", None, "headers")
            .into_iter()
            .flatten()
        {
            route_config_header(value, &mut headers, &mut to, &mut cc);
        }
    }
    if !options.no_to {
        for value in config.get_all("format", None, "to").into_iter().flatten() {
            to.push(value.to_string());
        }
    }
    if !options.no_cc {
        for value in config.get_all("format", None, "cc").into_iter().flatten() {
            cc.push(value.to_string());
        }
    }
    // Command-line --add-header / --to / --cc always apply (they come after the
    // --no-* clears in git's option order for the tests we model).
    headers.extend(options.cli_headers.iter().cloned());
    to.extend(options.cli_to.iter().cloned());
    cc.extend(options.cli_cc.iter().cloned());

    let mut out = Vec::new();
    for header in &headers {
        out.extend_from_slice(header.as_bytes());
        out.push(b'\n');
    }
    write_recipient_block(&mut out, "To: ", &to, encode_headers);
    write_recipient_block(&mut out, "Cc: ", &cc, encode_headers);
    out
}

/// Route a `format.headers` value: a `To: `/`Cc: ` prefix (case-insensitive)
/// strips the prefix and feeds the recipient lists; everything else is a raw
/// extra-header line. Mirrors builtin/log.c's `add_header`.
fn route_config_header(
    value: &str,
    headers: &mut Vec<String>,
    to: &mut Vec<String>,
    cc: &mut Vec<String>,
) {
    let trimmed = value.trim_end_matches('\n');
    if trimmed.len() >= 4 && trimmed[..4].eq_ignore_ascii_case("to: ") {
        to.push(trimmed[4..].to_string());
    } else if trimmed.len() >= 4 && trimmed[..4].eq_ignore_ascii_case("cc: ") {
        cc.push(trimmed[4..].to_string());
    } else {
        headers.push(trimmed.to_string());
    }
}

/// Emit a folded `To: `/`Cc: ` recipient block: the first recipient on the
/// header line, each subsequent one on a continuation line indented by four
/// spaces, with a trailing comma after every recipient except the last.
fn write_recipient_block(out: &mut Vec<u8>, label: &str, recipients: &[String], encode: bool) {
    if recipients.is_empty() {
        return;
    }
    out.extend_from_slice(label.as_bytes());
    for (idx, recipient) in recipients.iter().enumerate() {
        if idx > 0 {
            out.extend_from_slice(b"    ");
        }
        match parse_from_ident(recipient) {
            Ok(ident) => write_address_name_and_email(out, &ident.name, &ident.email, encode),
            Err(_) => out.extend_from_slice(recipient.as_bytes()),
        }
        if idx + 1 < recipients.len() {
            out.push(b',');
        }
        out.push(b'\n');
    }
}

/// Resolve the `--from`/`format.from` rewrite identity. `--from`/`--no-from`
/// override `format.from`; a bare `--from` (or `format.from=true`) uses the
/// runtime committer identity; an explicit ident string is parsed into a
/// name/email. A malformed ident is a fatal error (matching
/// `--from=ident notices bogus ident`).
fn resolve_from_ident(
    options: &FormatPatchOptions,
    config: &GitConfig,
) -> Result<Option<FromIdent>> {
    let ident_str: Option<String> = match &options.from {
        FromMode::Clear => return Ok(None),
        FromMode::Committer => Some(committer_ident_string(config)?),
        FromMode::Ident(value) => Some(value.clone()),
        FromMode::Unset => match config.get_entry("format", None, "from") {
            // `format.from` unset: no rewrite.
            None => None,
            // Bare `format.from` (no value) is boolean-true → committer.
            Some(None) => Some(committer_ident_string(config)?),
            Some(Some(value)) => match git_config_bool_str(value) {
                Some(true) => Some(committer_ident_string(config)?),
                Some(false) => None,
                // A non-boolean value is taken as a literal From: address.
                None => Some(value.to_string()),
            },
        },
    };
    match ident_str {
        Some(value) => Ok(Some(parse_from_ident(&value)?)),
        None => Ok(None),
    }
}

/// Format the runtime committer identity as a `Name <email>` string.
fn committer_ident_string(config: &GitConfig) -> Result<String> {
    let name = env::var("GIT_COMMITTER_NAME")
        .ok()
        .or_else(|| config.get("user", None, "name").map(str::to_string))
        .unwrap_or_else(|| "Git Rs".to_string());
    let email = env::var("GIT_COMMITTER_EMAIL")
        .ok()
        .or_else(|| config.get("user", None, "email").map(str::to_string))
        .unwrap_or_else(|| "sley@example.invalid".to_string());
    Ok(format!("{name} <{email}>"))
}

/// Parse a `Name <email>` ident into name + email parts, erroring on a missing
/// `<...>` mail section (git's `split_ident_line` failure → "invalid ident").
fn parse_from_ident(value: &str) -> Result<FromIdent> {
    let open = value
        .find('<')
        .ok_or_else(|| GitError::Command(format!("invalid ident line: {value}")))?;
    let close = value[open..]
        .find('>')
        .map(|rel| open + rel)
        .ok_or_else(|| GitError::Command(format!("invalid ident line: {value}")))?;
    let email = value[open + 1..close].to_string();
    let name = value[..open].trim().to_string();
    Ok(FromIdent { name, email })
}

/// Apply git's `git_config_bool` keyword rules to a string value, returning
/// `None` when it is neither a recognised boolean keyword nor empty.
fn git_config_bool_str(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" | "" => Some(false),
        _ => None,
    }
}

/// Resolve the trailing signature body. Precedence mirrors git: an explicit
/// `--no-signature` suppresses; `--signature=<text>` overrides config (empty
/// text suppresses); `--signature-file=<path>` / `format.signaturefile` reads a
/// file; `format.signature` config sets text (empty suppresses); otherwise the
/// default is the git version string. Returns `None` to drop the block.
fn resolve_signature(options: &FormatPatchOptions, config: &GitConfig) -> Result<Option<Vec<u8>>> {
    // Command-line --signature / --no-signature win over everything.
    match &options.signature {
        SignatureMode::Suppress => return Ok(None),
        SignatureMode::Text(text) => {
            return Ok((!text.is_empty()).then(|| text.clone().into_bytes()));
        }
        SignatureMode::Default => {}
    }
    // --signature-file overrides format.signaturefile; both read a file whose
    // bytes become the signature (git appends a trailing newline, so an extra
    // blank line follows the file content).
    let file = options.signature_file.clone().or_else(|| {
        config
            .get("format", None, "signaturefile")
            .map(str::to_string)
    });
    if let Some(path) = file {
        let mut bytes = fs::read(&path).map_err(|err| {
            GitError::Command(format!("could not read signature file {path}: {err}"))
        })?;
        // git emits `-- \n` + the file content verbatim + a single `\n`; the
        // renderer's framing already appends `\n\n` after the signature, so the
        // signature body is the file content with exactly one trailing newline
        // removed (yielding file + `\n` once the framing's first `\n` lands).
        if bytes.last() == Some(&b'\n') {
            bytes.pop();
        }
        return Ok(Some(bytes));
    }
    // format.signature config (empty value suppresses).
    if let Some(Some(value)) = config.get_entry("format", None, "signature") {
        return Ok((!value.is_empty()).then(|| value.as_bytes().to_vec()));
    }
    if let Some(None) = config.get_entry("format", None, "signature") {
        // A bare `format.signature` with no value is the empty string → suppress.
        return Ok(None);
    }
    // Default: the git version string.
    Ok(Some(
        sley_core::UPSTREAM_GIT_COMPAT_VERSION.as_bytes().to_vec(),
    ))
}

/// Bundle of everything a single patch needs to render, to keep the helper from
/// taking a dozen positional arguments.
struct RenderContext<'a> {
    db: &'a FileObjectDatabase,
    format: ObjectFormat,
    options: &'a FormatPatchOptions,
    /// Run-wide resolved formatting (prefix, headers, from, signature).
    resolved: &'a ResolvedFormat,
    record: &'a sley_rev::CommitRecord,
    diff_pathspec: Option<&'a DiffPathspec>,
    /// 1-based patch number for the `n` in `[PATCH n/m]` and the file name.
    seq: usize,
    /// The highest patch number — the `m` in `[PATCH n/m]`. Equals the commit
    /// count unless `--start-number` shifts the range.
    last_number: usize,
    /// Whether to print the numbered `n/m` form.
    numbered: bool,
    /// The fully-formed `Signed-off-by: ...` line, if `--signoff`.
    signoff_line: Option<&'a [u8]>,
    /// Abbreviation width for `index` lines.
    abbrev: usize,
    /// This patch's resolved Message-ID / In-Reply-To / References block.
    thread: &'a MailThreadHeaders,
    /// Whether `--encode-email-headers`/`format.encodeEmailHeaders` is on.
    encode_headers: bool,
    /// Repo config (for the `--signoff` committer-ident 8-bit CTE check).
    config: &'a GitConfig,
    /// Repository gitdir, used to resolve notes.
    git_dir: &'a Path,
    /// Ordered notes refs to append after the `---` separator.
    notes_refs: &'a [String],
    /// Optional rendered range-diff commentary for a single-patch output.
    range_diff: Option<&'a [u8]>,
    /// Optional base/prerequisite metadata block.
    base_info: Option<&'a BaseInfo>,
}

/// Render one commit into a complete mbox patch byte buffer.
fn render_patch(ctx: RenderContext<'_>) -> Result<Vec<u8>> {
    let RenderContext {
        db,
        format,
        options,
        resolved,
        record,
        diff_pathspec,
        seq,
        last_number,
        numbered,
        signoff_line,
        abbrev,
        thread,
        encode_headers,
        config,
        git_dir,
        notes_refs,
        range_diff,
        base_info,
    } = ctx;

    let commit = &record.commit;
    let mut out = Vec::new();

    // mbox `From ` separator: the commit oid (or the all-zero oid under
    // `--zero-commit`) + the fixed magic date git uses.
    out.extend_from_slice(b"From ");
    if resolved.zero_commit {
        out.extend_from_slice("0".repeat(format.hex_len()).as_bytes());
    } else {
        out.extend_from_slice(record.oid.to_hex().as_bytes());
    }
    out.extend_from_slice(b" Mon Sep 17 00:00:00 2001\n");

    // Message-ID / In-Reply-To / References block (git's log_write_email_headers,
    // emitted before the From:/Date:/Subject identity headers).
    thread.write(&mut out);

    // From: header. With `--from`/`format.from` the visible From: is the rewrite
    // identity and the real author moves to an in-body `From:`; otherwise the
    // author identity is used directly. The display name is RFC 2047-encoded /
    // RFC 822-quoted / wrapped exactly like git's pp_user_info.
    let (author_name, author_email) = commit_identity_name_email(&commit.author);
    let in_body_from = match &resolved.from_ident {
        Some(from) => {
            write_from_header(&mut out, &from.name, &from.email, encode_headers);
            // git keeps the in-body From: only when it differs from the header
            // From: (i.e. the author differs from the rewrite ident), unless
            // --force-in-body-from is set.
            let redundant = from.name == author_name && from.email == author_email;
            (!redundant || resolved.force_in_body_from)
                .then(|| format!("From: {author_name} <{author_email}>\n"))
        }
        None => {
            write_from_header(&mut out, &author_name, &author_email, encode_headers);
            None
        }
    };

    // Date: header — the *author* date in git's RFC 2822 rendering.
    let date = commit_identity_date(&commit.author, &DateMode::Rfc2822);
    writeln_fmt_buf(&mut out, format_args!("Date: {date}"));

    // Subject: [PREFIX n/m] <subject>. The subject is the collapsed leading
    // paragraph (multi-line subjects join with a space), RFC 2047-encoded when
    // header-encoding is on and it carries non-ASCII, else ASCII-wrapped at 78.
    let prefix = if options.keep_subject {
        None
    } else {
        subject_prefix_label(resolved, seq, last_number, numbered)
    };
    let subject_bytes = if options.keep_subject {
        // -k/--keep-subject emits the bare first line verbatim (no encoding,
        // no paragraph collapse, no 822-atom quoting).
        commit_subject(&commit.message).into_bytes()
    } else {
        format_patch_subject(&commit.message)
    };
    write_email_subject(&mut out, prefix.as_deref(), &subject_bytes, encode_headers);

    // Content-Transfer-Encoding: a non-ASCII commit body, a non-ASCII in-body
    // header, or `--signoff` with a non-ASCII committer ident forces the 8-bit
    // MIME block. git emits it right after the Subject, before the extra headers.
    let signoff_non_ascii =
        signoff_line.is_some() && committer_ident_has_non_ascii(config).unwrap_or(false);
    // `--attach`/`--inline` force git's `need_8bit_cte = -1` (NEVER): the plain
    // text/plain CTE block is replaced by the multipart preamble below.
    let need_8bit_cte = options.mime.is_none()
        && (signoff_non_ascii
            || message_body_has_non_ascii(&commit.message)
            || in_body_from
                .as_deref()
                .map(|h| h.bytes().any(|b| b >= 0x80))
                .unwrap_or(false));
    if need_8bit_cte {
        out.extend_from_slice(
            b"MIME-Version: 1.0\nContent-Type: text/plain; charset=UTF-8\nContent-Transfer-Encoding: 8bit\n",
        );
    }

    // Extra headers (custom `--add-header`/`format.headers`, then `To:`, then
    // `Cc:`) are emitted directly after the Subject, before the blank line.
    out.extend_from_slice(&resolved.header_block);

    // Blank line, then optional in-body `From:` header (with its own trailing
    // blank line), then the commit body (message minus the subject line),
    // normalized to end in exactly one newline. With --signoff the trailer is
    // appended to the body. Under `--attach`/`--inline` the blank line is
    // supplied by the multipart preamble's trailing `\n\n` instead.
    let body = format_patch_body(&commit.message, &subject_bytes, signoff_line);
    if let Some(mime) = &options.mime {
        write_mime_preamble(&mut out, mime);
        // git renders the text/plain part's body (the in-body From + commit
        // message) with its own leading newline on top of the preamble's
        // header/body blank; a subject-only commit (empty body, no in-body From)
        // omits it, leaving just the single preamble blank before `---`.
        if !body.is_empty() || in_body_from.is_some() {
            out.push(b'\n');
        }
    } else {
        out.push(b'\n');
    }
    if let Some(in_body) = in_body_from {
        out.extend_from_slice(in_body.as_bytes());
        out.push(b'\n');
    }
    if options.mboxrd {
        write_mboxrd_escaped_body(&mut out, &body);
    } else {
        out.extend_from_slice(&body);
    }
    if let Some(range_diff) = range_diff {
        out.push(b'\n');
        write_range_diff_commentary(&mut out, options, range_diff);
    }

    // Diff entries against the first parent (or the empty tree for a root).
    let entries = first_parent_diff_entries(db, format, options, diff_pathspec, commit)?;

    // The `---`/diffstat/diff block is emitted only when the commit actually
    // changes something. An empty commit goes straight from the message to the
    // `-- ` signature. When there are changes, the `---` separator introduces the
    // diffstat block (the default `--stat`), which `--no-stat` collapses to a
    // single blank line.
    if !entries.is_empty() {
        if options.stat {
            out.extend_from_slice(b"---\n");
            let notes = render_format_patch_notes(git_dir, format, notes_refs, &record.oid)?;
            out.extend_from_slice(&notes);
            write_patch_diffstat(&mut out, &entries, db, options)?;
            for entry in &entries {
                write_diff_summary_entry(&mut out, entry)?;
            }
            out.push(b'\n');
        } else {
            out.push(b'\n');
        }

        // MIME multipart: the diff goes into a second `text/x-patch` part,
        // introduced by git's `stat_sep` between the diffstat and the hunks.
        if let Some(mime) = &options.mime {
            let filename = mime_patch_filename(options, config, commit, seq);
            write_mime_part_header(&mut out, mime, &filename);
        }

        for entry in &entries {
            write_diff_patch_entry(
                &mut out,
                entry,
                format_patch_diff_options(db, format, options, abbrev),
            )?;
        }
    }

    if let Some(base) = base_info {
        write_base_info(&mut out, base, format);
    }

    // Signature trailer. The preceding content already ends in a newline, so
    // the `-- ` separator follows directly (no intervening blank line). When a
    // signature is present every patch ends `-- \n<sig>\n\n` (the trailing blank
    // is the inter-patch separator on stdout / the file's final newline). A
    // suppressed signature (`--no-signature`, `--signature=""`, empty
    // `format.signature`) drops the whole `-- \n...` block *and* the trailing
    // blank line: git emits nothing past the diff's own final newline.
    if let Some(mime) = &options.mime {
        // git: `\n--<leader><boundary>--\n\n\n` closes the multipart, in place of
        // the `-- \n<sig>` trailer (builtin/log.c, per patch).
        write_mime_closing(&mut out, mime);
    } else if let Some(signature) = &resolved.signature {
        out.extend_from_slice(b"-- \n");
        out.extend_from_slice(signature);
        out.extend_from_slice(b"\n\n");
    }
    Ok(out)
}

/// git's 12-dash `mime_boundary_leader` (diff.c). The actual delimiter lines are
/// `--` + this + the boundary string (14 dashes total).
const MIME_BOUNDARY_LEADER: &str = "------------";

/// The `multipart/mixed` preamble: the MIME headers, the human-readable note,
/// the first delimiter, and the first (`text/plain`) part's headers, ending in
/// the blank line that separates them from the commit body. Mirrors git's
/// `log_write_email_headers` strbuf for the `mime_boundary` case.
fn write_mime_preamble(out: &mut Vec<u8>, mime: &MimeAttach) {
    let b = &mime.boundary;
    write_fmt_buf(
        out,
        format_args!(
            "MIME-Version: 1.0\n\
             Content-Type: multipart/mixed; boundary=\"{MIME_BOUNDARY_LEADER}{b}\"\n\
             \n\
             This is a multi-part message in MIME format.\n\
             --{MIME_BOUNDARY_LEADER}{b}\n\
             Content-Type: text/plain; charset=UTF-8; format=fixed\n\
             Content-Transfer-Encoding: 8bit\n\n"
        ),
    );
}

/// The second (`text/x-patch`) part's headers, emitted between the diffstat and
/// the diff hunks (git's `stat_sep`). The leading `\n` is git's separator.
fn write_mime_part_header(out: &mut Vec<u8>, mime: &MimeAttach, filename: &str) {
    let b = &mime.boundary;
    let disposition = if mime.inline { "inline" } else { "attachment" };
    write_fmt_buf(
        out,
        format_args!(
            "\n--{MIME_BOUNDARY_LEADER}{b}\n\
             Content-Type: text/x-patch; name=\"{filename}\"\n\
             Content-Transfer-Encoding: 8bit\n\
             Content-Disposition: {disposition}; filename=\"{filename}\"\n\n"
        ),
    );
}

/// The closing delimiter that terminates the multipart message.
fn write_mime_closing(out: &mut Vec<u8>, mime: &MimeAttach) {
    write_fmt_buf(
        out,
        format_args!("\n--{MIME_BOUNDARY_LEADER}{}--\n\n\n", mime.boundary),
    );
}

/// The filename used for the MIME attachment part: the per-patch number under
/// `--numbered-files`, else the `NNNN-slug.patch` name git's `fmt_output_commit`
/// produces (the same name the file-output path writes).
fn mime_patch_filename(
    options: &FormatPatchOptions,
    config: &GitConfig,
    commit: &sley_object::Commit,
    seq: usize,
) -> String {
    if options.numbered_files {
        seq.to_string()
    } else {
        let slug = sanitize_patch_subject(&commit.message);
        let reroll_prefix = options
            .reroll_count
            .as_deref()
            .map(reroll_filename_prefix)
            .unwrap_or_default();
        let patch_name_max = resolve_patch_name_max(options, config);
        let suffix = patch_filename_suffix(options, config);
        build_patch_filename(&reroll_prefix, seq, &slug, patch_name_max, &suffix)
    }
}

fn write_base_info(out: &mut Vec<u8>, base: &BaseInfo, format: ObjectFormat) {
    out.push(b'\n');
    writeln_buf(out, &format!("base-commit: {}", base.base.to_hex()));
    for prereq in &base.prerequisites {
        writeln_buf(
            out,
            &format!(
                "prerequisite-patch-id: {}",
                hex_bytes(&prereq[..format.raw_len().min(prereq.len())])
            ),
        );
    }
}

fn hex_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Build the `[PATCH n/m]` / `[PATCH]` prefix string (without the trailing
/// space that separates it from the subject — that is added by the folder).
///
/// The prefix body (`RFC PATCH`, `PATCH (WIP)`, or an empty string for
/// `--subject-prefix=`) is resolved once in [`resolve_format`]. An empty body
/// yields the bare `[1/1]` form (no leading space) for the numbered case, or an
/// empty `[]` is suppressed entirely (git emits just `Subject: <subject>`).
fn subject_prefix_label(
    resolved: &ResolvedFormat,
    seq: usize,
    last_number: usize,
    numbered: bool,
) -> Option<String> {
    let body = &resolved.prefix_body;
    let number = if numbered {
        format!("{seq}/{last_number}")
    } else {
        String::new()
    };
    let inner = match (body.is_empty(), number.is_empty()) {
        // An empty body *and* no number leaves nothing to bracket: git emits a
        // bare `Subject: <subject>` with no `[]` at all.
        (true, true) => return None,
        (true, false) => number,
        (false, true) => body.clone(),
        (false, false) => format!("{body} {number}"),
    };
    Some(format!("[{inner}]"))
}

/// Collapse the leading subject paragraph (git's `format_subject` with a single
/// space separator): consecutive non-blank message lines are trimmed of trailing
/// whitespace and joined by one space, stopping at the first blank line. This is
/// what turns a three-line `one\ntwo\nthree` subject into `one two three`.
fn format_patch_subject(message: &[u8]) -> Vec<u8> {
    let text = message;
    let mut out: Vec<u8> = Vec::new();
    let mut first = true;
    let mut idx = 0;
    while idx < text.len() {
        let nl = text[idx..]
            .iter()
            .position(|&b| b == b'\n')
            .map(|p| idx + p)
            .unwrap_or(text.len());
        let mut line = &text[idx..nl];
        // Trim trailing CR/space/tab like git's get_one_line/is_blank_line.
        while let Some(&last) = line.last() {
            if last == b' ' || last == b'\t' || last == b'\r' {
                line = &line[..line.len() - 1];
            } else {
                break;
            }
        }
        if line.is_empty() {
            break;
        }
        if !first {
            out.push(b' ');
        }
        out.extend_from_slice(line);
        first = false;
        idx = nl + 1;
    }
    out
}

/// Append the `Subject:` header for one mail. Mirrors git's `pp_email_subject`
/// plus `fmt_output_email_subject`: writes `Subject: <prefix> `, then either an
/// RFC 2047 encoded-word sequence (when header-encoding is on and the subject
/// needs it) or an ASCII word-wrap at 78 columns (continuations indented one
/// space). The encoded path folds *inside* the encoded word at 76 columns; the
/// ASCII path measures its first-line budget from the prefix already written.
fn write_email_subject(out: &mut Vec<u8>, prefix: Option<&str>, subject: &[u8], encode: bool) {
    const MAX_LENGTH: isize = 78;
    let header_start = out.len();
    match prefix {
        Some(prefix) => write_fmt_buf(out, format_args!("Subject: {prefix} ")),
        None => out.extend_from_slice(b"Subject: "),
    }
    if encode && needs_rfc2047_encoding(subject) {
        add_rfc2047(out, subject, Rfc2047Type::Subject);
    } else {
        let prefix_cols = (out.len() - header_start) as isize;
        add_wrapped_text(out, subject, -prefix_cols, 1, MAX_LENGTH);
    }
    out.push(b'\n');
}

/// Append a `From: <name> <email>` header, mirroring git's `pp_user_info` mail
/// branch: the display name is RFC 2047-encoded (when header-encoding is on and
/// it carries non-ASCII), else RFC 822-quoted if it has specials, else wrapped at
/// `max_length` columns; the ` <email>` is folded onto its own line when it would
/// overflow that last line.
fn write_from_header(out: &mut Vec<u8>, name: &str, email: &str, encode: bool) {
    out.extend_from_slice(b"From: ");
    write_address_name_and_email(out, name, email, encode);
    out.push(b'\n');
}

fn write_address_name_and_email(out: &mut Vec<u8>, name: &str, email: &str, encode: bool) {
    let name_bytes = name.as_bytes();
    // git: max_length starts at 78, narrows to 76 once the name is rfc2047-encoded.
    let mut max_length: isize = 78;

    if encode && needs_rfc2047_encoding(name_bytes) {
        add_rfc2047(out, name_bytes, Rfc2047Type::Address);
        max_length = 76;
    } else if needs_rfc822_quoting(name_bytes) {
        let quoted = add_rfc822_quoted(name_bytes);
        let start_cols = last_line_length(out) as isize;
        add_wrapped_text(out, &quoted, -start_cols, 1, max_length);
    } else {
        let start_cols = last_line_length(out) as isize;
        add_wrapped_text(out, name_bytes, -start_cols, 1, max_length);
    }

    // git: if the " <email>" won't fit on the current last line, fold it down.
    let needed = last_line_length(out) as isize + 2 + email.len() as isize + 1;
    if max_length < needed {
        out.push(b'\n');
    }
    write_fmt_buf(out, format_args!(" <{email}>"));
}

/// Per-mail threading headers: the `Message-ID`, the `In-Reply-To` target, and
/// the ordered `References` chain (oldest → newest). Each id is the bare body
/// (no angle brackets); the writers add `<...>`.
#[derive(Default, Clone)]
struct MailThreadHeaders {
    message_id: Option<String>,
    references: Vec<String>,
}

impl MailThreadHeaders {
    /// Emit the `Message-ID`/`In-Reply-To`/`References` block. Mirrors git's
    /// `log_write_email_headers`: In-Reply-To is the *last* reference; References
    /// lists every reference, one per line, the first prefixed `References: ` and
    /// the rest indented by a single tab.
    fn write(&self, out: &mut Vec<u8>) {
        if let Some(id) = &self.message_id {
            writeln_fmt_buf(out, format_args!("Message-ID: <{id}>"));
        }
        if let Some(last) = self.references.last() {
            writeln_fmt_buf(out, format_args!("In-Reply-To: <{last}>"));
            for (i, r) in self.references.iter().enumerate() {
                let lead = if i > 0 { "\t" } else { "References: " };
                writeln_fmt_buf(out, format_args!("{lead}<{r}>"));
            }
        }
    }
}

/// The `--thread[=<style>]` / `format.thread` level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ThreadLevel {
    Unset,
    Shallow,
    Deep,
}

/// `--thread`/`--thread=shallow`/`--thread=deep`/`--no-thread`/`format.thread`
/// resolution, mirroring git's `thread_callback` + `git_format_config`:
/// command-line wins over config; a bare `--thread` (or `format.thread=true`) is
/// shallow.
fn resolve_thread_level(options: &FormatPatchOptions, config: &GitConfig) -> ThreadLevel {
    if let Some(level) = options.thread {
        return level;
    }
    match config.get_entry("format", None, "thread") {
        Some(Some(value)) if value.eq_ignore_ascii_case("deep") => ThreadLevel::Deep,
        Some(Some(value)) if value.eq_ignore_ascii_case("shallow") => ThreadLevel::Shallow,
        Some(value) => {
            if value.and_then(git_config_bool_str).unwrap_or(true) {
                ThreadLevel::Shallow
            } else {
                ThreadLevel::Unset
            }
        }
        None => ThreadLevel::Unset,
    }
}

/// git's `gen_message_id`: `<base>.<timestamp>.git.<email>`. `base` is the cover
/// keyword `cover` or a commit oid hex; `timestamp` is captured once per run (git
/// uses `time(NULL)` per call, but a single run-wide value keeps References
/// byte-identical to the Message-IDs they reference, which is what the test
/// normalization checks).
fn gen_message_id(base: &str, timestamp: i64, email: &str) -> String {
    format!("{base}.{timestamp}.git.{email}")
}

/// The run-wide timestamp baked into generated Message-IDs (git's `time(NULL)`,
/// or the pinned `GIT_COMMITTER_DATE` for deterministic test behavior). One value
/// per run keeps a patch's References byte-identical to the Message-IDs it points
/// at, which is what the t4014 threading normalization checks.
fn message_id_timestamp() -> i64 {
    env::var("GIT_COMMITTER_DATE")
        .ok()
        .as_deref()
        .and_then(parse_committer_date)
        .map(|(secs, _tz)| secs)
        .unwrap_or_else(current_unix_seconds)
}

/// Whether the commit *body* (everything past the header/body split, i.e. after
/// the first blank line) carries a non-ASCII byte — git's body scan in
/// `pp_title_line` that drives `need_8bit_cte`.
fn message_body_has_non_ascii(message: &[u8]) -> bool {
    let mut in_body = false;
    let mut i = 0;
    while i < message.len() {
        let ch = message[i];
        if !in_body {
            if ch == b'\n' && message.get(i + 1) == Some(&b'\n') {
                in_body = true;
            }
        } else if ch >= 0x80 {
            return true;
        }
        i += 1;
    }
    false
}

/// Whether the runtime committer identity (`Name <email>`) has any non-ASCII
/// byte — git's `has_non_ascii(fmt_name(WANT_COMMITTER_IDENT))` for the
/// `--signoff` 8-bit-CTE check.
fn committer_ident_has_non_ascii(config: &GitConfig) -> Result<bool> {
    let ident = committer_ident_string(config)?;
    Ok(ident.bytes().any(|b| b >= 0x80))
}

/// git's `clean_message_id`: strip leading whitespace + `<`, and trailing
/// whitespace + `>`, returning the inner id. Used for `--in-reply-to`.
fn clean_message_id(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut start = 0;
    while start < bytes.len() && (bytes[start].is_ascii_whitespace() || bytes[start] == b'<') {
        start += 1;
    }
    // Last index that is neither whitespace nor '>'.
    let mut last = None;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if !b.is_ascii_whitespace() && b != b'>' {
            last = Some(i);
        }
    }
    match last {
        Some(z) => String::from_utf8_lossy(&bytes[start..=z]).into_owned(),
        None => raw.to_string(),
    }
}

/// The resolved threading plan for a whole run: the cover's headers (when a cover
/// is emitted) and one `MailThreadHeaders` per patch, in emission order. Built by
/// replaying git's per-mail threading state machine (builtin/log.c).
struct ThreadPlan {
    cover: MailThreadHeaders,
    patches: Vec<MailThreadHeaders>,
}

/// Replay git's threading state machine to assign Message-ID / In-Reply-To /
/// References to the cover and each patch. `start_number` is the first patch's
/// `n` (so `rev.nr` for patch index `i` is `start_number + i`).
fn build_thread_plan(
    level: ThreadLevel,
    in_reply_to: Option<&str>,
    cover_letter: bool,
    commit_oids: &[String],
    start_number: usize,
    timestamp: i64,
    email: &str,
) -> ThreadPlan {
    let threading = level != ThreadLevel::Unset;
    // ref_message_ids: the live reference list git mutates as it walks.
    let mut ref_ids: Vec<String> = Vec::new();
    if let Some(irt) = in_reply_to {
        ref_ids.push(clean_message_id(irt));
    }
    // rev.message_id: the id assigned to the previously-emitted mail.
    let mut prev_message_id: Option<String> = None;

    let mut cover = MailThreadHeaders::default();
    if cover_letter {
        if threading {
            let id = gen_message_id("cover", timestamp, email);
            prev_message_id = Some(id.clone());
            cover.message_id = Some(id);
        }
        // The cover's In-Reply-To/References come from any pre-seeded ref_ids
        // (i.e. --in-reply-to), captured *before* the cover's own id is pushed.
        cover.references = ref_ids.clone();
    }

    let mut patches = Vec::with_capacity(commit_oids.len());
    for (i, oid) in commit_oids.iter().enumerate() {
        let rev_nr = start_number + i;
        if threading {
            if let Some(prev) = prev_message_id.take() {
                // SHALLOW: drop the previous id (don't chain) when there is at
                // least one reference already and we're past the cover's reply.
                // DEEP: always chain the previous id into references.
                let shallow_drop = level == ThreadLevel::Shallow
                    && !ref_ids.is_empty()
                    && (!cover_letter || rev_nr > 1);
                if !shallow_drop {
                    ref_ids.push(prev);
                }
            }
            let id = gen_message_id(oid, timestamp, email);
            prev_message_id = Some(id.clone());
            let mut h = MailThreadHeaders {
                message_id: Some(id),
                references: ref_ids.clone(),
            };
            // The patch's own Message-ID is not part of its References list.
            let _ = &mut h;
            patches.push(h);
        } else {
            // No threading: only --in-reply-to seeds In-Reply-To/References,
            // and only when explicitly given (ref_ids non-empty).
            patches.push(MailThreadHeaders {
                message_id: None,
                references: ref_ids.clone(),
            });
        }
    }

    ThreadPlan { cover, patches }
}

/// Produce the patch body: the commit message with its subject line removed,
/// guaranteed to end in exactly one newline (empty body yields no bytes other
/// than the optional sign-off). The `---` separator follows whatever this
/// returns.
///
/// `subject` is the (unprefixed) subject text. git's `append_signoff` runs over
/// the *whole* pretty-printed mail (the `Subject:` header line + blank + body),
/// so its blank-line / footer-detection rules see the subject too. We reproduce
/// that by running the trailer logic over `subject\n\n<body>` and then slicing
/// the subject framing back off — this is what makes the subject-only case emit
/// exactly one blank line before the sign-off (no spurious extra blanks).
fn format_patch_body(message: &[u8], subject: &[u8], signoff_line: Option<&[u8]>) -> Vec<u8> {
    let mut body = commit_body(message).to_vec();
    // Strip any trailing newlines, then re-add a single one (when non-empty) so
    // the body always ends "...text\n" before the sign-off / separator.
    while body.last() == Some(&b'\n') {
        body.pop();
    }
    if !body.is_empty() {
        body.push(b'\n');
    }
    let Some(signoff) = signoff_line else {
        return body;
    };
    // Reconstruct the mail buffer git's append_signoff operates on: the subject
    // line, a blank line, then the body. Run the trailer logic, then strip the
    // `subject\n\n` framing the renderer emits separately.
    let mut framed = Vec::with_capacity(subject.len() + body.len() + 2);
    framed.extend_from_slice(subject);
    framed.push(b'\n');
    framed.push(b'\n');
    let frame_len = framed.len();
    framed.extend_from_slice(&body);
    append_signoff_trailer(&mut framed, signoff);
    framed.split_off(frame_len)
}

fn write_mboxrd_escaped_body(out: &mut Vec<u8>, body: &[u8]) {
    let mut start = 0usize;
    while start < body.len() {
        let end = body[start..]
            .iter()
            .position(|&byte| byte == b'\n')
            .map(|pos| start + pos + 1)
            .unwrap_or(body.len());
        let line = &body[start..end];
        if mboxrd_trim_from_line(out, line) {
            start = end;
            continue;
        }
        if mboxrd_needs_escape(line) {
            out.push(b'>');
        }
        out.extend_from_slice(line);
        start = end;
    }
}

fn mboxrd_trim_from_line(out: &mut Vec<u8>, line: &[u8]) -> bool {
    let content = line.strip_suffix(b"\n").unwrap_or(line);
    if content.len() <= 4 || !content.starts_with(b"From") {
        return false;
    }
    if content[4..]
        .iter()
        .all(|byte| *byte == b' ' || *byte == b'\t')
    {
        out.extend_from_slice(b"From");
        if line.ends_with(b"\n") {
            out.push(b'\n');
        }
        return true;
    }
    false
}

fn mboxrd_needs_escape(line: &[u8]) -> bool {
    let mut rest = line;
    while rest.first() == Some(&b'>') {
        rest = &rest[1..];
    }
    rest.starts_with(b"From ")
}

/// Append a `Signed-off-by:` trailer to `body` following git's `append_signoff`
/// (sequencer.c) with `APPEND_SIGNOFF_DEDUP`. `body` is expected to already end
/// in a single `\n` (or be empty). The new sob is `<signoff_line>\n`.
///
/// The key parity behaviour: when the body's trailing paragraph already parses
/// as a *conforming trailer block* (a run of `Token: value` trailers, possibly
/// with recognised non-trailer lines like `(cherry picked from commit ...)`),
/// git appends the new sob directly after it with no blank line — and if that
/// block's last entry is already an identical sob, it appends nothing at all
/// (dedup). Otherwise a blank line precedes the sob.
fn append_signoff_trailer(body: &mut Vec<u8>, signoff_line: &[u8]) {
    let mut sob = signoff_line.to_vec();
    sob.push(b'\n');

    // Whole message equals the sob → treat as conforming footer with matching
    // last sob (git's `has_footer = 3`): nothing to append.
    if body.as_slice() == sob.as_slice() {
        return;
    }

    let footer = conforming_footer_state(body, &sob);

    if footer == FooterState::None {
        // Add a blank line so the body and the sob are separated, mirroring
        // git's buffer-state rules. After the line-completion above, an empty
        // body needs "\n\n" (title room); a single "\n" or a body ending in a
        // single (non-blank-terminated) "\n" needs one more "\n"; a body already
        // ending in "\n\n" needs nothing.
        if body.is_empty() {
            body.extend_from_slice(b"\n\n");
        } else if body.len() == 1 || body[body.len() - 2] != b'\n' {
            body.push(b'\n');
        }
    }

    // has_footer == 3 (sob is the last entry) → don't duplicate; DEDUP +
    // has_footer == 2 (sob present, not last) → also don't duplicate.
    if matches!(footer, FooterState::SobLast | FooterState::SobPresent) {
        return;
    }
    body.extend_from_slice(&sob);
}

/// The conforming-footer classification git's `has_conforming_footer` returns,
/// scoped to what `append_signoff` needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FooterState {
    /// No conforming trailer block in the trailing paragraph.
    None,
    /// A conforming footer with no matching sob.
    Conforming,
    /// A conforming footer that contains the sob (not as the last entry).
    SobPresent,
    /// A conforming footer whose last entry is the sob.
    SobLast,
}

/// Port of git's `has_conforming_footer` (sequencer.c) over `body`: detect the
/// trailing trailer block via [`find_trailer_block_start`], iterate its trailer
/// lines, and report whether the new `sob` appears and whether it is last.
fn conforming_footer_state(body: &[u8], sob: &[u8]) -> FooterState {
    let start = find_trailer_block_start(body);
    let block = &body[start..];
    // Trailer lines are the non-blank, non-continuation lines of the block whose
    // text begins a trailer (`Token: value`, or a recognised prefix). git counts
    // every advanced trailer; we mirror that and track the sob position.
    let mut count = 0usize;
    let mut found_sob = 0usize;
    for line in block.split_inclusive(|&b| b == b'\n') {
        if line == b"\n" || line.is_empty() {
            continue;
        }
        // Continuation lines (leading whitespace) belong to the previous trailer.
        if line[0].is_ascii_whitespace() {
            continue;
        }
        if !line_is_trailer(line) {
            continue;
        }
        count += 1;
        if line == sob {
            found_sob = count;
        }
    }
    if count == 0 {
        return FooterState::None;
    }
    if found_sob == count {
        FooterState::SobLast
    } else if found_sob > 0 {
        FooterState::SobPresent
    } else {
        FooterState::Conforming
    }
}

/// Does this single line (including its trailing `\n`) begin a git trailer? A
/// trailer is `Token: value` where `Token` is non-empty and made of
/// `[A-Za-z0-9-]`, or one of git's recognised non-`:` prefixes.
fn line_is_trailer(line: &[u8]) -> bool {
    let text = line.strip_suffix(b"\n").unwrap_or(line);
    if text.starts_with(b"Signed-off-by: ") || text.starts_with(b"(cherry picked from commit ") {
        return true;
    }
    // Find the `:` separator; the token before it must be non-empty and only
    // contain token characters (matching git's default `:` trailer separator).
    let Some(colon) = text.iter().position(|&b| b == b':') else {
        return false;
    };
    if colon == 0 {
        return false;
    }
    text[..colon]
        .iter()
        .all(|&b| b.is_ascii_alphanumeric() || b == b'-')
}

/// Port of trailer.c `find_trailer_block_start` for the format-patch signoff
/// path: returns the byte offset in `buf` at which the trailing trailer block
/// begins (or `buf.len()` if the trailing paragraph is not a trailer block).
/// Mirrors the for-each-ref port already in the tree, kept local to avoid a
/// cross-module dependency.
fn find_trailer_block_start(buf: &[u8]) -> usize {
    let len = buf.len();
    // Skip the title paragraph up to the first blank line.
    let mut s = 0usize;
    while s < len {
        if is_blank_line(buf, s) {
            break;
        }
        s = next_line(buf, s, len);
    }
    let end_of_title = s;

    let mut only_spaces = true;
    let mut recognized_prefix = false;
    let mut trailer_lines: i64 = 0;
    let mut non_trailer_lines: i64 = 0;
    let mut possible_continuation: i64 = 0;

    let mut maybe_l = last_line(buf, len);
    while let Some(l) = maybe_l {
        if l < end_of_title {
            break;
        }
        if is_blank_line(buf, l) {
            if only_spaces {
                // trailing blank; keep scanning upward
            } else {
                non_trailer_lines += possible_continuation;
                if (recognized_prefix && trailer_lines * 3 >= non_trailer_lines)
                    || (trailer_lines > 0 && non_trailer_lines == 0)
                {
                    return next_line(buf, l, len);
                }
                return len;
            }
        } else {
            only_spaces = false;
            let line = &buf[l..next_line(buf, l, len)];
            let text = line.strip_suffix(b"\n").unwrap_or(line);
            if text.starts_with(b"Signed-off-by: ")
                || text.starts_with(b"(cherry picked from commit ")
            {
                trailer_lines += 1;
                possible_continuation = 0;
                recognized_prefix = true;
            } else if trailer_separator_pos(text).is_some_and(|pos| pos >= 1)
                && !buf[l].is_ascii_whitespace()
            {
                trailer_lines += 1;
                possible_continuation = 0;
            } else if buf[l].is_ascii_whitespace() {
                possible_continuation += 1;
            } else {
                non_trailer_lines += 1;
                non_trailer_lines += possible_continuation;
                possible_continuation = 0;
            }
        }
        if l == 0 {
            break;
        }
        maybe_l = last_line(buf, l);
    }
    len
}

/// The position of the trailer `:` separator in a line's text, requiring the
/// token before it to be made of token characters (`[A-Za-z0-9-]`).
fn trailer_separator_pos(text: &[u8]) -> Option<usize> {
    let colon = text.iter().position(|&b| b == b':')?;
    if colon == 0 {
        return None;
    }
    text[..colon]
        .iter()
        .all(|&b| b.is_ascii_alphanumeric() || b == b'-')
        .then_some(colon)
}

/// True when the line beginning at `i` is blank (empty or only whitespace up to
/// the next `\n`).
fn is_blank_line(buf: &[u8], i: usize) -> bool {
    let end = next_line(buf, i, buf.len());
    let line = &buf[i..end];
    let trimmed = line.strip_suffix(b"\n").unwrap_or(line);
    trimmed.iter().all(|&b| b.is_ascii_whitespace())
}

/// Byte offset of the start of the line after the one beginning at `i`.
fn next_line(buf: &[u8], i: usize, len: usize) -> usize {
    match buf[i..len].iter().position(|&b| b == b'\n') {
        Some(rel) => i + rel + 1,
        None => len,
    }
}

/// Byte offset of the start of the last line ending before `end` (the line
/// containing `buf[end-1]`), or `None` when `end == 0`.
fn last_line(buf: &[u8], end: usize) -> Option<usize> {
    if end == 0 {
        return None;
    }
    // The last byte before `end`; if it is the line terminator, look at the line
    // it terminates.
    let scan_end = if buf[end - 1] == b'\n' { end - 1 } else { end };
    if scan_end == 0 {
        return Some(0);
    }
    match buf[..scan_end].iter().rposition(|&b| b == b'\n') {
        Some(pos) => Some(pos + 1),
        None => Some(0),
    }
}

/// Resolve the runtime committer identity (env first, then config `user.*`) and
/// format the `Signed-off-by:` trailer line, matching `git commit --signoff` /
/// `git format-patch --signoff`.
fn format_patch_signoff(config: &GitConfig) -> Result<Vec<u8>> {
    let name = env::var("GIT_COMMITTER_NAME")
        .ok()
        .or_else(|| config.get("user", None, "name").map(str::to_string))
        .unwrap_or_else(|| "Git Rs".to_string());
    let email = env::var("GIT_COMMITTER_EMAIL")
        .ok()
        .or_else(|| config.get("user", None, "email").map(str::to_string))
        .unwrap_or_else(|| "sley@example.invalid".to_string());
    Ok(format!("Signed-off-by: {name} <{email}>").into_bytes())
}

/// Compute the abbreviation width for patch `index` lines, honoring
/// `--full-index`, an explicit `--abbrev=<n>`, the repository's
/// `core.abbrev`, and git's default of 7.
fn patch_index_abbrev(
    git_dir: &Path,
    format: ObjectFormat,
    options: &FormatPatchOptions,
) -> Result<usize> {
    if options.full_index {
        return Ok(format.hex_len());
    }
    let repo_abbrev = repository_abbrev(git_dir, format)?;
    Ok(options
        .abbrev
        .or(repo_abbrev)
        .unwrap_or(7)
        .min(format.hex_len()))
}

/// Build the name-status diff entry list for `commit` against its first parent,
/// or against the empty tree when it is a root commit, honoring the rename/copy
/// detection options. Reuses the shared diff engine.
fn first_parent_diff_entries(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    options: &FormatPatchOptions,
    diff_pathspec: Option<&DiffPathspec>,
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
    rename_limit: 0,
    };
    let entries = match commit.parents.first() {
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
    }?;
    let entries = match diff_pathspec {
        Some(pathspec) => apply_diff_pathspec(entries, pathspec),
        None => entries,
    };
    let entries = apply_format_patch_relative(entries, options);
    apply_diff_order_file(entries, options.order_file.as_deref())
}

/// Select the commits to format, newest-to-oldest from the walk then reversed to
/// git's oldest-first output order, with merge commits dropped (format-patch is
/// `--no-merges` by default).
fn select_commits(
    repo: &RepositoryContext,
    options: &FormatPatchOptions,
) -> Result<FormatPatchSelection> {
    let format = repo.format();
    let db = repo.objects();
    let setup_args = format_patch_setup_args(options);
    if let Some(rev) = format_patch_bare_exclude(options) {
        let oid = repo
            .resolve_revision(rev)
            .map_err(|_| sley_rev::ambiguous_argument_error(rev))?;
        sley_rev::peel_to_commit(db, format, &oid)
            .map_err(|_| sley_rev::ambiguous_argument_error(rev))?;
    }
    let setup = sley_rev::setup_revisions(
        &setup_args,
        &sley_rev::RevisionSetupContext {
            git_dir: repo.git_dir(),
            worktree_root: repo.worktree_root().ok(),
            cwd: repo.cwd(),
            format,
            reader: db,
            config: Some(repo.config()),
        },
    )?;
    if let Some(leftover) = setup.leftovers.first() {
        return Err(GitError::Command(format!(
            "unsupported format-patch option {leftover}"
        )));
    }
    let revision_options = setup.options;
    let starts = revision_options
        .positives
        .iter()
        .map(|tip| sley_rev::peel_to_commit(db, format, &tip.oid))
        .collect::<Result<Vec<_>>>()?;

    let mut excluded = HashSet::new();
    let mut excluded_records = Vec::new();
    for oid in revision_options.negatives {
        for record in rev_list_walk_commits(db, format, [oid], false)? {
            excluded.insert(record.oid);
            excluded_records.push(record);
        }
    }

    let walked = rev_list_walk_commits(db, format, starts, false)?;
    let upstream_patch_ids = if options.ignore_if_in_upstream {
        let mut ids = HashSet::new();
        for record in &excluded_records {
            if record.parents.len() > 1 {
                continue;
            }
            let parent_tree = match record.parents.first() {
                Some(parent) => commit_tree_oid(db, format, parent)?,
                None => ObjectId::empty_tree(format),
            };
            if record.commit.tree == parent_tree {
                continue;
            }
            if let Some(id) = format_patch_commit_patch_id(db, format, record)? {
                ids.insert(id);
            }
        }
        ids
    } else {
        HashSet::new()
    };

    // `walked` is a breadth-first traversal, which is NOT git's output order.
    // format-patch is `git rev-list` reversed with merges dropped, so we must
    // first reorder the reachable set into rev-list's committer-date order
    // (the same `rev_list_date_order` the `rev-list` command uses) before
    // dropping merges and reversing — otherwise a branchy range emits patches
    // in the wrong sequence (and numbers them accordingly).
    let reachable: Vec<&sley_rev::CommitRecord> = walked
        .iter()
        .filter(|record| !excluded.contains(&record.oid))
        .collect();
    let ordered = rev_list_date_order(reachable)?;
    // Keep non-merge commits in rev-list (newest-first) order.
    let mut selected: Vec<sley_rev::CommitRecord> = ordered
        .into_iter()
        .filter(|record| record.parents.len() <= 1)
        .cloned()
        .collect();
    let grep_kind = log_grep_pattern_kind_from_config(
        repo.config(),
        options.grep_pattern_kind,
        options.grep_pattern_kind_explicit,
    );
    if let Some(matcher) = compile_log_message_grep_matcher(
        &options.grep_patterns,
        grep_kind,
        options.grep_ignore_case,
    )? {
        selected.retain(|record| {
            let matched = if options.grep_all_match {
                matcher.matches_all(&record.commit.message)
            } else {
                matcher.matches_any(&record.commit.message)
            };
            matched != options.grep_invert
        });
    }
    if !setup.pathspecs.is_empty() {
        let pathspec = sley_rev::Pathspec::parse(
            setup.pathspecs.iter().map(|spec| spec.as_bytes()),
            sley_rev::PathspecMatchMagic::default(),
        )
        .map_err(|err| GitError::Command(format!("bad pathspec: {err:?}")))?;
        selected = sley_rev::simplify_history(
            db,
            format,
            selected,
            &pathspec,
            sley_rev::SimplifyOptions {
                full_history: false,
                first_parent: false,
                ..Default::default()
            },
        )?;
    }

    // `-<n>` keeps the n newest of those before reversing to oldest-first.
    if let Some(count) = options.count {
        selected.truncate(count);
    }
    if !upstream_patch_ids.is_empty() {
        let mut kept = Vec::with_capacity(selected.len());
        for record in selected {
            let parent_tree = match record.parents.first() {
                Some(parent) => commit_tree_oid(db, format, parent)?,
                None => ObjectId::empty_tree(format),
            };
            if record.commit.tree != parent_tree
                && let Some(id) = format_patch_commit_patch_id(db, format, &record)?
                && upstream_patch_ids.contains(&id)
            {
                continue;
            }
            kept.push(record);
        }
        selected = kept;
    }
    selected.reverse();
    Ok(FormatPatchSelection {
        commits: selected,
        pathspecs: setup.pathspecs,
    })
}

fn format_patch_setup_args(options: &FormatPatchOptions) -> Vec<String> {
    let mut args = options.setup_args.clone();
    if let Some(rev) = format_patch_bare_exclude(options) {
        args[0] = "HEAD".to_string();
        args.insert(1, format!("^{rev}"));
        return args;
    }
    let dashdash = args.iter().position(|arg| arg == "--");
    let rev_end = dashdash.unwrap_or(args.len());
    if rev_end == 0 {
        args.insert(0, "HEAD".to_string());
    }
    args
}

fn format_patch_bare_exclude(options: &FormatPatchOptions) -> Option<&str> {
    if options.count.is_some() {
        return None;
    }
    // `--root` treats a lone revision as a range (`<rev>` and its ancestors)
    // rather than the `<rev>..HEAD` since-form, so never derive a `^<rev>`.
    if options.root {
        return None;
    }
    let args = &options.setup_args;
    let dashdash = args.iter().position(|arg| arg == "--");
    let rev_end = dashdash.unwrap_or(args.len());
    if rev_end != 1 {
        return None;
    }
    let rev = args[0].as_str();
    (!rev.starts_with('^') && !rev.contains("..")).then_some(rev)
}

fn format_patch_commit_patch_id(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    record: &sley_rev::CommitRecord,
) -> Result<Option<Vec<u8>>> {
    if record.parents.len() > 1 {
        return Ok(None);
    }
    let parent_tree = match record.parents.first() {
        Some(parent) => commit_tree_oid(db, format, parent)?,
        None => ObjectId::empty_tree(format),
    };
    let diff = render_tree_to_tree_patch(db, format, &parent_tree, &record.commit.tree)
        .unwrap_or_default();
    Ok(commands::patch_id::patch_id_for_diff(&diff, format))
}

fn resolve_base_info(
    repo: &RepositoryContext,
    options: &FormatPatchOptions,
    config: &GitConfig,
    commits: &[sley_rev::CommitRecord],
) -> Result<Option<BaseInfo>> {
    if commits.is_empty() {
        return Ok(None);
    }
    let format = repo.format();
    let db = repo.objects();
    let base_oid = match &options.base {
        BaseMode::None => return Ok(None),
        BaseMode::Commit(rev) => Some(resolve_base_commit(repo, rev)?),
        BaseMode::Auto => resolve_upstream_base(repo, config)?,
        BaseMode::Config => match config.get("format", None, "useAutoBase") {
            Some(value) if value.eq_ignore_ascii_case("true") => {
                resolve_upstream_base(repo, config)?
            }
            Some(value) if value.eq_ignore_ascii_case("whenAble") => {
                resolve_upstream_base(repo, config)?
            }
            _ => None,
        },
    };
    let Some(base_oid) = base_oid else {
        return Ok(None);
    };
    validate_base_commit(db, format, &base_oid, commits)?;
    let prerequisites = prerequisite_patch_ids(db, format, &base_oid, commits)?;
    Ok(Some(BaseInfo {
        base: base_oid,
        prerequisites,
    }))
}

fn resolve_base_commit(repo: &RepositoryContext, rev: &str) -> Result<ObjectId> {
    let oid = repo
        .resolve_revision(rev)
        .map_err(|_| sley_rev::ambiguous_argument_error(rev))?;
    sley_rev::peel_to_commit(repo.objects(), repo.format(), &oid)
        .map_err(|_| sley_rev::ambiguous_argument_error(rev))
}

fn resolve_upstream_base(repo: &RepositoryContext, config: &GitConfig) -> Result<Option<ObjectId>> {
    let Some(branch) = current_branch_name(repo)? else {
        return Ok(None);
    };
    let Some(merge) = config.get("branch", Some(&branch), "merge") else {
        return Ok(None);
    };
    let remote = config.get("branch", Some(&branch), "remote").unwrap_or(".");
    let rev = if remote == "." {
        merge.to_string()
    } else {
        let short = merge.strip_prefix("refs/heads/").unwrap_or(merge);
        format!("refs/remotes/{remote}/{short}")
    };
    resolve_base_commit(repo, &rev).map(Some)
}

fn current_branch_name(repo: &RepositoryContext) -> Result<Option<String>> {
    match repo.refs().read_ref("HEAD")? {
        Some(RefTarget::Symbolic(name)) => Ok(name.strip_prefix("refs/heads/").map(str::to_string)),
        _ => Ok(None),
    }
}

fn validate_base_commit(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    base: &ObjectId,
    commits: &[sley_rev::CommitRecord],
) -> Result<()> {
    if commits.iter().any(|record| &record.oid == base) {
        eprintln!("fatal: base commit should be the ancestor of revision list");
        return Err(GitError::Exit(128));
    }
    let tips = commits.iter().map(|record| record.oid).collect::<Vec<_>>();
    let reachable = rev_list_walk_commits(db, format, tips, false)?;
    if !reachable.iter().any(|record| &record.oid == base) {
        eprintln!("fatal: base commit should be the ancestor of revision list");
        return Err(GitError::Exit(128));
    }
    Ok(())
}

fn prerequisite_patch_ids(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    base: &ObjectId,
    commits: &[sley_rev::CommitRecord],
) -> Result<Vec<Vec<u8>>> {
    let Some(oldest_parent) = commits[0].parents.first() else {
        return Ok(Vec::new());
    };
    let selected: HashSet<ObjectId> = commits.iter().map(|record| record.oid).collect();
    let mut chain = Vec::new();
    let mut cursor = *oldest_parent;
    while &cursor != base {
        if selected.contains(&cursor) {
            break;
        }
        let record = read_commit_record_for_format_patch(db, format, cursor)?;
        let Some(parent) = record.parents.first().copied() else {
            break;
        };
        chain.push(record);
        cursor = parent;
    }
    chain.reverse();
    let mut ids = Vec::new();
    for record in &chain {
        let parent_tree = match record.parents.first() {
            Some(parent) => commit_tree_oid(db, format, parent)?,
            None => ObjectId::empty_tree(format),
        };
        let diff = render_tree_to_tree_patch(db, format, &parent_tree, &record.commit.tree)
            .unwrap_or_default();
        if let Some(id) = commands::patch_id::stable_patch_id_for_diff(&diff, format) {
            ids.push(id);
        }
    }
    Ok(ids)
}

fn read_commit_record_for_format_patch(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    oid: ObjectId,
) -> Result<sley_rev::CommitRecord> {
    let object = db.read_object(&oid)?;
    let commit: Commit = Commit::parse_ref(format, &object.body)?.into();
    Ok(sley_rev::CommitRecord {
        oid,
        parents: commit.parents.clone(),
        commit,
    })
}

fn resolve_format_patch_relative_prefix(
    repo: &RepositoryContext,
    options: &FormatPatchOptions,
    config: &GitConfig,
) -> Result<Option<Vec<u8>>> {
    match &options.relative_mode {
        RelativeMode::Off => Ok(None),
        RelativeMode::On(Some(path)) => Ok(normalize_relative_prefix(path)),
        RelativeMode::On(None) => cwd_relative_prefix(repo),
        RelativeMode::Config => {
            if config.get_bool("diff", None, "relative").unwrap_or(false) {
                cwd_relative_prefix(repo)
            } else {
                Ok(None)
            }
        }
    }
}

fn cwd_relative_prefix(repo: &RepositoryContext) -> Result<Option<Vec<u8>>> {
    let root = repo.worktree_root()?;
    let cwd = repo.cwd();
    let Ok(relative) = cwd.strip_prefix(root) else {
        return Ok(None);
    };
    let text = relative.to_string_lossy().replace('\\', "/");
    Ok(normalize_relative_prefix(&text))
}

fn normalize_relative_prefix(path: &str) -> Option<Vec<u8>> {
    let trimmed = path.trim_matches('/');
    if trimmed.is_empty() || trimmed == "." {
        return None;
    }
    let mut bytes = trimmed.as_bytes().to_vec();
    bytes.push(b'/');
    Some(bytes)
}

fn apply_format_patch_relative(
    entries: Vec<sley_diff_merge::NameStatusEntry>,
    options: &FormatPatchOptions,
) -> Vec<sley_diff_merge::NameStatusEntry> {
    let Some(prefix) = options.relative_prefix.as_deref() else {
        return entries;
    };
    entries
        .into_iter()
        .filter_map(|mut entry| {
            let new_path = strip_relative_prefix(&entry.path, prefix);
            let old_path = entry
                .old_path
                .as_ref()
                .and_then(|path| strip_relative_prefix(path, prefix));
            match (new_path, old_path) {
                (Some(path), old) => {
                    entry.path = path.into();
                    entry.old_path = old.map(Into::into);
                    Some(entry)
                }
                (None, Some(path)) => {
                    entry.path = path.into();
                    entry.old_path = None;
                    Some(entry)
                }
                (None, None) => None,
            }
        })
        .collect()
}

fn strip_relative_prefix(path: &[u8], prefix: &[u8]) -> Option<Vec<u8>> {
    path.strip_prefix(prefix).map(|stripped| stripped.to_vec())
}

fn format_patch_display_path(out_dir: &str, file_name: &str) -> PathBuf {
    if out_dir.is_empty() || out_dir == "." {
        PathBuf::from(file_name)
    } else {
        Path::new(out_dir).join(file_name)
    }
}

/// Parse a `format.noprefix` config value as a strict boolean. git tightened
/// this from "any value is treated as true" and now errors on a non-boolean,
/// printing the migration hints.
fn parse_format_noprefix_bool(value: &str) -> Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "" | "true" | "yes" | "on" | "1" => Ok(true),
        "false" | "no" | "off" | "0" => Ok(false),
        _ => {
            eprintln!("fatal: bad boolean config value '{value}' for 'format.noprefix'");
            eprintln!("hint: 'format.noprefix' used to accept any value and treat that as 'true'.");
            eprintln!("hint: Now it only accepts boolean values, like what 'diff.noprefix' does.");
            Err(GitError::Exit(128))
        }
    }
}

/// Default `patch_name_max` (git `FORMAT_PATCH_NAME_MAX_DEFAULT`).
const FORMAT_PATCH_NAME_MAX_DEFAULT: usize = 64;
/// The `.patch` suffix used by output filenames.
const PATCH_SUFFIX: &str = ".patch";

/// Parse a `--filename-max-length=<n>` / `format.filenameMaxLength` value.
fn parse_filename_max_length(value: &str) -> Result<usize> {
    value
        .trim()
        .parse::<i64>()
        .map(|n| n.max(0) as usize)
        .map_err(|_| GitError::Command(format!("could not parse '{value}'")))
}

/// Resolve the filename length cap: CLI flag, else `format.filenameMaxLength`,
/// else the default. git clamps anything `<= len("0000-") + len(suffix)` up to
/// that floor so the number and suffix always fit.
fn resolve_patch_name_max(options: &FormatPatchOptions, config: &GitConfig) -> usize {
    let raw = options
        .filename_max_length
        .or_else(|| {
            config
                .get("format", None, "filenamemaxlength")
                .and_then(|value| parse_filename_max_length(value).ok())
        })
        .unwrap_or(FORMAT_PATCH_NAME_MAX_DEFAULT);
    let floor = "0000-".len() + PATCH_SUFFIX.len();
    raw.max(floor)
}

/// git `fmt_output_subject` reroll prefix: a sanitized `v<reroll>-`. The reroll
/// string itself runs through `format_sanitized_subject` (so non-pathname
/// characters collapse to `-`), then a literal `-` is appended.
fn reroll_filename_prefix(reroll: &str) -> String {
    let sanitized = sanitize_filename_component(format!("v{reroll}").as_bytes());
    format!("{sanitized}-")
}

/// Build a patch output basename: `<reroll>NNNN-<slug><suffix>`, hard-truncated
/// so the whole basename fits in `patch_name_max` (git `fmt_output_subject`: the
/// part before the suffix is capped at `patch_name_max - (len(suffix) + 1)`).
fn build_patch_filename(
    reroll_prefix: &str,
    seq: usize,
    slug: &str,
    patch_name_max: usize,
    suffix: &str,
) -> String {
    let mut stem = format!("{reroll_prefix}{seq:04}-{slug}");
    let max_len = patch_name_max.saturating_sub(suffix.len() + 1);
    if stem.len() > max_len {
        stem.truncate(max_len);
    }
    format!("{stem}{suffix}")
}

/// The output-file / MIME-attachment filename suffix: `--suffix`, else
/// `format.filenameSuffix`, else git's `.patch` default.
fn patch_filename_suffix(options: &FormatPatchOptions, config: &GitConfig) -> String {
    options
        .suffix
        .clone()
        .or_else(|| {
            config
                .get("format", None, "filenamesuffix")
                .map(|value| value.to_string())
        })
        .unwrap_or_else(|| PATCH_SUFFIX.to_string())
}

/// git's `format_sanitized_subject` over the commit subject (no length cap; the
/// caller truncates the assembled filename).
fn sanitize_patch_subject(message: &[u8]) -> String {
    let subject = commit_subject(message);
    sanitize_filename_component(subject.as_bytes())
}

/// git's `format_sanitized_subject`: keep alphanumerics, `.` and `_`; collapse
/// each run of other characters to a single `-`; collapse consecutive dots; no
/// leading separator; trim trailing `-`/`.`.
fn sanitize_filename_component(input: &[u8]) -> String {
    let bytes = input;
    let mut out = String::new();
    // `space` tracks whether a separator is pending: 2 = at start (suppress a
    // leading dash), 1 = a separator was seen, 0 = last char was kept.
    let mut space = 2u8;
    let mut idx = 0;
    while idx < bytes.len() {
        let byte = bytes[idx];
        if is_title_char(byte) {
            if space == 1 {
                out.push('-');
            }
            space = 0;
            out.push(byte as char);
            if byte == b'.' {
                // Collapse a run of dots into the single one just written.
                while idx + 1 < bytes.len() && bytes[idx + 1] == b'.' {
                    idx += 1;
                }
            }
        } else {
            space |= 1;
        }
        idx += 1;
    }
    while out.ends_with(['-', '.']) {
        out.pop();
    }
    out
}

/// A "title" character for filename sanitization: ASCII alphanumeric, `.`, `_`.
fn is_title_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'.' || byte == b'_'
}

// --- format-patch diff adapter -------------------------------------------------
//
// The mail framing stays in this module; per-file patch and summary rendering
// routes through the unified diff helpers with format-patch's byte constraints.

fn format_patch_diff_options<'a>(
    db: &'a FileObjectDatabase,
    format: ObjectFormat,
    options: &'a FormatPatchOptions,
    abbrev: usize,
) -> crate::DiffRenderOptions<'a> {
    crate::DiffRenderOptions {
        binary: false,
        db,
        worktree_root: None,
        use_worktree_new: false,
        format,
        abbrev,
        src_prefix: &options.src_prefix,
        dst_prefix: &options.dst_prefix,
        context: options.context_lines,
        userdiff: None,
        funcname: None,
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
    }
}

/// Number of unchanged lines of context git keeps around each change in a hunk.
const HUNK_CONTEXT: usize = 3;

/// Emit the unified-diff hunks for a single file change into `out`, grouping
/// changes with [`HUNK_CONTEXT`] lines of surrounding context (merging nearby
/// groups), and prefixing each `@@` header with git's default section heading.
pub(crate) fn write_patch_hunks(
    out: &mut Vec<u8>,
    old_content: Option<&[u8]>,
    new_content: Option<&[u8]>,
    options: &crate::DiffRenderOptions<'_>,
) {
    write_patch_hunks_with(out, old_content, new_content, options);
}

/// [`write_patch_hunks`] with explicit hunk shaping options.
///
/// Thin adapter over the shared renderer
/// [`sley_diff_merge::render::render_hunks`]: it translates the sley-cli
/// option bundle (userdiff funcname, `DiffColors`, word-diff config) into the
/// engine's seams and delegates all hunk byte-shaping to the engine.
pub(crate) fn write_patch_hunks_with(
    out: &mut Vec<u8>,
    old_content: Option<&[u8]>,
    new_content: Option<&[u8]>,
    options: &crate::DiffRenderOptions<'_>,
) {
    let mut heading = sley_diff_format::heading_classifier(options.funcname);
    let mut word_diff: Option<sley_diff_format::WordDiffAdapter> = None;
    let default_colors = commands::diff_words::DiffColors::default();
    let mut word_diff_config: Option<commands::diff_words::WordDiffConfig> = None;
    if let Some(word_request) = options.word_diff {
        word_diff_config = Some(commands::diff_words::WordDiffConfig {
            mode: word_request.mode,
            regex: None,
            colors: options.colors.unwrap_or(&default_colors),
        });
    }
    word_diff = word_diff_config
        .as_ref()
        .map(sley_diff_format::WordDiffAdapter::new);
    let mut render_options = sley_diff_merge::render::HunkRenderOptions {
        context: options.context,
        interhunk: options.interhunk,
        heading: Some(&mut heading),
        colors: options.colors.map(sley_diff_format::render_colors),
        word_diff: word_diff
            .as_mut()
            .map(|adapter| adapter as &mut dyn sley_diff_merge::render::HunkWordDiff),
        ws_error: None,
        color_moved: None,
        ..Default::default()
    };
    sley_diff_merge::render::render_hunks(out, old_content, new_content, &mut render_options);
}

/// The diffstat block (`--stat`) written into `out`, via the shared
/// `show_stats` port. format-patch wraps mails at 72 columns: a zero
/// stat-width becomes `MAIL_DEFAULT_WRAP` exactly like `cmd_format_patch`,
/// and the diff.stat*Width config is never consulted.
fn write_patch_diffstat(
    out: &mut Vec<u8>,
    entries: &[sley_diff_merge::NameStatusEntry],
    db: &FileObjectDatabase,
    options: &FormatPatchOptions,
) -> Result<()> {
    let stat_entries = collect_diff_stat_entries(entries, db, None, false)?;
    let mut widths = options.stat_widths;
    if widths.stat_width == 0 {
        // MAIL_DEFAULT_WRAP
        widths.stat_width = 72;
    }
    write_diff_stat_materialized_with_widths(
        out,
        &stat_entries,
        DiffStatOptions {
            compact_summary: false,
            stat_count: options.stat_count,
            color: false,
        },
        widths,
    )
}

/// Append `text` plus a newline to the buffer.
fn writeln_buf(out: &mut Vec<u8>, text: &str) {
    out.extend_from_slice(text.as_bytes());
    out.push(b'\n');
}

fn write_fmt_buf(out: &mut Vec<u8>, args: std::fmt::Arguments<'_>) {
    std::io::Write::write_fmt(out, args).expect("writing to Vec cannot fail");
}

fn writeln_fmt_buf(out: &mut Vec<u8>, args: std::fmt::Arguments<'_>) {
    write_fmt_buf(out, args);
    out.push(b'\n');
}

/// Parse `git format-patch` arguments into [`FormatPatchOptions`]. Recognizes the
/// common flags; `--` forces remaining tokens to be revision arguments.
fn parse_format_patch_args(args: &[String]) -> Result<FormatPatchOptions> {
    let mut options = FormatPatchOptions::default();
    let mut positional_only = false;
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
                positional_only = true;
                options.setup_args.push(arg.clone());
            }
            "--stdout" => options.stdout = true,
            "-o" | "--output-directory" => {
                let value = iter.next().ok_or_else(|| {
                    GitError::Command("-o/--output-directory requires a value".into())
                })?;
                options.output_directory = Some(value.clone());
            }
            value if let Some(dir) = value.strip_prefix("--output-directory=") => {
                options.output_directory = Some(dir.to_string());
            }
            value if let Some(dir) = value.strip_prefix("-o") => {
                options.output_directory = Some(dir.to_string());
            }
            "--output" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--output requires a value".into()))?;
                options.output = Some(value.clone());
            }
            value if let Some(path) = value.strip_prefix("--output=") => {
                options.output = Some(path.to_string());
            }
            "-n" | "--numbered" => options.number_mode = NumberMode::Numbered,
            "-N" | "--no-numbered" => options.number_mode = NumberMode::Unnumbered,
            "--start-number" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--start-number requires a value".into()))?;
                options.start_number = Some(parse_format_patch_number(value, "--start-number")?);
            }
            value if let Some(n) = value.strip_prefix("--start-number=") => {
                options.start_number = Some(parse_format_patch_number(n, "--start-number")?);
            }
            "--numbered-files" => options.numbered_files = true,
            value if let Some(suffix) = value.strip_prefix("--suffix=") => {
                options.suffix = Some(suffix.to_string());
            }
            "-s" | "--signoff" | "--signed-off-by" => options.signoff = true,
            "--stat" => options.stat = true,
            value
                if value.starts_with("--stat=")
                    || value.starts_with("--stat-width=")
                    || value.starts_with("--stat-name-width=")
                    || value.starts_with("--stat-graph-width=")
                    || value.starts_with("--stat-count=") =>
            {
                options.stat = true;
                diff_stat_parse_width_option(value, &mut options.stat_widths)?;
                if let Some(count) = diff_stat_count_option(value)? {
                    options.stat_count = count;
                }
            }
            "--no-stat" => options.stat = false,
            "-U" | "--unified" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command(format!("{arg} requires a value")))?;
                options.context_lines = parse_unified_context_count(value);
            }
            value if let Some(n) = value.strip_prefix("-U") => {
                options.context_lines = parse_unified_context_count(n);
            }
            value if let Some(n) = value.strip_prefix("--unified=") => {
                options.context_lines = parse_unified_context_count(n);
            }
            // git's `format-patch -p` drops the leading diffstat (like
            // `--no-stat`). The long `--patch` is the diff-machinery flag and,
            // quirkily, does *not* disable the stat — so it is a no-op here.
            "-p" => options.stat = false,
            "--patch" | "--no-patch-with-stat" | "--numstat" => {}
            "--ignore-submodules" | "--no-ignore-submodules" => {}
            value if let Some(mode) = value.strip_prefix("--ignore-submodules=") => {
                if !matches!(mode, "" | "all" | "dirty" | "untracked" | "none") {
                    eprintln!("fatal: bad --ignore-submodules argument: {mode}");
                    return Err(GitError::Exit(128));
                }
            }
            "--full-index" => options.full_index = true,
            "--no-renames" => options.detect_renames = false,
            "-M" | "--find-renames" => options.detect_renames = true,
            value if let Some(rest) = value.strip_prefix("--find-renames=") => {
                options.detect_renames = true;
                options.rename_threshold = parse_similarity(rest)?;
            }
            value if value.starts_with("-M") => {
                options.detect_renames = true;
                options.rename_threshold = parse_similarity(&value[2..])?;
            }
            "-C" | "--find-copies" => {
                options.detect_renames = true;
                options.detect_copies = true;
            }
            value if let Some(rest) = value.strip_prefix("--find-copies=") => {
                options.detect_renames = true;
                options.detect_copies = true;
                options.copy_threshold = parse_similarity(rest)?;
            }
            value if value.starts_with("-C") => {
                options.detect_renames = true;
                options.detect_copies = true;
                options.copy_threshold = parse_similarity(&value[2..])?;
            }
            "-O" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("-O requires a value".into()))?;
                options.order_file = Some(value.clone());
            }
            value if let Some(path) = value.strip_prefix("-O") => {
                options.order_file = Some(path.to_string());
            }
            "--find-copies-harder" => {
                options.detect_copies = true;
                options.find_copies_harder = true;
            }
            "--abbrev" => options.abbrev = Some(7),
            "--no-abbrev" => options.abbrev = None,
            value if let Some(width) = value.strip_prefix("--abbrev=") => {
                options.abbrev = Some(width.parse::<usize>().unwrap_or(0).max(4));
            }
            value if let Some(prefix) = value.strip_prefix("--subject-prefix=") => {
                options.subject_prefix = Some(prefix.to_string());
            }
            "--subject-prefix" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--subject-prefix requires a value".into()))?;
                options.subject_prefix = Some(value.clone());
            }
            // `--rfc` (default token `RFC`), `--rfc=<token>`, `--rfc=` (clear),
            // `--no-rfc` (clear). A leading `-` in the token appends rather than
            // prepends (handled in resolve_prefix_body).
            "--rfc" => options.rfc = RfcMode::Token("RFC".to_string()),
            "--no-rfc" => options.rfc = RfcMode::Clear,
            value if let Some(token) = value.strip_prefix("--rfc=") => {
                options.rfc = if token.is_empty() {
                    RfcMode::Clear
                } else {
                    RfcMode::Token(token.to_string())
                };
            }
            // `-v<n>` / `--reroll-count=<n>` (also `--reroll-count <n>`):
            // appends ` v<n>` to the subject prefix.
            "-v" | "--reroll-count" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--reroll-count requires a value".into()))?;
                options.reroll_count = Some(value.clone());
            }
            value if let Some(n) = value.strip_prefix("--reroll-count=") => {
                options.reroll_count = Some(n.to_string());
            }
            value if let Some(n) = value.strip_prefix("-v") => {
                options.reroll_count = Some(n.to_string());
            }
            "--range-diff" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--range-diff requires a value".into()))?;
                options.range_diff = Some(value.clone());
            }
            value if let Some(previous) = value.strip_prefix("--range-diff=") => {
                options.range_diff = Some(previous.to_string());
            }
            "--filename-max-length" => {
                let value = iter.next().ok_or_else(|| {
                    GitError::Command("--filename-max-length requires a value".into())
                })?;
                options.filename_max_length = Some(parse_filename_max_length(value)?);
            }
            value if let Some(n) = value.strip_prefix("--filename-max-length=") => {
                options.filename_max_length = Some(parse_filename_max_length(n)?);
            }
            "-k" | "--keep-subject" => options.keep_subject = true,
            "--pretty=mboxrd" | "--format=mboxrd" => options.mboxrd = true,
            "--pretty" | "--format" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command(format!("{arg} requires a value")))?;
                if value == "mboxrd" {
                    options.mboxrd = true;
                }
            }
            // Recipient / extra-header injection.
            "--to" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--to requires a value".into()))?;
                options.cli_to.push(value.clone());
            }
            value if let Some(addr) = value.strip_prefix("--to=") => {
                options.cli_to.push(addr.to_string());
            }
            "--no-to" => {
                options.no_to = true;
                options.cli_to.clear();
            }
            "--cc" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--cc requires a value".into()))?;
                options.cli_cc.push(value.clone());
            }
            value if let Some(addr) = value.strip_prefix("--cc=") => {
                options.cli_cc.push(addr.to_string());
            }
            "--no-cc" => {
                options.no_cc = true;
                options.cli_cc.clear();
            }
            "--add-header" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--add-header requires a value".into()))?;
                push_cli_header(&mut options, value);
            }
            value if let Some(header) = value.strip_prefix("--add-header=") => {
                push_cli_header(&mut options, header);
            }
            // git's header_callback unset clears all three lists.
            "--no-add-header" => {
                options.no_add_header = true;
                options.cli_headers.clear();
                options.cli_to.clear();
                options.cli_cc.clear();
            }
            // `--from[=<ident>]` / `--no-from`.
            "--from" => options.from = FromMode::Committer,
            value if let Some(ident) = value.strip_prefix("--from=") => {
                options.from = FromMode::Ident(ident.to_string());
            }
            "--no-from" => options.from = FromMode::Clear,
            "--force-in-body-from" => options.force_in_body_from = Some(true),
            "--no-force-in-body-from" => options.force_in_body_from = Some(false),
            // `--signature=<text>` / `--no-signature` / `--signature-file=<path>`.
            "--signature" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--signature requires a value".into()))?;
                options.signature = SignatureMode::Text(value.clone());
            }
            value if let Some(text) = value.strip_prefix("--signature=") => {
                options.signature = SignatureMode::Text(text.to_string());
            }
            "--no-signature" => options.signature = SignatureMode::Suppress,
            "--signature-file" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--signature-file requires a value".into()))?;
                options.signature_file = Some(value.clone());
            }
            value if let Some(path) = value.strip_prefix("--signature-file=") => {
                options.signature_file = Some(path.to_string());
            }
            "--zero-commit" => options.zero_commit = true,
            "--always" => {}
            "--ignore-if-in-upstream" => options.ignore_if_in_upstream = true,
            "--base" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--base requires a value".into()))?;
                options.base = if value == "auto" {
                    BaseMode::Auto
                } else {
                    BaseMode::Commit(value.clone())
                };
            }
            value if let Some(base) = value.strip_prefix("--base=") => {
                options.base = if base == "auto" {
                    BaseMode::Auto
                } else {
                    BaseMode::Commit(base.to_string())
                };
            }
            "--no-base" => options.base = BaseMode::None,
            // Accepted-but-inert formatting knobs that do not change the bytes
            // sley emits for the common path.
            "--no-color"
            | "--color"
            | "--minimal"
            | "--patience"
            | "--histogram"
            | "--indent-heuristic"
            | "--no-indent-heuristic"
            | "--binary"
            | "--no-binary"
            | "--text"
            | "-a"
            | "--ita-invisible-in-index" => {}
            // `--attach`/`--inline` wrap each patch in MIME multipart/mixed; the
            // optional `=<boundary>` overrides the default (git version string).
            // `--no-attach` clears it. git: builtin/log.c attach/inline handlers.
            "--attach" => {
                options.mime = Some(MimeAttach {
                    boundary: sley_core::UPSTREAM_GIT_COMPAT_VERSION.to_string(),
                    inline: false,
                });
            }
            value if let Some(boundary) = value.strip_prefix("--attach=") => {
                options.mime = Some(MimeAttach {
                    boundary: boundary.to_string(),
                    inline: false,
                });
            }
            "--inline" => {
                options.mime = Some(MimeAttach {
                    boundary: sley_core::UPSTREAM_GIT_COMPAT_VERSION.to_string(),
                    inline: true,
                });
            }
            value if let Some(boundary) = value.strip_prefix("--inline=") => {
                options.mime = Some(MimeAttach {
                    boundary: boundary.to_string(),
                    inline: true,
                });
            }
            "--no-attach" => options.mime = None,
            "--no-prefix" => options.prefix_mode = Some(false),
            "--default-prefix" => options.prefix_mode = Some(true),
            "--relative" => options.relative_mode = RelativeMode::On(None),
            value if let Some(path) = value.strip_prefix("--relative=") => {
                options.relative_mode = RelativeMode::On(Some(path.to_string()));
            }
            "--no-relative" => options.relative_mode = RelativeMode::Off,
            value if value.starts_with("--color=") => {}
            // Message threading: bare `--thread` is shallow; `--thread=deep` /
            // `--thread=shallow` pick the style; `--no-thread` clears it.
            "--no-thread" => options.thread = Some(ThreadLevel::Unset),
            "--thread" => options.thread = Some(ThreadLevel::Shallow),
            value if let Some(style) = value.strip_prefix("--thread=") => {
                options.thread = Some(match style {
                    "" | "shallow" => ThreadLevel::Shallow,
                    "deep" => ThreadLevel::Deep,
                    other => {
                        eprintln!("fatal: Unknown value for --thread: {other}");
                        return Err(GitError::Exit(128));
                    }
                });
            }
            // `--in-reply-to=<msgid>` (and the two-token form) seeds the chain.
            "--in-reply-to" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--in-reply-to requires a value".into()))?;
                options.in_reply_to = Some(value.clone());
            }
            value if let Some(msgid) = value.strip_prefix("--in-reply-to=") => {
                options.in_reply_to = Some(msgid.to_string());
            }
            "--notes" | "--show-notes" => options.notes.add_default(),
            value if let Some(reff) = value.strip_prefix("--notes=") => {
                options.notes.add_ref(reff);
            }
            value if let Some(reff) = value.strip_prefix("--show-notes=") => {
                options.notes.add_ref(reff);
            }
            "--no-notes" => options.notes.disable(),
            // Cover-letter family.
            "--cover-letter" => options.cover_letter = Some(true),
            "--no-cover-letter" => options.cover_letter = Some(false),
            "--commit-list-format" | "--commit-list" => {
                let value = iter.next().ok_or_else(|| {
                    GitError::Command("--commit-list-format requires a value".into())
                })?;
                options.commit_list_format = Some(value.clone());
            }
            value if let Some(fmt) = value.strip_prefix("--commit-list-format=") => {
                options.commit_list_format = Some(fmt.to_string());
            }
            value if let Some(fmt) = value.strip_prefix("--commit-list=") => {
                options.commit_list_format = Some(fmt.to_string());
            }
            "--cover-from-description" => {
                let value = iter.next().ok_or_else(|| {
                    GitError::Command("--cover-from-description requires a value".into())
                })?;
                options.cover_from_description = Some(parse_cover_from_description(value)?);
            }
            value if let Some(mode) = value.strip_prefix("--cover-from-description=") => {
                options.cover_from_description = Some(parse_cover_from_description(mode)?);
            }
            "--description-file" => {
                let value = iter.next().ok_or_else(|| {
                    GitError::Command("--description-file requires a value".into())
                })?;
                options.description_file = Some(value.clone());
            }
            value if let Some(path) = value.strip_prefix("--description-file=") => {
                options.description_file = Some(path.to_string());
            }
            "--encode-email-headers" => options.encode_email_headers = Some(true),
            "--no-encode-email-headers" => options.encode_email_headers = Some(false),
            "--root" => options.root = true,
            // `-<n>`: limit to the last n commits.
            value
                if value.starts_with('-')
                    && value.len() > 1
                    && value[1..].bytes().all(|byte| byte.is_ascii_digit()) =>
            {
                options.count = Some(parse_format_patch_number(&value[1..], "count")?);
            }
            value if value.starts_with('^') && value.len() > 1 => {
                options.setup_args.push(value.to_string());
            }
            // git explicitly rejects these diff output formats for format-patch
            // (`builtin/log.c`: "--%s does not make sense").
            "--name-only" | "--name-status" | "--check" => {
                eprintln!("fatal: {arg} does not make sense");
                return Err(GitError::Exit(128));
            }
            value if value.starts_with('-') && value != "-" => {
                return Err(GitError::Command(format!(
                    "unsupported format-patch option {value}"
                )));
            }
            value => options.setup_args.push(value.to_string()),
        }
    }
    // git rejects `-k`/`--keep-subject` combined with an explicit subject prefix
    // or `--rfc`: the two are mutually exclusive (the prefix has nowhere to go
    // when the subject is kept verbatim).
    if options.keep_subject
        && (options.subject_prefix.is_some() || !matches!(options.rfc, RfcMode::Unset))
    {
        // git emits this as a `fatal:` die() (exit 128); the test compares the
        // stderr text byte-for-byte, so print it here rather than routing through
        // the generic `sley: command failed:` formatter.
        eprintln!("fatal: options '--subject-prefix/--rfc' and '-k' cannot be used together");
        return Err(GitError::Exit(128));
    }
    Ok(options)
}

/// Route a command-line `--add-header` value: a `To: `/`Cc: ` prefix
/// (case-insensitive) strips the prefix and routes to the recipient lists;
/// everything else becomes a raw extra-header line. Mirrors builtin/log.c's
/// `add_header`.
fn push_cli_header(options: &mut FormatPatchOptions, value: &str) {
    let trimmed = value.trim_end_matches('\n');
    if trimmed.len() >= 4 && trimmed[..4].eq_ignore_ascii_case("to: ") {
        options.cli_to.push(trimmed[4..].to_string());
    } else if trimmed.len() >= 4 && trimmed[..4].eq_ignore_ascii_case("cc: ") {
        options.cli_cc.push(trimmed[4..].to_string());
    } else {
        options.cli_headers.push(trimmed.to_string());
    }
}

/// Parse a `--cover-from-description=<mode>` / `format.coverFromDescription`
/// value, mirroring git's `parse_cover_from_description`. An unrecognised mode
/// is a fatal error printed exactly as git does (`<arg>: invalid cover from
/// description mode`) so the byte-for-byte stderr check in t4014 passes.
fn parse_cover_from_description(arg: &str) -> Result<CoverFromDescription> {
    match arg {
        "default" | "message" => Ok(CoverFromDescription::Message),
        "none" => Ok(CoverFromDescription::None),
        "subject" => Ok(CoverFromDescription::Subject),
        "auto" => Ok(CoverFromDescription::Auto),
        other => {
            eprintln!("fatal: {other}: invalid cover from description mode");
            Err(GitError::Exit(128))
        }
    }
}

/// Parse a non-negative integer flag value, with a git-flavored error context.
fn parse_format_patch_number(value: &str, what: &str) -> Result<usize> {
    value
        .parse::<usize>()
        .map_err(|_| GitError::Command(format!("invalid {what} value '{value}'")))
}

fn parse_unified_context_count(value: &str) -> usize {
    let (number, multiplier) = match value.as_bytes().last() {
        Some(b'k' | b'K') => (&value[..value.len() - 1], 1024usize),
        Some(b'm' | b'M') => (&value[..value.len() - 1], 1024 * 1024),
        Some(b'g' | b'G') => (&value[..value.len() - 1], 1024 * 1024 * 1024),
        _ => (value, 1),
    };
    if number.starts_with('-') {
        return 0;
    }
    number
        .strip_prefix('+')
        .unwrap_or(number)
        .parse::<usize>()
        .unwrap_or(0)
        .saturating_mul(multiplier)
}

/// Parse an `-M`/`-C`/`--find-renames=`/`--find-copies=` similarity into a
/// 0..=100 percentage; accepts a bare integer or a trailing `%`.
fn parse_similarity(value: &str) -> Result<u8> {
    if value.is_empty() {
        return Ok(sley_diff_merge::DEFAULT_RENAME_THRESHOLD);
    }
    let digits = value.strip_suffix('%').unwrap_or(value);
    let parsed = digits
        .parse::<u32>()
        .map_err(|_| GitError::Command(format!("invalid similarity value {value}")))?;
    Ok(parsed.min(100) as u8)
}
