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
//! diff/stat rendering in the shared
//! crate writes only to `io::Stdout`; format-patch needs to direct output at a
//! file too, so the unified-patch, diffstat, and summary rendering is reproduced
//! here against an in-memory byte buffer using the same `sley_diff_merge` engine
//! (name-status diff + blob reads) the rest of the CLI uses.

// Glob the crate root for shared plumbing; see commands::stash for rationale.
use crate::*;

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

/// Parsed `git format-patch` invocation.
struct FormatPatchOptions {
    /// Stream all patches to stdout instead of writing files (`--stdout`).
    stdout: bool,
    /// Output directory for the `.patch` files (`-o`/`--output-directory`).
    output_directory: Option<String>,
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
    /// Custom subject prefix replacing `PATCH` (`--subject-prefix=<p>`).
    subject_prefix: String,
    /// String inserted just before the prefix, e.g. `RFC ` (`--rfc`).
    reroll_prefix: Option<String>,
    /// `-k`/`--keep-subject`: emit the commit subject verbatim with no
    /// `[PATCH ...]` prefix.
    keep_subject: bool,
    /// `-<n>`: limit to the last n commits of the default tip.
    count: Option<usize>,
    /// `--numbered-files`: name output files `1`, `2`, ... with no slug.
    numbered_files: bool,
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
    /// Positional revision arguments (single committish or a range).
    revisions: Vec<String>,
}

impl Default for FormatPatchOptions {
    fn default() -> Self {
        Self {
            stdout: false,
            output_directory: None,
            number_mode: NumberMode::Auto,
            start_number: None,
            signoff: false,
            stat: true,
            stat_widths: DiffStatWidths::plumbing(),
            stat_count: None,
            subject_prefix: "PATCH".to_string(),
            reroll_prefix: None,
            keep_subject: false,
            count: None,
            numbered_files: false,
            full_index: false,
            abbrev: None,
            detect_renames: true,
            detect_copies: false,
            find_copies_harder: false,
            rename_threshold: sley_diff_merge::DEFAULT_RENAME_THRESHOLD,
            copy_threshold: sley_diff_merge::DEFAULT_RENAME_THRESHOLD,
            revisions: Vec::new(),
        }
    }
}

pub(crate) fn cmd_format_patch(args: &[String]) -> Result<()> {
    let options = parse_format_patch_args(args)?;

    let repo = RepositoryContext::discover_current()?;
    let cwd = repo.cwd();
    let git_dir = repo.git_dir();
    let format = repo.format();
    let config = repo.config();
    let db = repo.objects();

    let commits = select_commits(&repo, &options)?;

    let count = commits.len();
    let numbered = match options.number_mode {
        NumberMode::Numbered => true,
        NumberMode::Unnumbered => false,
        // Auto-numbering keys off the count actually emitted, not the start
        // offset: a single patch is unnumbered, several are numbered.
        NumberMode::Auto => count > 1,
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

    if options.stdout {
        let mut stdout = io::stdout();
        for (idx, record) in commits.iter().enumerate() {
            // In stream mode git separates consecutive patches with an extra
            // blank line (on top of each patch's own trailing blank).
            if idx > 0 {
                stdout.write_all(b"\n")?;
            }
            let buffer = render_patch(RenderContext {
                db,
                format,
                options: &options,
                record,
                seq: start_number + idx,
                last_number,
                numbered,
                signoff_line: signoff_line.as_deref(),
                abbrev,
            })?;
            stdout.write_all(&buffer)?;
        }
        stdout.flush()?;
        return Ok(());
    }

    let out_dir = options.output_directory.as_deref().unwrap_or(".");
    let out_dir_path = resolve_cli_path(cwd, out_dir);
    fs::create_dir_all(&out_dir_path)?;
    let mut stdout = io::stdout();
    for (idx, record) in commits.iter().enumerate() {
        let seq = start_number + idx;
        let buffer = render_patch(RenderContext {
            db,
            format,
            options: &options,
            record,
            seq,
            last_number,
            numbered,
            signoff_line: signoff_line.as_deref(),
            abbrev,
        })?;
        let file_name = if options.numbered_files {
            seq.to_string()
        } else {
            patch_file_name(seq, &record.commit.message)
        };
        let file_path = out_dir_path.join(&file_name);
        fs::write(&file_path, &buffer)?;
        // git prints the path as joined with the user-provided directory string
        // (so a relative `-o build` yields `build/0001-...patch`).
        let display = Path::new(out_dir).join(&file_name);
        writeln!(stdout, "{}", display.display())?;
    }
    stdout.flush()?;
    Ok(())
}

/// Bundle of everything a single patch needs to render, to keep the helper from
/// taking a dozen positional arguments.
struct RenderContext<'a> {
    db: &'a FileObjectDatabase,
    format: ObjectFormat,
    options: &'a FormatPatchOptions,
    record: &'a sley_rev::CommitRecord,
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
}

