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
    /// is woven into cover-letter/output naming (only the prefix part is wired).
    reroll_count: Option<String>,
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
    /// Revision setup arguments (single committish, ranges, `--`, pathspecs).
    setup_args: Vec<String>,
}

struct FormatPatchSelection {
    commits: Vec<sley_rev::CommitRecord>,
    pathspecs: Vec<String>,
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
            subject_prefix: None,
            rfc: RfcMode::Unset,
            reroll_count: None,
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
            count: None,
            numbered_files: false,
            full_index: false,
            abbrev: None,
            detect_renames: true,
            detect_copies: false,
            find_copies_harder: false,
            rename_threshold: sley_diff_merge::DEFAULT_RENAME_THRESHOLD,
            copy_threshold: sley_diff_merge::DEFAULT_RENAME_THRESHOLD,
            setup_args: Vec::new(),
        }
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
    let options = parse_format_patch_args(args)?;

    let repo = RepositoryContext::discover_current()?;
    let cwd = repo.cwd();
    let git_dir = repo.git_dir();
    let format = repo.format();
    let config = repo.config();
    let db = repo.objects();

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
                resolved: &resolved,
                record,
                diff_pathspec: diff_pathspec.as_ref(),
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
            resolved: &resolved,
            record,
            diff_pathspec: diff_pathspec.as_ref(),
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