/// Render one commit into a complete mbox patch byte buffer.
fn render_patch(ctx: RenderContext<'_>) -> Result<Vec<u8>> {
    let RenderContext {
        db,
        format,
        options,
        record,
        seq,
        last_number,
        numbered,
        signoff_line,
        abbrev,
    } = ctx;

    let commit = &record.commit;
    let mut out = Vec::new();

    // mbox `From ` separator: the commit oid + the fixed magic date git uses.
    out.extend_from_slice(b"From ");
    out.extend_from_slice(record.oid.to_hex().as_bytes());
    out.extend_from_slice(b" Mon Sep 17 00:00:00 2001\n");

    // From: header (author identity).
    let (author_name, author_email) = commit_identity_name_email(&commit.author);
    out.extend_from_slice(format!("From: {author_name} <{author_email}>\n").as_bytes());

    // Date: header — the *author* date in git's RFC 2822 rendering.
    let date = commit_identity_date(&commit.author, ForEachRefDateMode::Rfc2822);
    out.extend_from_slice(format!("Date: {date}\n").as_bytes());

    // Subject: [PREFIX n/m] <subject> (or, with -k/--keep-subject, the bare
    // subject), folded to <=78 columns.
    let prefix = if options.keep_subject {
        None
    } else {
        Some(subject_prefix_label(options, seq, last_number, numbered))
    };
    let subject = commit_subject(&commit.message);
    write_folded_subject(&mut out, prefix.as_deref(), &subject);

    // Blank line, then the commit body (message minus the subject line),
    // normalized to end in exactly one newline. With --signoff the trailer is
    // appended to the body.
    out.push(b'\n');
    let body = format_patch_body(&commit.message, signoff_line);
    out.extend_from_slice(&body);

    // Diff entries against the first parent (or the empty tree for a root).
    let entries = first_parent_diff_entries(db, format, options, commit)?;

    // The `---`/diffstat/diff block is emitted only when the commit actually
    // changes something. An empty commit goes straight from the message to the
    // `-- ` signature. When there are changes, the `---` separator introduces the
    // diffstat block (the default `--stat`), which `--no-stat` collapses to a
    // single blank line.
    if !entries.is_empty() {
        if options.stat {
            out.extend_from_slice(b"---\n");
            write_patch_diffstat(&mut out, &entries, db, options)?;
            for entry in &entries {
                write_patch_summary_entry(&mut out, entry)?;
            }
            out.push(b'\n');
        } else {
            out.push(b'\n');
        }

        for entry in &entries {
            write_patch_diff_entry(&mut out, entry, db, format, options, abbrev)?;
        }
    }

    // Signature trailer. The preceding content already ends in a newline, so
    // the `-- ` separator follows directly (no intervening blank line). Every
    // patch ends with a trailing blank line — in files and on stdout alike;
    // stdout additionally inserts a separator *between* patches.
    out.extend_from_slice(b"-- \n");
    out.extend_from_slice(sley_core::UPSTREAM_GIT_COMPAT_VERSION.as_bytes());
    out.extend_from_slice(b"\n\n");
    Ok(out)
}

/// Build the `[PATCH n/m]` / `[PATCH]` prefix string (without the trailing
/// space that separates it from the subject — that is added by the folder).
fn subject_prefix_label(
    options: &FormatPatchOptions,
    seq: usize,
    last_number: usize,
    numbered: bool,
) -> String {
    let mut label = String::from("[");
    if let Some(reroll) = &options.reroll_prefix {
        label.push_str(reroll);
    }
    label.push_str(&options.subject_prefix);
    if numbered {
        label.push_str(&format!(" {seq}/{last_number}"));
    }
    label.push(']');
    label
}

/// Append the `Subject:` header, folding so each output line is at most 78
/// columns (continuation lines are indented by a single space), matching git's
/// RFC 2822 subject wrapping.
///
/// The first line starts with `Subject: <prefix> ` (or just `Subject: ` when
/// `prefix` is `None`, for `-k`/`--keep-subject`); the subject words are then
/// packed greedily. A word is moved to a fresh continuation line (indent: one
/// space) when appending it would push the line past 78 columns *and* the
/// current line already carries content that can be "left behind" — the prefix
/// on the first line, or at least one already-placed word on a continuation
/// line. A single word longer than the budget therefore lands alone on its own
/// over-long line rather than being split.
fn write_folded_subject(out: &mut Vec<u8>, prefix: Option<&str>, subject: &str) {
    const WRAP: usize = 78;
    let mut line = match prefix {
        Some(prefix) => format!("Subject: {prefix} "),
        None => String::from("Subject: "),
    };
    for word in subject.split(' ') {
        if word.is_empty() {
            // `split(' ')` yields empty strings for runs of spaces; git does not
            // preserve those in subjects, so skip them.
            continue;
        }
        // The current line always carries content that can be left behind when a
        // word wraps — the prefix on the first line, or an already-placed word on
        // a continuation line — because every wrap immediately appends the word
        // that triggered it. So a word folds whenever appending it would exceed
        // the budget; an over-long word that does not fit even at the start of a
        // line ends up alone on its own (over-long) line.
        let needs_space = !line.ends_with(' ');
        let candidate = line.len() + usize::from(needs_space) + word.len();
        if candidate > WRAP {
            // Flush the wrapped line verbatim: when only the prefix is present
            // (the first word is being shed), git keeps the trailing space, so
            // do not trim flushed lines.
            out.extend_from_slice(line.as_bytes());
            out.push(b'\n');
            // Start a continuation line indented by one space.
            line = String::from(" ");
        }
        if !line.ends_with(' ') {
            line.push(' ');
        }
        line.push_str(word);
    }
    // Trim only the final line: an empty subject leaves the prefix line ending
    // in a space, which git emits without the trailing space.
    out.extend_from_slice(line.trim_end().as_bytes());
    out.push(b'\n');
}

/// Produce the patch body: the commit message with its subject line removed,
/// guaranteed to end in exactly one newline (empty body yields no bytes other
/// than the optional sign-off). The `---` separator follows whatever this
/// returns.
fn format_patch_body(message: &[u8], signoff_line: Option<&[u8]>) -> Vec<u8> {
    let mut body = commit_body(message).to_vec();
    // Strip any trailing newlines, then re-add a single one (when non-empty) so
    // the body always ends "...text\n" before the sign-off / separator.
    while body.last() == Some(&b'\n') {
        body.pop();
    }
    if let Some(signoff) = signoff_line {
        if body.is_empty() {
            body.extend_from_slice(signoff);
            body.push(b'\n');
            return body;
        }
        body.push(b'\n');
        // git places a blank line before the sign-off trailer unless the body
        // already ends with a recognised trailer block; reproduce the simple
        // common case (blank line then the trailer).
        body.push(b'\n');
        body.extend_from_slice(signoff);
        body.push(b'\n');
        return body;
    }
    if !body.is_empty() {
        body.push(b'\n');
    }
    body
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

/// Select the commits to format, newest-to-oldest from the walk then reversed to
/// git's oldest-first output order, with merge commits dropped (format-patch is
/// `--no-merges` by default).
fn select_commits(
    repo: &RepositoryContext,
    options: &FormatPatchOptions,
) -> Result<Vec<sley_rev::CommitRecord>> {
    let format = repo.format();
    let db = repo.objects();
    let mut includes: Vec<String> = Vec::new();
    let mut excludes: Vec<String> = Vec::new();
    let mut linear_ranges: Vec<(String, String, bool)> = Vec::new();
    let mut symmetric_ranges: Vec<(String, String, bool)> = Vec::new();

    for rev in &options.revisions {
        if rev.contains("..") {
            add_rev_list_revision_arg(
                rev,
                false,
                &mut includes,
                &mut excludes,
                &mut linear_ranges,
                &mut symmetric_ranges,
            )?;
        } else if let Some(exclude) = rev.strip_prefix('^') {
            excludes.push(exclude.to_string());
        } else if options.count.is_some() {
            // With `-<n>`, a bare committish X is the *tip* of the walk (format
            // `n` commits ending at X), not the `X..HEAD` exclude it means on
            // its own.
            includes.push(rev.clone());
        } else {
            // A bare committish X means "X..HEAD" for format-patch.
            includes.push("HEAD".to_string());
            excludes.push(rev.clone());
        }
    }

    // With no positional revisions, default to HEAD (optionally limited by -n).
    if includes.is_empty()
        && excludes.is_empty()
        && linear_ranges.is_empty()
        && symmetric_ranges.is_empty()
    {
        includes.push("HEAD".to_string());
    }

    let mut starts = Vec::new();
    for rev in &includes {
        starts.push(resolve_format_patch_commit(repo, rev)?);
    }
    let mut range_excludes = Vec::new();
    for (left, right, _not) in &linear_ranges {
        let left_oid = resolve_format_patch_commit(repo, left)?;
        let right_oid = resolve_format_patch_commit(repo, right)?;
        range_excludes.push(left_oid);
        starts.push(right_oid);
    }
    for (left, right, _not) in &symmetric_ranges {
        let left_oid = resolve_format_patch_commit(repo, left)?;
        let right_oid = resolve_format_patch_commit(repo, right)?;
        let bases = merge_bases(repo.git_dir(), db, format, &left_oid, &right_oid)?;
        starts.push(left_oid);
        starts.push(right_oid);
        range_excludes.extend(bases);
    }

    let mut excluded = HashSet::new();
    for oid in range_excludes {
        for record in rev_list_walk_commits(db, format, [oid], false)? {
            excluded.insert(record.oid);
        }
    }
    for rev in &excludes {
        let oid = resolve_format_patch_commit(repo, rev)?;
        for record in rev_list_walk_commits(db, format, [oid], false)? {
            excluded.insert(record.oid);
        }
    }

    let walked = rev_list_walk_commits(db, format, starts, false)?;
    // Keep non-excluded, non-merge commits (newest-first from the walk).
    let mut selected: Vec<sley_rev::CommitRecord> = walked
        .into_iter()
        .filter(|record| !excluded.contains(&record.oid) && record.parents.len() <= 1)
        .collect();

    // `-<n>` keeps the n newest of those before reversing to oldest-first.
    if let Some(count) = options.count {
        selected.truncate(count);
    }
    selected.reverse();
    Ok(selected)
}

/// Resolve a revision string to a commit id, emitting git's exact
/// "ambiguous argument ... unknown revision" fatal (exit 128) when it cannot be
/// resolved or peeled — the same message `git format-patch <bad-rev>` prints.
fn resolve_format_patch_commit(repo: &RepositoryContext, rev: &str) -> Result<ObjectId> {
    let oid = repo
        .resolve_revision(rev)
        .map_err(|_| unknown_revision_error(rev))?;
    sley_rev::peel_to_commit(repo.objects(), repo.format(), &oid)
        .map_err(|_| unknown_revision_error(rev))
}

/// Print git's "unknown revision or path" fatal block and return an exit-128
/// error, matching the stderr `git format-patch <bad-rev>` produces.
fn unknown_revision_error(spec: &str) -> GitError {
    eprintln!(
        "fatal: ambiguous argument '{spec}': unknown revision or path not in the working tree."
    );
    eprintln!(
        "Use '--' to separate paths from revisions, like this:\n'git <command> [<revision>...] -- [<file>...]'"
    );
    GitError::Exit(128)
}

/// Build the output file name `NNNN-<slug>.patch` for the patch numbered `seq`.
fn patch_file_name(seq: usize, message: &[u8]) -> String {
    let slug = sanitize_patch_subject(message);
    format!("{seq:04}-{slug}.patch")
}

/// git's `format_sanitized_subject`: keep alphanumerics, `.` and `_`; collapse
/// each run of other characters to a single `-`; collapse consecutive dots; no
/// leading separator; trim trailing `-`/`.`; then hard-truncate to 52 bytes.
fn sanitize_patch_subject(message: &[u8]) -> String {
    const MAX: usize = 52;
    let subject = commit_subject(message);
    let bytes = subject.as_bytes();
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
    if out.len() > MAX {
        out.truncate(MAX);
    }
    out
}

/// A "title" character for filename sanitization: ASCII alphanumeric, `.`, `_`.
fn is_title_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'.' || byte == b'_'
}