/// Fold the parsed options together with repository config into the run-wide
/// [`ResolvedFormat`]: the subject prefix, the To/Cc/extra-header block, the
/// `--from` rewrite identity, and the signature trailer text. This mirrors
/// git's `cmd_format_patch` set-up phase (builtin/log.c), which assembles these
/// once before walking the commits.
fn resolve_format(options: &FormatPatchOptions, config: &GitConfig) -> Result<ResolvedFormat> {
    let prefix_body = resolve_prefix_body(options, config);
    let header_block = resolve_header_block(options, config);
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
fn resolve_header_block(options: &FormatPatchOptions, config: &GitConfig) -> Vec<u8> {
    let mut headers: Vec<String> = Vec::new();
    let mut to: Vec<String> = Vec::new();
    let mut cc: Vec<String> = Vec::new();

    if !options.no_add_header {
        // format.headers entries route through add_header (To:/Cc: prefixes go
        // to the recipient lists; everything else is a raw header line).
        for value in config.get_all("format", None, "headers") {
            if let Some(value) = value {
                route_config_header(value, &mut headers, &mut to, &mut cc);
            }
        }
    }
    if !options.no_to {
        for value in config.get_all("format", None, "to") {
            if let Some(value) = value {
                to.push(value.to_string());
            }
        }
    }
    if !options.no_cc {
        for value in config.get_all("format", None, "cc") {
            if let Some(value) = value {
                cc.push(value.to_string());
            }
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
    write_recipient_block(&mut out, "To: ", &to);
    write_recipient_block(&mut out, "Cc: ", &cc);
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
fn write_recipient_block(out: &mut Vec<u8>, label: &str, recipients: &[String]) {
    if recipients.is_empty() {
        return;
    }
    out.extend_from_slice(label.as_bytes());
    for (idx, recipient) in recipients.iter().enumerate() {
        if idx > 0 {
            out.extend_from_slice(b"    ");
        }
        out.extend_from_slice(recipient.as_bytes());
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
fn resolve_signature(
    options: &FormatPatchOptions,
    config: &GitConfig,
) -> Result<Option<Vec<u8>>> {
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
    let file = options
        .signature_file
        .clone()
        .or_else(|| {
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

    // From: header. With `--from`/`format.from` the visible From: is the rewrite
    // identity and the real author moves to an in-body `From:`; otherwise the
    // author identity is used directly.
    let (author_name, author_email) = commit_identity_name_email(&commit.author);
    let in_body_from = match &resolved.from_ident {
        Some(from) => {
            out.extend_from_slice(
                format!("From: {} <{}>\n", from.name, from.email).as_bytes(),
            );
            // git keeps the in-body From: only when it differs from the header
            // From: (i.e. the author differs from the rewrite ident), unless
            // --force-in-body-from is set.
            let redundant = from.name == author_name && from.email == author_email;
            (!redundant || resolved.force_in_body_from)
                .then(|| format!("From: {author_name} <{author_email}>\n"))
        }
        None => {
            out.extend_from_slice(
                format!("From: {author_name} <{author_email}>\n").as_bytes(),
            );
            None
        }
    };

    // Date: header — the *author* date in git's RFC 2822 rendering.
    let date = commit_identity_date(&commit.author, &DateMode::Rfc2822);
    out.extend_from_slice(format!("Date: {date}\n").as_bytes());

    // Subject: [PREFIX n/m] <subject> (or, with -k/--keep-subject, the bare
    // subject), folded to <=78 columns.
    let prefix = if options.keep_subject {
        None
    } else {
        subject_prefix_label(resolved, seq, last_number, numbered)
    };
    let subject = commit_subject(&commit.message);
    write_folded_subject(&mut out, prefix.as_deref(), &subject);
    let subject_bytes = subject.clone().into_bytes();

    // Extra headers (custom `--add-header`/`format.headers`, then `To:`, then
    // `Cc:`) are emitted directly after the Subject, before the blank line.
    out.extend_from_slice(&resolved.header_block);

    // Blank line, then optional in-body `From:` header (with its own trailing
    // blank line), then the commit body (message minus the subject line),
    // normalized to end in exactly one newline. With --signoff the trailer is
    // appended to the body.
    out.push(b'\n');
    if let Some(in_body) = in_body_from {
        out.extend_from_slice(in_body.as_bytes());
        out.push(b'\n');
    }
    let body = format_patch_body(&commit.message, &subject_bytes, signoff_line);
    out.extend_from_slice(&body);

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
    // the `-- ` separator follows directly (no intervening blank line). When a
    // signature is present every patch ends `-- \n<sig>\n\n` (the trailing blank
    // is the inter-patch separator on stdout / the file's final newline). A
    // suppressed signature (`--no-signature`, `--signature=""`, empty
    // `format.signature`) drops the whole `-- \n...` block *and* the trailing
    // blank line: git emits nothing past the diff's own final newline.
    if let Some(signature) = &resolved.signature {
        out.extend_from_slice(b"-- \n");
        out.extend_from_slice(signature);
        out.extend_from_slice(b"\n\n");
    }
    Ok(out)
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
        // body needs "\n\n" (title room), a single "\n" needs one more, and a
        // body ending in a single "\n" gets a blank line; a body already ending
        // in "\n\n" needs nothing.
        if body.is_empty() {
            body.extend_from_slice(b"\n\n");
        } else if body.len() == 1 {
            body.push(b'\n');
        } else if body[body.len() - 2] != b'\n' {
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
    Ok(match diff_pathspec {
        Some(pathspec) => apply_diff_pathspec(entries, pathspec),
        None => entries,
    })
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
    for oid in revision_options.negatives {
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
            },
        )?;
    }

    // `-<n>` keeps the n newest of those before reversing to oldest-first.
    if let Some(count) = options.count {
        selected.truncate(count);
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
    let args = &options.setup_args;
    let dashdash = args.iter().position(|arg| arg == "--");
    let rev_end = dashdash.unwrap_or(args.len());
    if rev_end != 1 {
        return None;
    }
    let rev = args[0].as_str();
    (!rev.starts_with('^') && !rev.contains("..")).then_some(rev)
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

/// Options for [`write_patch_hunks_with`]: hunk shaping and heading lookup.
///
/// This is the sley-cli-side option bundle; it carries the repository-coupled
/// concerns (userdiff funcname driver, sley-cli `DiffColors`, word-diff
/// config) and is translated into the engine's
/// [`sley_diff_merge::render::HunkRenderOptions`] by [`write_patch_hunks_with`].
pub(crate) struct PatchHunkOptions<'a> {
    /// Lines of context around each change (`-U<n>`, default 3).
    pub(crate) context: usize,
    /// Extra inter-hunk merging distance (`--inter-hunk-context`).
    pub(crate) interhunk: usize,
    /// Compiled userdiff funcname patterns for the path; `None` selects the
    /// default `def_ff` heuristic.
    pub(crate) funcname: Option<&'a commands::userdiff::CompiledFuncname>,
    /// ANSI palette when color output is enabled.
    pub(crate) colors: Option<&'a commands::diff_words::DiffColors>,
    /// Word-diff rendering (replaces the +/- line bodies of each hunk).
    pub(crate) word_diff: Option<&'a commands::diff_words::WordDiffConfig<'a>>,
}

impl Default for PatchHunkOptions<'_> {
    fn default() -> Self {
        Self {
            context: HUNK_CONTEXT,
            interhunk: 0,
            funcname: None,
            colors: None,
            word_diff: None,
        }
    }
}

/// Map a sley-cli [`DiffColors`](commands::diff_words::DiffColors) palette into
/// the engine's [`RenderColors`](sley_diff_merge::render::RenderColors) borrow.
pub(crate) fn render_colors(
    colors: &commands::diff_words::DiffColors,
) -> sley_diff_merge::render::RenderColors<'_> {
    sley_diff_merge::render::RenderColors {
        frag: &colors.frag,
        func: &colors.func,
        old: &colors.old,
        new: &colors.new,
        context: &colors.context,
        reset: &colors.reset,
    }
}

/// Bridge a sley-cli word-diff config + its line buffers into the engine's
/// [`HunkWordDiff`](sley_diff_merge::render::HunkWordDiff) hook. The engine
/// owns hunk shaping; this adapter owns the word-level rendering.
pub(crate) struct WordDiffAdapter<'a> {
    config: &'a commands::diff_words::WordDiffConfig<'a>,
    buffers: commands::diff_words::WordDiffBuffers,
}

impl<'a> WordDiffAdapter<'a> {
    pub(crate) fn new(config: &'a commands::diff_words::WordDiffConfig<'a>) -> Self {
        Self {
            config,
            buffers: commands::diff_words::WordDiffBuffers::new(),
        }
    }
}

impl sley_diff_merge::render::HunkWordDiff for WordDiffAdapter<'_> {
    fn push_minus(&mut self, content: &[u8]) {
        self.buffers.push_minus(content);
    }

    fn push_plus(&mut self, content: &[u8]) {
        self.buffers.push_plus(content);
    }

    fn flush(&mut self, out: &mut Vec<u8>) {
        self.buffers.flush(out, self.config);
    }

    fn emit_context_line(&mut self, out: &mut Vec<u8>, content: &[u8]) {
        commands::diff_words::WordDiffBuffers::emit_context_line(out, self.config, content);
    }
}

/// A per-line section-heading classifier matching git's funcname resolution:
/// a userdiff `xfuncname` pattern when a driver is present, else the default
/// `def_ff` heuristic. Returned as a closure for the engine's
/// [`HeadingFn`](sley_diff_merge::render::HeadingFn) seam.
pub(crate) fn heading_classifier<'a>(
    funcname: Option<&'a commands::userdiff::CompiledFuncname>,
) -> impl FnMut(&[u8]) -> Option<Vec<u8>> + 'a {
    move |line: &[u8]| match funcname {
        Some(funcname) => funcname.match_line(line),
        None => commands::userdiff::default_funcname_heading(line),
    }
}

/// Emit the unified-diff hunks for a single file change into `out`, grouping
/// changes with [`HUNK_CONTEXT`] lines of surrounding context (merging nearby
/// groups), and prefixing each `@@` header with git's default section heading.
pub(crate) fn write_patch_hunks(
    out: &mut Vec<u8>,
    old_content: Option<&[u8]>,
    new_content: Option<&[u8]>,
) {
    write_patch_hunks_with(out, old_content, new_content, &PatchHunkOptions::default());
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
    options: &PatchHunkOptions<'_>,
) {
    let mut heading = heading_classifier(options.funcname);
    let mut word_diff = options.word_diff.map(WordDiffAdapter::new);
    let mut render_options = sley_diff_merge::render::HunkRenderOptions {
        context: options.context,
        interhunk: options.interhunk,
        heading: Some(&mut heading),
        colors: options.colors.map(render_colors),
        word_diff: word_diff
            .as_mut()
            .map(|adapter| adapter as &mut dyn sley_diff_merge::render::HunkWordDiff),
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
            options.setup_args.push(arg.clone());
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
            "-k" | "--keep-subject" => options.keep_subject = true,
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
            | "--ita-invisible-in-index" => {}
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
                options.setup_args.push(value.to_string());
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