// --- diff rendering into a byte buffer ----------------------------------------
//
// The shared crate's `write_diff_*` helpers target `io::Stdout` exclusively;
// format-patch must also write into files, so the rendering below mirrors that
// logic against a `Vec<u8>`, using the same `sley_diff_merge` data
// (NameStatusEntry) and blob reads. Output is kept byte-for-byte compatible.

/// Render one file's unified-diff section (the `diff --git ...` block) into
/// `out`, including the mode/index/`---`/`+++` headers and the single
/// whole-file hunk. Binary changes get the `Binary files ... differ` line.
fn write_patch_diff_entry(
    out: &mut Vec<u8>,
    entry: &sley_diff_merge::NameStatusEntry,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    options: &FormatPatchOptions,
    abbrev: usize,
) -> Result<()> {
    let old_content = entry_old_content(entry, db)?;
    let new_content = entry_new_content(entry, db)?;
    let content_changed = old_content.as_deref() != new_content.as_deref();

    let old_path = entry.old_path.as_deref().unwrap_or(&entry.path);
    let diff_old_path = patch_prefixed_path("a/", old_path);
    let diff_new_path = patch_prefixed_path("b/", &entry.path);
    writeln_buf(out, &format!("diff --git {diff_old_path} {diff_new_path}"));
    write_patch_mode_headers(out, entry);
    write_patch_similarity_headers(out, entry, old_path, &entry.path);

    let is_binary = old_content.as_deref().is_some_and(|c| c.contains(&0))
        || new_content.as_deref().is_some_and(|c| c.contains(&0));
    if is_binary {
        if !content_changed {
            return Ok(());
        }
        writeln_buf(
            out,
            &format!(
                "index {}..{}{}",
                patch_blob_oid(
                    entry.old_oid.as_ref(),
                    old_content.as_deref(),
                    format,
                    abbrev
                ),
                patch_blob_oid(
                    entry.new_oid.as_ref(),
                    new_content.as_deref(),
                    format,
                    abbrev
                ),
                patch_mode_suffix(entry)
            ),
        );
        let old = if old_content.is_some() {
            patch_prefixed_path("a/", old_path)
        } else {
            "/dev/null".to_string()
        };
        let new = if new_content.is_some() {
            patch_prefixed_path("b/", &entry.path)
        } else {
            "/dev/null".to_string()
        };
        writeln_buf(out, &format!("Binary files {old} and {new} differ"));
        return Ok(());
    }

    if !content_changed {
        return Ok(());
    }
    writeln_buf(
        out,
        &format!(
            "index {}..{}{}",
            patch_blob_oid(
                entry.old_oid.as_ref(),
                old_content.as_deref(),
                format,
                abbrev
            ),
            patch_blob_oid(
                entry.new_oid.as_ref(),
                new_content.as_deref(),
                format,
                abbrev
            ),
            patch_mode_suffix(entry)
        ),
    );
    let _ = options; // reserved for future patch knobs; keeps the signature stable.
    match entry.status {
        sley_diff_merge::NameStatus::Added => writeln_buf(out, "--- /dev/null"),
        _ => writeln_buf(out, &format!("--- {}", patch_header_path("a/", old_path))),
    }
    match entry.status {
        sley_diff_merge::NameStatus::Deleted => writeln_buf(out, "+++ /dev/null"),
        _ => writeln_buf(
            out,
            &format!("+++ {}", patch_header_path("b/", &entry.path)),
        ),
    }
    write_patch_hunks(out, old_content.as_deref(), new_content.as_deref());
    Ok(())
}

/// Emit the `new file mode` / `deleted file mode` / `old mode`+`new mode`
/// headers appropriate for the entry's status.
fn write_patch_mode_headers(out: &mut Vec<u8>, entry: &sley_diff_merge::NameStatusEntry) {
    match entry.status {
        sley_diff_merge::NameStatus::Added => {
            if let Some(mode) = entry.new_mode {
                writeln_buf(out, &format!("new file mode {mode:06o}"));
            }
        }
        sley_diff_merge::NameStatus::Deleted => {
            if let Some(mode) = entry.old_mode {
                writeln_buf(out, &format!("deleted file mode {mode:06o}"));
            }
        }
        sley_diff_merge::NameStatus::Modified
        | sley_diff_merge::NameStatus::Renamed(_)
        | sley_diff_merge::NameStatus::Copied(_) => {
            if let (Some(old_mode), Some(new_mode)) = (entry.old_mode, entry.new_mode)
                && old_mode != new_mode
            {
                writeln_buf(out, &format!("old mode {old_mode:06o}"));
                writeln_buf(out, &format!("new mode {new_mode:06o}"));
            }
        }
    }
}

/// Emit the `similarity index`/`rename from|to`/`copy from|to` headers for
/// rename and copy entries.
fn write_patch_similarity_headers(
    out: &mut Vec<u8>,
    entry: &sley_diff_merge::NameStatusEntry,
    old_path: &[u8],
    path: &[u8],
) {
    let old = status_quote_path(old_path, false);
    let new = status_quote_path(path, false);
    match entry.status {
        sley_diff_merge::NameStatus::Renamed(score) => {
            writeln_buf(out, &format!("similarity index {score}%"));
            writeln_buf(out, &format!("rename from {old}"));
            writeln_buf(out, &format!("rename to {new}"));
        }
        sley_diff_merge::NameStatus::Copied(score) => {
            writeln_buf(out, &format!("similarity index {score}%"));
            writeln_buf(out, &format!("copy from {old}"));
            writeln_buf(out, &format!("copy to {new}"));
        }
        _ => {}
    }
}

/// Number of unchanged lines of context git keeps around each change in a hunk.
const HUNK_CONTEXT: usize = 3;

/// The per-line origin marker for an emitted diff line.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LineKind {
    Context,
    Delete,
    Insert,
}

/// One line of the unified diff, with its origin and 0-based positions in the
/// old/new files (used to compute hunk ranges).
struct TaggedLine<'a> {
    kind: LineKind,
    content: &'a [u8],
    old_index: usize,
    new_index: usize,
}

/// Emit the unified-diff hunks for a single file change into `out`, grouping
/// changes with [`HUNK_CONTEXT`] lines of surrounding context (merging nearby
/// groups), and prefixing each `@@` header with git's default section heading.
pub(crate) fn write_patch_hunks(
    out: &mut Vec<u8>,
    old_content: Option<&[u8]>,
    new_content: Option<&[u8]>,
) {
    let old = sley_diff_merge::split_lines(old_content.unwrap_or_default());
    let new = sley_diff_merge::split_lines(new_content.unwrap_or_default());
    let ops = sley_diff_merge::myers_diff_lines(&old, &new);

    // Flatten the edit script into a tagged line stream carrying old/new
    // positions.
    let mut tagged: Vec<TaggedLine<'_>> = Vec::new();
    let mut old_idx = 0usize;
    let mut new_idx = 0usize;
    for op in ops {
        match op {
            sley_diff_merge::DiffOp::Equal(n) => {
                for _ in 0..n {
                    tagged.push(TaggedLine {
                        kind: LineKind::Context,
                        content: old[old_idx].content,
                        old_index: old_idx,
                        new_index: new_idx,
                    });
                    old_idx += 1;
                    new_idx += 1;
                }
            }
            sley_diff_merge::DiffOp::Delete(n) => {
                for _ in 0..n {
                    tagged.push(TaggedLine {
                        kind: LineKind::Delete,
                        content: old[old_idx].content,
                        old_index: old_idx,
                        new_index: new_idx,
                    });
                    old_idx += 1;
                }
            }
            sley_diff_merge::DiffOp::Insert(n) => {
                for _ in 0..n {
                    tagged.push(TaggedLine {
                        kind: LineKind::Insert,
                        content: new[new_idx].content,
                        old_index: old_idx,
                        new_index: new_idx,
                    });
                    new_idx += 1;
                }
            }
        }
    }

    // Indices of changed (non-context) lines.
    let change_positions: Vec<usize> = tagged
        .iter()
        .enumerate()
        .filter(|(_, line)| line.kind != LineKind::Context)
        .map(|(idx, _)| idx)
        .collect();
    if change_positions.is_empty() {
        return;
    }

    // Group changes whose context windows overlap into single hunks.
    let mut groups: Vec<(usize, usize)> = Vec::new();
    let mut group_start = change_positions[0];
    let mut group_end = change_positions[0];
    for &pos in &change_positions[1..] {
        // Two change runs merge when at most 2*HUNK_CONTEXT context lines
        // separate them (so their context windows touch or overlap).
        if pos - group_end <= 2 * HUNK_CONTEXT {
            group_end = pos;
        } else {
            groups.push((group_start, group_end));
            group_start = pos;
            group_end = pos;
        }
    }
    groups.push((group_start, group_end));

    for (first_change, last_change) in groups {
        let hunk_start = first_change.saturating_sub(HUNK_CONTEXT);
        let hunk_end = (last_change + HUNK_CONTEXT + 1).min(tagged.len());
        write_one_hunk(out, &tagged, &old, hunk_start, hunk_end);
    }
}

/// Emit a single hunk covering `tagged[start..end]`: the `@@ -os,oc +ns,nc @@
/// <heading>` header followed by the context/`-`/`+` lines, including the
/// `\ No newline at end of file` markers.
fn write_one_hunk(
    out: &mut Vec<u8>,
    tagged: &[TaggedLine<'_>],
    old_lines: &[sley_diff_merge::DiffLine<'_>],
    start: usize,
    end: usize,
) {
    let slice = &tagged[start..end];
    let mut old_count = 0usize;
    let mut new_count = 0usize;
    for line in slice {
        match line.kind {
            LineKind::Context => {
                old_count += 1;
                new_count += 1;
            }
            LineKind::Delete => old_count += 1,
            LineKind::Insert => new_count += 1,
        }
    }
    // 1-based starting line numbers; an empty side starts at 0.
    let old_start = if old_count == 0 {
        slice.first().map(|line| line.old_index).unwrap_or(0)
    } else {
        slice
            .iter()
            .find(|line| line.kind != LineKind::Insert)
            .map(|line| line.old_index + 1)
            .unwrap_or(1)
    };
    let new_start = if new_count == 0 {
        slice.first().map(|line| line.new_index).unwrap_or(0)
    } else {
        slice
            .iter()
            .find(|line| line.kind != LineKind::Delete)
            .map(|line| line.new_index + 1)
            .unwrap_or(1)
    };

    let heading = hunk_section_heading(old_lines, slice.first().map(|line| line.old_index));
    out.extend_from_slice(b"@@ -");
    out.extend_from_slice(format_hunk_range(old_start, old_count).as_bytes());
    out.extend_from_slice(b" +");
    out.extend_from_slice(format_hunk_range(new_start, new_count).as_bytes());
    out.extend_from_slice(b" @@");
    if let Some(heading) = heading {
        out.push(b' ');
        out.extend_from_slice(heading);
    }
    out.push(b'\n');

    for line in slice {
        let prefix = match line.kind {
            LineKind::Context => b' ',
            LineKind::Delete => b'-',
            LineKind::Insert => b'+',
        };
        write_patch_line(out, prefix, line.content);
    }
}

/// Format one `start,count` side of an `@@` header. git omits the count when it
/// is exactly 1 (e.g. `+5` rather than `+5,1`).
fn format_hunk_range(start: usize, count: usize) -> String {
    if count == 1 {
        start.to_string()
    } else {
        format!("{start},{count}")
    }
}

/// git's default section heading for a hunk: the nearest line *before* the
/// hunk's first line whose first byte looks like the start of a function (an
/// ASCII letter, `_`, or `$`), returned without its trailing newline. Returns
/// `None` when no such line precedes the hunk.
fn hunk_section_heading<'a>(
    old_lines: &[sley_diff_merge::DiffLine<'a>],
    first_old_index: Option<usize>,
) -> Option<&'a [u8]> {
    let first = first_old_index?;
    // Scan upward from the line just above the hunk.
    for idx in (0..first).rev() {
        let line = old_lines[idx].bytes_without_newline();
        if line.first().is_some_and(is_funcname_start) {
            return Some(line);
        }
    }
    None
}

/// Whether `byte` can begin a git default-funcname section heading line.
fn is_funcname_start(byte: &u8) -> bool {
    byte.is_ascii_alphabetic() || *byte == b'_' || *byte == b'$'
}

/// Write a single diff line with its `prefix` marker, appending the
/// `\ No newline at end of file` note when the source line lacks a trailing LF.
fn write_patch_line(out: &mut Vec<u8>, prefix: u8, line: &[u8]) {
    out.push(prefix);
    out.extend_from_slice(line);
    if !line.ends_with(b"\n") {
        out.extend_from_slice(b"\n\\ No newline at end of file\n");
    }
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
    let mut widths = options.stat_widths;
    if widths.stat_width == 0 {
        // MAIL_DEFAULT_WRAP
        widths.stat_width = 72;
    }
    write_diff_stat_with_widths(
        out,
        entries,
        db,
        None,
        false,
        DiffStatOptions {
            compact_summary: false,
            stat_count: options.stat_count,
            color: false,
        },
        widths,
    )
}






/// The ` create mode`/` delete mode`/` rename`/` copy`/` mode change` summary
/// line for a single entry (the `--summary` lines format-patch always includes
/// when stats are on), mirroring the shared renderer.
fn write_patch_summary_entry(
    out: &mut Vec<u8>,
    entry: &sley_diff_merge::NameStatusEntry,
) -> Result<()> {
    match entry.status {
        sley_diff_merge::NameStatus::Added => {
            let mode = entry.new_mode.unwrap_or(0);
            writeln_buf(
                out,
                &format!(
                    " create mode {mode:06o} {}",
                    status_quote_path(&entry.path, false)
                ),
            );
        }
        sley_diff_merge::NameStatus::Deleted => {
            let mode = entry.old_mode.unwrap_or(0);
            writeln_buf(
                out,
                &format!(
                    " delete mode {mode:06o} {}",
                    status_quote_path(&entry.path, false)
                ),
            );
        }
        sley_diff_merge::NameStatus::Renamed(score) => {
            if let Some(old_path) = &entry.old_path {
                writeln_buf(
                    out,
                    &format!(
                        " rename {} => {} ({score}%)",
                        status_quote_path(old_path, false),
                        status_quote_path(&entry.path, false)
                    ),
                );
            }
        }
        sley_diff_merge::NameStatus::Copied(score) => {
            if let Some(old_path) = &entry.old_path {
                writeln_buf(
                    out,
                    &format!(
                        " copy {} => {} ({score}%)",
                        status_quote_path(old_path, false),
                        status_quote_path(&entry.path, false)
                    ),
                );
            }
        }
        sley_diff_merge::NameStatus::Modified => {
            if entry.old_mode != entry.new_mode
                && let (Some(old_mode), Some(new_mode)) = (entry.old_mode, entry.new_mode)
            {
                writeln_buf(
                    out,
                    &format!(
                        " mode change {old_mode:06o} => {new_mode:06o} {}",
                        status_quote_path(&entry.path, false)
                    ),
                );
            }
        }
    }
    Ok(())
}


/// Read the old blob for an entry, if it has one.
fn entry_old_content(
    entry: &sley_diff_merge::NameStatusEntry,
    db: &FileObjectDatabase,
) -> Result<Option<Vec<u8>>> {
    entry
        .old_oid
        .as_ref()
        .map(|oid| read_patch_blob(db, oid))
        .transpose()
}

/// Read the new blob for an entry (tree-to-tree; never the worktree), if any.
fn entry_new_content(
    entry: &sley_diff_merge::NameStatusEntry,
    db: &FileObjectDatabase,
) -> Result<Option<Vec<u8>>> {
    if entry.new_mode.is_none() {
        return Ok(None);
    }
    entry
        .new_oid
        .as_ref()
        .map(|oid| read_patch_blob(db, oid))
        .transpose()
}

/// Read a blob object's bytes, erroring if the id is not a blob.
fn read_patch_blob(db: &FileObjectDatabase, oid: &ObjectId) -> Result<Vec<u8>> {
    let object = db.read_object(oid)?;
    if object.object_type != ObjectType::Blob {
        return Err(GitError::InvalidObject(format!(
            "format-patch expected blob object {oid}"
        )));
    }
    Ok(object.body.clone())
}

/// Render the `<prefix><path>` token for a `diff --git` line / `Binary files`
/// line, C-quoting if needed.
fn patch_prefixed_path(prefix: &str, path: &[u8]) -> String {
    let mut bytes = Vec::with_capacity(prefix.len() + path.len());
    bytes.extend_from_slice(prefix.as_bytes());
    bytes.extend_from_slice(path);
    status_quote_path(&bytes, false)
}

/// Render the `<prefix><path>` token for a `---`/`+++` header line. git appends
/// a literal tab when the (unquoted) name contains a space, so downstream
/// parsers can find the path boundary.
fn patch_header_path(prefix: &str, path: &[u8]) -> String {
    let mut bytes = Vec::with_capacity(prefix.len() + path.len());
    bytes.extend_from_slice(prefix.as_bytes());
    bytes.extend_from_slice(path);
    let mut quoted = status_quote_path(&bytes, false);
    if !quoted.starts_with('"') && bytes.contains(&b' ') {
        quoted.push('\t');
    }
    quoted
}

/// Abbreviated (or zero-filled, for /dev/null sides) blob id for an `index`
/// line, computing it from content when the entry lacks a stored oid.
fn patch_blob_oid(
    oid: Option<&ObjectId>,
    content: Option<&[u8]>,
    format: ObjectFormat,
    abbrev: usize,
) -> String {
    let hex = oid
        .cloned()
        .or_else(|| {
            content.and_then(|content| sley_core::object_id_for_bytes(format, "blob", content).ok())
        })
        .map(|oid| oid.to_hex())
        .unwrap_or_else(|| "0".repeat(format.hex_len()));
    hex[..abbrev.min(hex.len())].to_string()
}

/// The trailing ` <mode>` on an `index` line when the file mode is unchanged.
fn patch_mode_suffix(entry: &sley_diff_merge::NameStatusEntry) -> String {
    match (entry.old_mode, entry.new_mode) {
        (Some(old_mode), Some(new_mode)) if old_mode == new_mode => format!(" {old_mode:06o}"),
        _ => String::new(),
    }
}

/// Append `text` plus a newline to the buffer.
fn writeln_buf(out: &mut Vec<u8>, text: &str) {
    out.extend_from_slice(text.as_bytes());
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
            options.revisions.push(arg.clone());
            continue;
        }
        match arg.as_str() {
            "--" => positional_only = true,
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
            // git's `format-patch -p` drops the leading diffstat (like
            // `--no-stat`). The long `--patch` is the diff-machinery flag and,
            // quirkily, does *not* disable the stat — so it is a no-op here.
            "-p" => options.stat = false,
            "--patch" | "--no-patch-with-stat" => {}
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
                options.subject_prefix = prefix.to_string();
            }
            "--subject-prefix" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--subject-prefix requires a value".into()))?;
                options.subject_prefix = value.clone();
            }
            "--rfc" => options.reroll_prefix = Some("RFC ".to_string()),
            "-k" | "--keep-subject" => options.keep_subject = true,
            // Accepted-but-inert formatting knobs that do not change the bytes
            // sley emits for the common path.
            "--no-color"
            | "--color"
            | "--no-thread"
            | "--minimal"
            | "--patience"
            | "--histogram"
            | "--indent-heuristic"
            | "--no-indent-heuristic"
            | "--binary"
            | "--no-binary"
            | "--no-prefix"
            | "--text"
            | "-a"
            | "--ita-invisible-in-index"
            | "--no-signature" => {}
            value if value.starts_with("--color=") => {}
            value if value.starts_with("--thread") => {}
            value if value.starts_with("--cover-letter") => {
                return Err(GitError::Unsupported(
                    "format-patch --cover-letter is not supported".into(),
                ));
            }
            // `-<n>`: limit to the last n commits.
            value
                if value.starts_with('-')
                    && value.len() > 1
                    && value[1..].bytes().all(|byte| byte.is_ascii_digit()) =>
            {
                options.count = Some(parse_format_patch_number(&value[1..], "count")?);
            }
            value if value.starts_with('^') && value.len() > 1 => {
                options.revisions.push(value.to_string());
            }
            value if value.starts_with('-') && value != "-" => {
                return Err(GitError::Command(format!(
                    "unsupported format-patch option {value}"
                )));
            }
            value => options.revisions.push(value.to_string()),
        }
    }
    Ok(options)
}

/// Parse a non-negative integer flag value, with a git-flavored error context.
fn parse_format_patch_number(value: &str, what: &str) -> Result<usize> {
    value
        .parse::<usize>()
        .map_err(|_| GitError::Command(format!("invalid {what} value '{value}'")))
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
