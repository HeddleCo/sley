//! Extracted from the crate root (sley#8 phase 1) — code motion only.

// A glob of the crate root brings every shared helper/type into scope via
// descendant-privacy; see commands::stash for the rationale.
use crate::*;
use sley_notes::{NotesRef, read_note_bytes};

/// Tracks `git log`'s notes-display state (`--notes`, `--show-notes[=ref]`,
/// `--no-notes`, `--standard-notes`, `--no-standard-notes`), mirroring git's
/// `display_notes_opt` / `show_notes` resolution.
#[derive(Default, Clone)]
struct NotesDisplay {
    /// Whether any notes flag was given (git's `show_notes_given`).
    given: bool,
    /// Whether notes display is currently enabled (git's `show_notes`).
    enabled: bool,
    /// Tri-state `use_default_notes`: None = unset (-1), Some(true) = forced on,
    /// Some(false) = standard refs suppressed.
    use_default: Option<bool>,
    /// Extra refs from `--notes=<ref>` / `--show-notes=<ref>`, expanded.
    extra_refs: Vec<String>,
}

impl NotesDisplay {
    /// `--notes` / `--show-notes`: enable display using the standard refs.
    fn add_default(&mut self) {
        self.use_default = Some(true);
        self.enabled = true;
        self.given = true;
    }
    /// `--notes=<ref>`: add a specific ref without forcing the standard refs on
    /// (only `--show-notes=<ref>` re-enables the defaults).
    fn add_ref(&mut self, reff: &str) {
        self.extra_refs
            .push(NotesRef::expand(reff).as_str().to_string());
        self.enabled = true;
        self.given = true;
    }
    /// `--show-notes=<ref>`: like `add_ref`, but additionally turns the standard
    /// refs back on when they were unset (matches git's `--show-notes=` path).
    fn add_show_ref(&mut self, reff: &str) {
        if self.use_default.is_none() {
            self.use_default = Some(true);
        }
        self.add_ref(reff);
    }
    /// `--no-notes`: clear all display state and turn notes off.
    fn disable(&mut self) {
        self.use_default = Some(false);
        self.extra_refs.clear();
        self.enabled = false;
        self.given = true;
    }
    /// `--no-standard-notes`: suppress the standard refs but keep any extra refs
    /// (does not by itself disable display).
    fn no_standard(&mut self) {
        self.use_default = Some(false);
        self.given = true;
    }
    /// `--standard-notes`: re-enable the standard refs (keeps extra refs).
    fn add_standard(&mut self) {
        self.use_default = Some(true);
        self.given = true;
    }

    /// Resolve whether notes display is active. When no flag was given, notes
    /// show only for the default (no-`--pretty`) format. When a flag was given,
    /// the explicit `enabled` state wins.
    fn is_active(&self, default_format: bool) -> bool {
        if self.given {
            self.enabled
        } else {
            default_format
        }
    }

    /// Compute the ordered, de-duplicated list of notes refs to display,
    /// mirroring git's `load_display_notes`: the standard refs (default notes
    /// ref + `GIT_NOTES_DISPLAY_REF` env or `notes.displayRef` config, glob
    /// expanded) come first when `use_default` is set or unset-with-no-extras,
    /// then the `--notes=<ref>` extras (glob expanded). A `notes.displayRef`
    /// with no value is a fatal error.
    fn resolve_refs(&self, git_dir: &Path, store: &FileRefStore) -> Result<Vec<String>> {
        let mut refs: Vec<String> = Vec::new();
        let load_standard = matches!(self.use_default, Some(true))
            || (self.use_default.is_none() && self.extra_refs.is_empty());
        if load_standard {
            // git's default_notes_ref takes GIT_NOTES_REF verbatim when set —
            // even when empty, which yields a no-op (no default note shown).
            let default_ref = match env::var("GIT_NOTES_REF") {
                Ok(value) => value,
                Err(_) => crate::commands::notes::raw_notes_ref(git_dir, None),
            };
            if !default_ref.is_empty() {
                push_unique(&mut refs, default_ref);
            }
            // A command-line `-c notes.displayRef` with no value is a parse
            // error (the key is a string, not a bool). Detect the bool-true
            // marker the `-c key` form injects and reject it, as git does.
            if matches!(global_config_value("notes.displayRef"), Ok(Some(v)) if v == "true") {
                eprintln!("error: missing value for 'notes.displayref'");
                eprintln!("fatal: unable to parse 'notes.displayref' from command-line config");
                return Err(GitError::Exit(128));
            }
            if let Ok(env_value) = env::var("GIT_NOTES_DISPLAY_REF") {
                for part in env_value.split(':').filter(|s| !s.is_empty()) {
                    for expanded in expand_notes_glob(store, part)? {
                        push_unique(&mut refs, expanded);
                    }
                }
            } else if let Ok(config) = read_repo_config(git_dir) {
                for value in config
                    .get_all("notes", None, "displayRef")
                    .into_iter()
                    .flatten()
                {
                    if value.is_empty() {
                        eprintln!(
                            "fatal: unable to parse 'notes.displayref' from command-line config"
                        );
                        return Err(GitError::Exit(128));
                    }
                    for expanded in expand_notes_glob(store, value)? {
                        push_unique(&mut refs, expanded);
                    }
                }
            }
        }
        for extra in &self.extra_refs {
            for expanded in expand_notes_glob(store, extra)? {
                push_unique(&mut refs, expanded);
            }
        }
        Ok(refs)
    }
}

/// Push `value` to `refs` only if it is not already present (preserve order).
fn push_unique(refs: &mut Vec<String>, value: String) {
    if !refs.contains(&value) {
        refs.push(value);
    }
}

/// Expand a single notes-ref spec: a `*`-containing glob matches existing refs
/// by prefix (ref-name sorted); an exact ref is returned as-is.
fn expand_notes_glob(store: &FileRefStore, glob: &str) -> Result<Vec<String>> {
    if !glob.contains('*') {
        return Ok(vec![glob.to_string()]);
    }
    let prefix = glob.trim_end_matches('*');
    let mut matched: Vec<String> = store
        .list_refs()?
        .into_iter()
        .map(|entry| entry.name)
        .filter(|name| name.starts_with(prefix))
        .collect();
    matched.sort();
    Ok(matched)
}

/// Resolve the standard notes display refs and render the notes block for
/// `oid`, for callers (e.g. `git show`) that always use the default display set.
/// Returns the bytes to append after the commit message (empty when none).
pub(crate) fn render_standard_notes(
    git_dir: &Path,
    format: ObjectFormat,
    oid: &ObjectId,
) -> Result<Vec<u8>> {
    let store = FileRefStore::new(git_dir, format);
    let display = NotesDisplay {
        use_default: Some(true),
        ..NotesDisplay::default()
    };
    let refs = display.resolve_refs(git_dir, &store)?;
    render_notes_block(git_dir, format, &store, &refs, oid)
}

/// Render the `Notes:` / `Notes (<name>):` block(s) for `oid` across the
/// resolved display refs, matching git's `format_note`: a leading blank line,
/// the label, then each note line indented by four spaces. Returns the bytes to
/// append after the commit message (empty when no notes exist).
fn render_notes_block(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    display_refs: &[String],
    oid: &ObjectId,
) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for reff in display_refs {
        let handle = NotesRef::expand(reff);
        let Some(mut body) = read_note_bytes(git_dir, format, store, &handle, oid)? else {
            continue;
        };
        // git drops a single trailing newline before indenting.
        if body.last() == Some(&b'\n') {
            body.pop();
        }
        // Label: bare `Notes:` only for the literal default ref.
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
        // An empty note prints just the label (git's loop runs over zero bytes).
        if !body.is_empty() {
            for line in body.split(|b| *b == b'\n') {
                out.extend_from_slice(b"    ");
                out.extend_from_slice(line);
                out.push(b'\n');
            }
        }
    }
    Ok(out)
}

pub(crate) fn cmd_log(args: &[String]) -> Result<()> {
    cmd_log_impl(args, false)
}

/// `git whatchanged --i-still-use-this`: log with raw diff output by default
/// and `always_show_header = 0` semantics (commits whose diff comes out empty
/// — e.g. merges — are omitted entirely).
pub(crate) fn cmd_whatchanged(args: &[String]) -> Result<()> {
    let mut acknowledged = false;
    let mut filtered = Vec::with_capacity(args.len());
    for arg in args {
        if arg == "--i-still-use-this" {
            acknowledged = true;
        } else {
            filtered.push(arg.clone());
        }
    }
    if !acknowledged {
        eprintln!(
            "fatal: git whatchanged is nominated for removal.\nIf you still use this command, add an extra option, '--i-still-use-this',\non the command line and let us know you still use it by sending an e-mail\nto <git@vger.kernel.org>.  Thanks."
        );
        return Err(GitError::Exit(128));
    }
    cmd_log_impl(&filtered, true)
}

fn log_limited_commit_format_supported(compiled: &CompiledLogFormat) -> bool {
    !compiled.tokens.is_empty()
        && compiled.uses_oid()
        && !compiled.uses_decorations()
        && !compiled.uses_source()
        && compiled.tokens.iter().all(|token| {
            matches!(
                token,
                FormatToken::Literal(_)
                    | FormatToken::Percent
                    | FormatToken::OidFull
                    | FormatToken::OidAbbrev
                    | FormatToken::ParentsFull
                    | FormatToken::ParentsAbbrev
                    | FormatToken::Marker
                    | FormatToken::Subject
                    | FormatToken::SanitizedSubject
                    | FormatToken::NoteName
                    | FormatToken::ColorParen
                    | FormatToken::ColorName(_)
                    | FormatToken::Newline
                    | FormatToken::HexByte(_)
            )
        })
}

fn cmd_log_impl(args: &[String], whatchanged: bool) -> Result<()> {
    let mut setup_args = Vec::new();
    let mut setup_not = false;
    let mut default_revision_given = false;
    let mut output = LogOutput::Default(LogDefaultKind::Medium);
    let mut notes_display = NotesDisplay::default();
    let mut preset_oneline: Option<bool> = None;
    // Raw `--pretty=`/`--format=` spec captured during arg parse and resolved
    // after config is loaded (aliases live in `pretty.<name>`). The bool is the
    // "format kind" flag: `--format=`/`tformat:` terminate each entry with a
    // newline; `--pretty=format:` separates entries instead.
    let mut pretty_spec: Option<(String, bool)> = None;
    let mut walk_reflogs = false;
    let mut min_parents = None;
    let mut max_parents = None;
    let mut show_parents = false;
    let mut show_children = false;
    let mut abbrev_commit = false;
    let mut abbrev_len = Some(7usize);
    let mut abbrev_len_explicit = false;
    let mut decoration = LogDecorationMode::Off;
    let mut read_stdin = false;
    let mut author_patterns = Vec::new();
    let mut committer_patterns = Vec::new();
    let mut grep_patterns = Vec::new();
    let mut grep_all_match = false;
    let mut invert_grep = false;
    let mut regexp_ignore_case = false;
    let mut pattern_kind = crate::grep_source::PatternKind::Basic;
    let mut date_mode = DateMode::Default;
    let mut date_explicit = false;
    // `-z` / `--null`: separate/terminate compiled-format entries with NUL
    // instead of newline.
    let mut null_terminate = false;
    let mut graph = false;
    // Diff-output options (`-p`, `--stat`, ...): rendered per commit against
    // its first parent, mirroring git's log diff machinery.
    let mut diff_opts = LogDiffOptions::default();
    // `--root` flag; falls back to the log.showRoot config (default true).
    let mut show_root_flag: Option<bool> = None;
    let mut line_prefix: Option<String> = None;
    let mut color_always = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--" => {
                setup_args.push(arg.clone());
                setup_args.extend(iter.cloned());
                break;
            }
            "--not" => {
                setup_not = !setup_not;
                setup_args.push(arg.clone());
            }
            "--stdin" => read_stdin = true,
            "--default" => {
                default_revision_given = true;
                setup_args.push(arg.clone());
                setup_args.push(
                    iter.next()
                        .ok_or_else(|| GitError::Command("--default requires a value".into()))?
                        .clone(),
                );
            }
            "--full-history" | "--sparse" | "--dense" | "--remove-empty"
            | "--simplify-merges" | "--show-pulls" | "--reverse" | "--topo-order"
            | "--date-order" | "--author-date-order" | "--first-parent" | "--no-walk"
            | "--no-walk=sorted" | "--no-walk=unsorted" | "--do-walk" | "--all"
            | "--branches" | "--tags" | "--remotes" => setup_args.push(arg.clone()),
            "--parents" => show_parents = true,
            "--children" => show_children = true,
            "--abbrev-commit" => abbrev_commit = true,
            "--no-abbrev-commit" => abbrev_commit = false,
            "--abbrev" => {
                abbrev_len = Some(7);
                abbrev_len_explicit = true;
            }
            "--no-abbrev" => {
                abbrev_len = None;
                abbrev_len_explicit = true;
            }
            "--glob" | "--exclude" | "--exclude-hidden" => {
                setup_args.push(arg.clone());
                setup_args.push(
                    iter.next()
                        .ok_or_else(|| GitError::Command(format!("{arg} requires a value")))?
                        .clone(),
                );
            }
            value
                if value.starts_with("--glob=")
                    || value.starts_with("--exclude=")
                    || value.starts_with("--exclude-hidden=")
                    || value.starts_with("--branches=")
                    || value.starts_with("--tags=")
                    || value.starts_with("--remotes=") =>
            {
                setup_args.push(arg.clone());
            }
            "--author" => {
                let value = iter.next().ok_or_else(log_author_requires_value_error)?;
                author_patterns.push(value.to_string());
            }
            value if value.starts_with("--author=") => {
                author_patterns.push(value["--author=".len()..].to_string());
            }
            "--committer" => {
                let value = iter.next().ok_or_else(log_committer_requires_value_error)?;
                committer_patterns.push(value.to_string());
            }
            value if value.starts_with("--committer=") => {
                committer_patterns.push(value["--committer=".len()..].to_string());
            }
            "--grep" => {
                let value = iter.next().ok_or_else(log_grep_requires_value_error)?;
                grep_patterns.push(value.to_string());
            }
            value if value.starts_with("--grep=") => {
                grep_patterns.push(value["--grep=".len()..].to_string());
            }
            "--all-match" => grep_all_match = true,
            "--invert-grep" => invert_grep = true,
            "-i" | "--regexp-ignore-case" => regexp_ignore_case = true,
            "-F" | "--fixed-strings" => pattern_kind = crate::grep_source::PatternKind::Fixed,
            "--basic-regexp" => pattern_kind = crate::grep_source::PatternKind::Basic,
            "-E" | "--extended-regexp" => pattern_kind = crate::grep_source::PatternKind::Extended,
            "-P" | "--perl-regexp" => pattern_kind = crate::grep_source::PatternKind::Perl,
            "-g" | "--walk-reflogs" => walk_reflogs = true,
            "--no-walk-reflogs" => walk_reflogs = false,
            "--max-age" => {
                setup_args.push(arg.clone());
                setup_args.push(iter.next().ok_or_else(log_max_age_requires_value_error)?.clone());
            }
            "--min-age" => {
                setup_args.push(arg.clone());
                setup_args.push(iter.next().ok_or_else(log_min_age_requires_value_error)?.clone());
            }
            "--since" | "--after" | "--until" | "--before" => {
                setup_args.push(arg.clone());
                setup_args.push(
                    iter.next()
                        .ok_or_else(|| log_date_cutoff_requires_value_error(arg))?
                        .clone(),
                );
            }
            value
                if value.starts_with("--max-age=")
                    || value.starts_with("--min-age=")
                    || value.starts_with("--since=")
                    || value.starts_with("--after=")
                    || value.starts_with("--until=")
                    || value.starts_with("--before=") =>
            {
                setup_args.push(arg.clone());
            }
            "--merges" => min_parents = Some(2),
            "--no-merges" => max_parents = Some(1),
            "--no-min-parents" => min_parents = None,
            "--no-max-parents" => max_parents = None,
            "-q"
            | "--quiet"
            | "--no-quiet"
            | "--unpacked"
            | "--no-source"
            | "--use-mailmap"
            | "--no-use-mailmap"
            | "--mailmap"
            | "--no-mailmap"
            | "--show-signature"
            | "--no-show-signature"
            | "--no-decorate"
            | "--decorate=no"
            | "--decorate=auto"
            | "--decorate="
            | "--decorate=false"
            | "--decorate=0"
            | "--decorate=off"
            | "--clear-decorations"
            | "--no-decorate-refs"
            | "--no-decorate-refs-exclude"
            | "--full-diff"
            | "--relative"
            | "--no-relative"
            | "--ext-diff"
            | "--no-ext-diff"
            | "--no-renames"
            | "--find-renames"
            | "--find-copies"
            | "--find-copies-harder"
            | "--no-find-copies-harder"
            | "--minimal"
            | "--patience"
            | "--histogram"
            | "--indent-heuristic"
            | "--no-indent-heuristic"
            | "--ignore-space-at-eol"
            | "--ignore-cr-at-eol"
            | "--ignore-space-change"
            | "--ignore-all-space"
            | "--ignore-blank-lines"
            | "--function-context"
            | "--no-prefix"
            | "--default-prefix"
            | "--full-index"
            | "--break-rewrites"
            | "--irreversible-delete"
            | "--textconv"
            | "--no-textconv"
            | "--submodule"
            | "--ignore-submodules"
            | "--color-moved"
            | "--no-color-moved"
            | "--ita-visible-in-index"
            | "--ita-invisible-in-index"
            | "--pickaxe-all"
            | "--pickaxe-regex"
            | "-M"
            | "-C"
            | "-B"
            | "-D"
            | "-b"
            | "-w"
            | "-bw"
            | "-wb"
            | "-W" => {}
            "--decorate" | "--decorate=short" | "--decorate=true" | "--decorate=1"
            | "--decorate=on" | "--decorate=yes" => decoration = LogDecorationMode::Short,
            "--decorate=full" => decoration = LogDecorationMode::Full,
            value if value.starts_with("--decorate=") => {
                return Err(GitError::Command(format!(
                    "invalid --decorate option {value}"
                )));
            }
            value if value.starts_with("-M") => {
                log_validate_similarity_option(&value[2..], "find-renames")?;
            }
            value if value.starts_with("-C") => {
                log_validate_similarity_option(&value[2..], "find-copies")?;
            }
            value if value.starts_with("-B") => {
                log_validate_break_rewrites_option(&value[2..])?;
            }
            value if value.starts_with("--relative=") => {}
            value if value.starts_with("--find-renames=") => {
                log_validate_similarity_option(&value["--find-renames=".len()..], "find-renames")?;
            }
            value if value.starts_with("--find-copies=") => {
                log_validate_similarity_option(&value["--find-copies=".len()..], "find-copies")?;
            }
            "--diff-merges" => {
                let value = iter
                    .next()
                    .ok_or_else(log_diff_merges_requires_value_error)?;
                let mode = log_parse_diff_merges(value)?;
                diff_opts.merges = Some(mode);
                diff_opts.merges_imply_patch = mode != LogDiffMerges::Off;
            }
            value if value.starts_with("--diff-merges=") => {
                let mode = log_parse_diff_merges(&value["--diff-merges=".len()..])?;
                diff_opts.merges = Some(mode);
                diff_opts.merges_imply_patch = mode != LogDiffMerges::Off;
            }
            value if value.starts_with("--no-walk=") => {
                return log_no_walk_invalid_argument(value);
            }
            value if value.starts_with("--min-parents=") => {
                min_parents = Some(log_parse_parent_count(&value["--min-parents=".len()..])?);
            }
            value if value.starts_with("--max-parents=") => {
                max_parents = Some(log_parse_parent_count(&value["--max-parents=".len()..])?);
            }
            value if value.starts_with("--abbrev=") => {
                abbrev_len = Some(log_parse_abbrev_width(&value["--abbrev=".len()..]));
                abbrev_len_explicit = true;
            }
            value if value.starts_with("--unpacked=") => {
                eprintln!("fatal: --unpacked=<packfile> no longer supported");
                return Err(GitError::Exit(128));
            }
            "--min-parents" | "--max-parents" => {
                return log_fatal_unrecognized_argument(arg);
            }
            value
                if value.starts_with("--merges=")
                    || value.starts_with("--no-merges=")
                    || value.starts_with("--no-min-parents=")
                    || value.starts_with("--no-max-parents=")
                    || value.starts_with("--parents=")
                    || value.starts_with("--no-parents=")
                    || value.starts_with("--children=")
                    || value.starts_with("--no-children=")
                    || value.starts_with("--abbrev-commit=")
                    || value.starts_with("--no-abbrev-commit=")
                    || value.starts_with("--topo-order=")
                    || value.starts_with("--date-order=")
                    || value.starts_with("--author-date-order=")
                    || value.starts_with("--sparse=")
                    || value.starts_with("--dense=")
                    || value.starts_with("--remove-empty=")
                    || value.starts_with("--full-history=")
                    || value.starts_with("--simplify-merges=")
                    || value.starts_with("--show-pulls=")
                    || value.starts_with("--all=")
                    || value.starts_with("--no-all=")
                    || value.starts_with("--no-branches=")
                    || value.starts_with("--no-tags=")
                    || value.starts_with("--no-remotes=")
                    || value.starts_with("--no-author=")
                    || value.starts_with("--no-committer=")
                    || value.starts_with("--no-max-age=")
                    || value.starts_with("--no-min-age=")
                    || value.starts_with("--no-since=")
                    || value.starts_with("--no-after=")
                    || value.starts_with("--no-until=")
                    || value.starts_with("--no-before=") =>
            {
                return log_fatal_unrecognized_argument(value);
            }
            "--no-parents" | "--no-children" | "--no-all" | "--no-branches" | "--no-tags"
            | "--no-remotes" | "--no-author" | "--no-committer" | "--no-max-age"
            | "--no-min-age" | "--no-since" | "--no-after" | "--no-until" | "--no-before" => {
                return log_fatal_unrecognized_argument(arg);
            }
            value
                if value == "--no-grep"
                    || value.starts_with("--no-grep=")
                    || value.starts_with("--all-match=")
                    || value.starts_with("--no-all-match")
                    || value.starts_with("--invert-grep=")
                    || value.starts_with("--no-invert-grep")
                    || value.starts_with("--regexp-ignore-case=")
                    || value.starts_with("--no-regexp-ignore-case")
                    || value.starts_with("--fixed-strings=")
                    || value.starts_with("--no-fixed-strings")
                    || value.starts_with("--basic-regexp=")
                    || value.starts_with("--no-basic-regexp")
                    || value.starts_with("--extended-regexp=")
                    || value.starts_with("--no-extended-regexp") =>
            {
                return log_fatal_unrecognized_argument(value);
            }
            "--no-first-parent" => {
                return log_fatal_unrecognized_argument(arg);
            }
            value
                if value.starts_with("--first-parent=")
                    || value.starts_with("--no-first-parent=") =>
            {
                return log_fatal_unrecognized_argument(value);
            }
            "--date" => {
                let value = iter.next().ok_or_else(log_date_requires_value_error)?;
                date_mode = log_date_mode(value)?;
                date_explicit = true;
            }
            value if value.starts_with("--date=") => {
                date_mode = log_date_mode(&value["--date=".len()..])?;
                date_explicit = true;
            }
            "--diff-algorithm" => {
                let value = iter
                    .next()
                    .ok_or_else(|| log_option_requires_value_error("diff-algorithm"))?;
                log_validate_diff_algorithm(value)?;
            }
            value if value.starts_with("--diff-algorithm=") => {
                log_validate_diff_algorithm(&value["--diff-algorithm=".len()..])?;
            }
            "--anchored" => {
                iter.next()
                    .ok_or_else(|| log_option_requires_value_error("anchored"))?;
            }
            value if value.starts_with("--anchored=") => {}
            "--inter-hunk-context" => {
                let value = iter
                    .next()
                    .ok_or_else(|| log_option_requires_value_error("inter-hunk-context"))?;
                log_validate_inter_hunk_context(value)?;
            }
            "--inter-hunk-context=" => {
                return log_inter_hunk_context_requires_number_error();
            }
            value if value.starts_with("--inter-hunk-context=") => {
                log_validate_inter_hunk_context(&value["--inter-hunk-context=".len()..])?;
            }
            "--src-prefix" => {
                iter.next()
                    .ok_or_else(|| log_option_requires_value_error("src-prefix"))?;
            }
            "--dst-prefix" => {
                iter.next()
                    .ok_or_else(|| log_option_requires_value_error("dst-prefix"))?;
            }
            value if value.starts_with("--src-prefix=") => {}
            value if value.starts_with("--dst-prefix=") => {}
            value if value.starts_with("--break-rewrites=") => {
                log_validate_break_rewrites_option(&value["--break-rewrites=".len()..])?;
            }
            value if value.starts_with("--submodule=") => {
                log_validate_submodule_format(&value["--submodule=".len()..])?;
            }
            value if value.starts_with("--ignore-submodules=") => {
                log_validate_ignore_submodules(&value["--ignore-submodules=".len()..])?;
            }
            value if value.starts_with("--color-moved=") => {
                log_validate_color_moved(&value["--color-moved=".len()..])?;
            }
            "--graph" => graph = true,
            "--no-graph" => graph = false,
            "--color" => color_always = true,
            "--no-color" => color_always = false,
            "--line-prefix" => {
                let value = iter
                    .next()
                    .ok_or_else(|| log_option_requires_value_error("line-prefix"))?;
                line_prefix = Some(value.to_string());
            }
            value if value.starts_with("--line-prefix=") => {
                line_prefix = Some(value["--line-prefix=".len()..].to_string());
            }
            value if value.starts_with("--color=") => {
                log_validate_color(&value["--color=".len()..])?;
                color_always = value["--color=".len()..].eq_ignore_ascii_case("always");
            }
            "--color-moved-ws" => {
                let value = iter
                    .next()
                    .ok_or_else(|| log_option_requires_value_error("color-moved-ws"))?;
                log_validate_color_moved_ws(value)?;
            }
            value if value.starts_with("--color-moved-ws=") => {
                log_validate_color_moved_ws(&value["--color-moved-ws=".len()..])?;
            }
            "--ws-error-highlight" => {
                let value = iter
                    .next()
                    .ok_or_else(|| log_option_requires_value_error("ws-error-highlight"))?;
                log_validate_ws_error_highlight(value)?;
            }
            value if value.starts_with("--ws-error-highlight=") => {
                log_validate_ws_error_highlight(&value["--ws-error-highlight=".len()..])?;
            }
            "--output-indicator-new" => {
                let value = iter
                    .next()
                    .ok_or_else(|| log_option_requires_value_error("output-indicator-new"))?;
                log_validate_output_indicator_for_log("output-indicator-new", value)?;
            }
            "--output-indicator-old" => {
                let value = iter
                    .next()
                    .ok_or_else(|| log_option_requires_value_error("output-indicator-old"))?;
                log_validate_output_indicator_for_log("output-indicator-old", value)?;
            }
            "--output-indicator-context" => {
                let value = iter
                    .next()
                    .ok_or_else(|| log_option_requires_value_error("output-indicator-context"))?;
                log_validate_output_indicator_for_log("output-indicator-context", value)?;
            }
            value if value.starts_with("--output-indicator-new=") => {
                log_validate_output_indicator_for_log(
                    "output-indicator-new",
                    &value["--output-indicator-new=".len()..],
                )?;
            }
            value if value.starts_with("--output-indicator-old=") => {
                log_validate_output_indicator_for_log(
                    "output-indicator-old",
                    &value["--output-indicator-old=".len()..],
                )?;
            }
            value if value.starts_with("--output-indicator-context=") => {
                log_validate_output_indicator_for_log(
                    "output-indicator-context",
                    &value["--output-indicator-context=".len()..],
                )?;
            }
            value if value.starts_with("--no-renames=") => {
                return log_option_takes_no_value_error("no-renames");
            }
            value if value.starts_with("--no-patch=") => {
                return log_option_takes_no_value_error("no-patch");
            }
            value if value.starts_with("--no-diff-merges=") => {
                return log_fatal_unrecognized_argument(value);
            }
            value if value.starts_with("--no-prefix=") => {
                return log_option_takes_no_value_error("no-prefix");
            }
            value if value.starts_with("--default-prefix=") => {
                return log_option_takes_no_value_error("default-prefix");
            }
            value if value.starts_with("--full-index=") => {
                return log_option_takes_no_value_error("full-index");
            }
            value if value.starts_with("--no-abbrev=") => {
                return log_option_takes_no_value_error("no-abbrev");
            }
            value if value.starts_with("--irreversible-delete=") => {
                return log_option_takes_no_value_error("irreversible-delete");
            }
            value if value.starts_with("--textconv=") => {
                return log_option_takes_no_value_error("textconv");
            }
            value if value.starts_with("--no-textconv=") => {
                return log_option_takes_no_value_error("no-textconv");
            }
            value if value.starts_with("--no-color-moved=") => {
                return log_option_takes_no_value_error("no-color-moved");
            }
            value if value.starts_with("--no-color=") => {
                return log_option_takes_no_value_error("no-color");
            }
            value if value.starts_with("--ita-visible-in-index=") => {
                return log_option_takes_no_value_error("ita-visible-in-index");
            }
            value if value.starts_with("--ita-invisible-in-index=") => {
                return log_option_takes_no_value_error("ita-invisible-in-index");
            }
            value if value.starts_with("--pickaxe-all=") => {
                return log_option_takes_no_value_error("pickaxe-all");
            }
            value if value.starts_with("--pickaxe-regex=") => {
                return log_option_takes_no_value_error("pickaxe-regex");
            }
            value if value.starts_with("--find-copies-harder=") => {
                return log_option_takes_no_value_error("find-copies-harder");
            }
            value if value.starts_with("--no-find-copies-harder=") => {
                return log_option_takes_no_value_error("no-find-copies-harder");
            }
            value if value.starts_with("--indent-heuristic=") => {
                return log_option_takes_no_value_error("indent-heuristic");
            }
            value if value.starts_with("--no-indent-heuristic=") => {
                return log_option_takes_no_value_error("no-indent-heuristic");
            }
            value if value.starts_with("--ignore-space-at-eol=") => {
                return log_option_takes_no_value_error("ignore-space-at-eol");
            }
            value if value.starts_with("--ignore-cr-at-eol=") => {
                return log_option_takes_no_value_error("ignore-cr-at-eol");
            }
            value if value.starts_with("--ignore-space-change=") => {
                return log_option_takes_no_value_error("ignore-space-change");
            }
            value if value.starts_with("--ignore-all-space=") => {
                return log_option_takes_no_value_error("ignore-all-space");
            }
            value if value.starts_with("--ignore-blank-lines=") => {
                return log_option_takes_no_value_error("ignore-blank-lines");
            }
            value if value.starts_with("--function-context=") => {
                return log_option_takes_no_value_error("function-context");
            }
            value if value.starts_with("--no-relative=") => {
                return log_option_takes_no_value_error("no-relative");
            }
            value if value.starts_with("--ext-diff=") => {
                return log_option_takes_no_value_error("ext-diff");
            }
            value if value.starts_with("--no-ext-diff=") => {
                return log_option_takes_no_value_error("no-ext-diff");
            }
            value if value.starts_with("--clear-decorations=") => {
                return log_option_takes_no_value_error("clear-decorations");
            }
            value if value.starts_with("--no-decorate-refs=") => {
                return log_option_takes_no_value_error("no-decorate-refs");
            }
            value if value.starts_with("--no-decorate-refs-exclude=") => {
                return log_option_takes_no_value_error("no-decorate-refs-exclude");
            }
            value if value.starts_with("--do-walk=") => {
                return log_fatal_unrecognized_argument(value);
            }
            "--decorate-refs" => {
                iter.next()
                    .ok_or_else(|| log_option_requires_value_error("decorate-refs"))?;
            }
            "--decorate-refs-exclude" => {
                iter.next()
                    .ok_or_else(|| log_option_requires_value_error("decorate-refs-exclude"))?;
            }
            value if value.starts_with("--decorate-refs=") => {}
            value if value.starts_with("--decorate-refs-exclude=") => {}
            value if value.starts_with("--use-mailmap=") => {
                return log_option_takes_no_value_error("use-mailmap");
            }
            value if value.starts_with("--no-use-mailmap=") => {
                return log_option_takes_no_value_error("no-use-mailmap");
            }
            value if value.starts_with("--mailmap=") => {
                return log_option_takes_no_value_error("mailmap");
            }
            value if value.starts_with("--no-mailmap=") => {
                return log_option_takes_no_value_error("no-mailmap");
            }
            value if value.starts_with("--encoding=") => {}
            "--notes" | "--show-notes" => notes_display.add_default(),
            value if value.starts_with("--notes=") => {
                notes_display.add_ref(&value["--notes=".len()..]);
            }
            value if value.starts_with("--show-notes=") => {
                notes_display.add_show_ref(&value["--show-notes=".len()..]);
            }
            "--no-notes" => notes_display.disable(),
            "--no-standard-notes" => notes_display.no_standard(),
            "--standard-notes" => notes_display.add_standard(),
            value if value.starts_with("--no-notes=") => {
                return log_fatal_unrecognized_argument(value);
            }
            value if value.starts_with("--no-show-signature=") => {
                return log_fatal_unrecognized_argument(value);
            }
            "-z" | "--null" => null_terminate = true,
            "--no-null" => null_terminate = false,
            "--oneline" => {
                preset_oneline = Some(false);
                pretty_spec = None;
            }
            // Built-in `short`/`medium` map to the default-output kinds (short
            // omits the `Date:` line); other named/custom formats fall through
            // to the compiled `pretty_spec` path below.
            "--pretty=short" | "--format=short" => {
                output = LogOutput::Default(LogDefaultKind::Short);
                pretty_spec = None;
                preset_oneline = None;
            }
            "--pretty=medium" | "--format=medium" => {
                output = LogOutput::Default(LogDefaultKind::Medium);
                pretty_spec = None;
                preset_oneline = None;
            }
            "--pretty" | "--format" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command(format!("{arg} requires a value")))?;
                pretty_spec = Some((value.to_string(), arg == "--format"));
                preset_oneline = None;
            }
            value if value.starts_with("--pretty=") => {
                pretty_spec = Some((value["--pretty=".len()..].to_string(), false));
                preset_oneline = None;
            }
            "-n" | "--max-count" => {
                setup_args.push(arg.clone());
                setup_args.push(
                    iter.next()
                        .ok_or_else(|| GitError::Command(format!("{arg} requires a value")))?
                        .clone(),
                );
            }
            value if value.starts_with("--max-count=") => {
                setup_args.push(arg.clone());
            }
            "--skip" => {
                setup_args.push(arg.clone());
                setup_args.push(
                    iter.next()
                        .ok_or_else(|| GitError::Command("--skip requires a value".into()))?
                        .clone(),
                );
            }
            value if value.starts_with("--skip=") => {
                setup_args.push(arg.clone());
            }
            value if value.starts_with("--format=") => {
                pretty_spec = Some((value["--format=".len()..].to_string(), true));
                preset_oneline = None;
            }
            value if value.starts_with("-n") && value.len() > 2 => {
                setup_args.push(arg.clone());
            }
            value
                if value.starts_with('-')
                    && value[1..].bytes().all(|byte| byte.is_ascii_digit()) =>
            {
                setup_args.push(arg.clone());
            }
            "-p" | "-u" | "--patch" => diff_opts.patch = true,
            "-s" | "--no-patch" => diff_opts = LogDiffOptions::default(),
            "--stat" => diff_opts.stat = true,
            value
                if value.starts_with("--stat=")
                    || value.starts_with("--stat-width=")
                    || value.starts_with("--stat-name-width=")
                    || value.starts_with("--stat-graph-width=")
                    || value.starts_with("--stat-count=") =>
            {
                diff_opts.stat = true;
                diff_stat_parse_width_option(value, &mut diff_opts.stat_widths)?;
                if let Some(count) = diff_stat_count_option(value)? {
                    diff_opts.stat_count = count;
                }
            }
            "--compact-summary" => diff_opts.compact_summary = true,
            "--numstat" => diff_opts.numstat = true,
            "--shortstat" => diff_opts.shortstat = true,
            "--summary" => diff_opts.summary = true,
            "--patch-with-stat" => {
                diff_opts.patch = true;
                diff_opts.stat = true;
            }
            "--patch-with-raw" => {
                diff_opts.patch = true;
                diff_opts.raw = true;
            }
            "--raw" => diff_opts.raw = true,
            "-m" => diff_opts.merges = Some(LogDiffMerges::FirstParent),
            "--no-diff-merges" => diff_opts.merges = Some(LogDiffMerges::Off),
            "--root" => show_root_flag = Some(true),
            value if value.starts_with('-') => {
                return Err(GitError::Command(format!("unsupported log option {value}")));
            }
            value => setup_args.push(value.to_string()),
        }
    }
    if read_stdin {
        let mut input = String::new();
        io::stdin().read_to_string(&mut input)?;
        if setup_not {
            setup_args.push("--not".to_string());
        }
        setup_args.extend(
            input
                .lines()
                .filter(|line| !line.is_empty())
                .map(str::to_string),
        );
    }
    if show_parents && show_children {
        eprintln!("fatal: options '--parents' and '--children' cannot be used together");
        return Err(GitError::Exit(128));
    }
    if whatchanged && !diff_opts.any() {
        diff_opts.raw = true;
    }
    if !default_revision_given {
        setup_args.splice(0..0, ["--default".to_string(), "HEAD".to_string()]);
    }
    let git_dir = discover_git_dir(env::current_dir()?)?;
    let format = repository_object_format(&git_dir)?;
    let config = read_repo_config(&git_dir)?;
    let db = FileObjectDatabase::from_git_dir(&git_dir, format);
    let output_encoding = log_output_encoding(&config);
    let cwd = env::current_dir()?;
    let worktree_root = worktree_root_for_git_dir(&git_dir).ok();
    let setup = sley_rev::setup_revisions(
        &setup_args,
        &sley_rev::RevisionSetupContext {
            git_dir: &git_dir,
            worktree_root: worktree_root.as_deref(),
            cwd: &cwd,
            format,
            reader: &db,
            config: Some(&config),
        },
    )?;
    if let Some(leftover) = setup.leftovers.first() {
        return Err(GitError::Command(format!(
            "unsupported log option {leftover}"
        )));
    }
    let revision_options = setup.options;
    let max_count = revision_options.max_count;
    let skip = revision_options.skip;
    let max_age = revision_options.date_window.min_time;
    let min_age = revision_options.date_window.max_time;
    let reverse = revision_options.reverse;
    let ordering = match revision_options.order {
        sley_rev::RevisionOrder::Default => RevListOrdering::Default,
        sley_rev::RevisionOrder::Topo => RevListOrdering::Topo,
        sley_rev::RevisionOrder::Date => RevListOrdering::Date,
        sley_rev::RevisionOrder::AuthorDate => RevListOrdering::AuthorDate,
    };
    let (walk, no_walk_unsorted) = match revision_options.no_walk {
        sley_rev::NoWalkMode::Walk => (true, true),
        sley_rev::NoWalkMode::Sorted => (false, false),
        sley_rev::NoWalkMode::Unsorted => (false, true),
    };
    let first_parent = revision_options.first_parent;
    let pathspecs = setup.pathspecs;
    let full_history = revision_options.full_history;
    if graph && !walk {
        eprintln!("fatal: cannot combine --no-walk with --graph");
        return Err(GitError::Exit(128));
    }
    // Per-commit diff rendering context (only consulted when a diff-output
    // option was given).
    let log_diff = if diff_opts.any() || diff_opts.merges_imply_patch {
        let show_root = show_root_flag
            .unwrap_or_else(|| config.get_bool("log", None, "showroot").unwrap_or(true));
        // diff.renames: false disables detection, "copies"/"copy" adds copy
        // detection, anything else (or unset) means rename detection.
        let (detect_renames, detect_copies) = match config
            .get("diff", None, "renames")
            .map(|value| value.trim().to_ascii_lowercase())
            .as_deref()
        {
            Some("false") | Some("no") | Some("off") | Some("0") => (false, false),
            Some("copies") | Some("copy") => (true, true),
            _ => (true, false),
        };
        let diff_pathspec = if pathspecs.is_empty() {
            None
        } else {
            let cwd = env::current_dir()?;
            let worktree_root = worktree_root_for_git_dir(&git_dir)?;
            Some(DiffPathspec::new(&cwd, &worktree_root, &pathspecs)?)
        };
        let repo_abbrev = repository_abbrev(&git_dir, format)?;
        Some(LogDiffContext {
            db: &db,
            format,
            config: &config,
            opts: &diff_opts,
            merges: diff_opts.merges.unwrap_or(if first_parent {
                LogDiffMerges::FirstParent
            } else {
                LogDiffMerges::Off
            }),
            show_root,
            detect_renames,
            detect_copies,
            pathspec: diff_pathspec,
            patch_abbrev: repo_abbrev.unwrap_or(7).min(format.hex_len()),
            raw_abbrev: repo_abbrev,
        })
    } else {
        None
    };
    // Resolve the captured `--pretty=`/`--format=` spec now that config (and its
    // `pretty.<name>` aliases) is available.
    if let Some((spec, format_kind)) = pretty_spec.take() {
        match resolve_pretty_spec(&spec, format_kind, &config)? {
            ResolvedPretty::Oneline => preset_oneline = Some(true),
            ResolvedPretty::Default => output = LogOutput::Default(LogDefaultKind::Medium),
            ResolvedPretty::Reference => {
                // reference defaults the date to short; an explicit --date wins.
                if !date_explicit {
                    date_mode = DateMode::Short;
                }
                output = LogOutput::Compiled {
                    compiled: CompiledLogFormat::compile(
                        "%C(auto)%h (%s, %ad)",
                        LogFormatDialect::Log,
                    )?,
                    final_newline: true,
                    show_children: false,
                    inline_children: false,
                };
            }
            ResolvedPretty::Compiled {
                compiled,
                final_newline,
            } => {
                output = LogOutput::Compiled {
                    compiled,
                    final_newline,
                    show_children: false,
                    inline_children: false,
                };
            }
        }
    }
    if let Some(pretty_oneline) = preset_oneline {
        if matches!(output, LogOutput::Default(_)) {
            let use_full_oid = match pretty_oneline {
                true => !abbrev_commit,
                false => abbrev_len.is_none(),
            };
            output = LogOutput::Compiled {
                compiled: presets::log_oneline(
                    decoration != LogDecorationMode::Off,
                    use_full_oid,
                    show_parents,
                )?,
                final_newline: true,
                show_children,
                inline_children: true,
            };
        }
    } else if let LogOutput::Compiled {
        show_children: compiled_children,
        ..
    } = &mut output
    {
        *compiled_children = show_children;
    }
    if !abbrev_len_explicit && log_output_needs_abbrev(&output, abbrev_commit, show_children) {
        abbrev_len = repository_abbrev(&git_dir, format)?;
    }
    let author_filters =
        compile_log_filter_matcher(&author_patterns, pattern_kind, regexp_ignore_case, "header")?;
    let committer_filters =
        compile_log_filter_matcher(&committer_patterns, pattern_kind, regexp_ignore_case, "header")?;
    let grep_filters = compile_log_filter_matcher(
        &grep_patterns,
        pattern_kind,
        regexp_ignore_case,
        "command line",
    )?;
    if walk_reflogs {
        let reflog_revisions = revision_options
            .positives
            .iter()
            .filter_map(|tip| tip.source_name.clone())
            .collect::<Vec<_>>();
        return log_walk_reflogs(
            &git_dir,
            format,
            &reflog_revisions,
            max_count,
            skip,
            &output,
            reverse,
        );
    }
    let log_format_source = if !revision_options.had_ref_selector
        && revision_options.positives.len() == 1
    {
        revision_options.positives[0].source_name.clone()
    } else {
        None
    };
    let mut starts = Vec::new();
    // `(start_commit_oid, source_label)` pairs in command-line order, used to
    // build the `%S` per-commit source map (later starts override earlier ones).
    let mut source_starts: Vec<(ObjectId, String)> = Vec::new();
    for tip in &revision_options.positives {
        let commit = match sley_rev::peel_to_commit(&db, format, &tip.oid) {
            Ok(commit) => commit,
            Err(err) if tip.from_ref_selector => {
                let Ok(object) = db.read_object(&tip.oid) else {
                    return Err(err);
                };
                if matches!(object.object_type, ObjectType::Blob | ObjectType::Tree) {
                    continue;
                }
                return Err(err);
            }
            Err(err) => return Err(err),
        };
        if let Some(source_name) = &tip.source_name {
            source_starts.push((commit, source_name.clone()));
        }
        starts.push(commit);
    }
    let mut excluded = HashSet::new();
    for oid in &revision_options.negatives {
        for record in rev_list_walk_commits(&db, format, [*oid], first_parent)? {
            excluded.insert(record.oid);
        }
    }
    if walk
        && !graph
        && line_prefix.is_none()
        && matches!(ordering, RevListOrdering::Default | RevListOrdering::Date)
        && pathspecs.is_empty()
        && !full_history
        && matches!(&output, LogOutput::Compiled { compiled, show_children: false, .. }
            if compiled.is_metadata_emitable() && compiled.uses_oid() && !compiled.uses_decorations())
        && decoration == LogDecorationMode::Off
        && !show_children
        && author_filters.is_none()
        && committer_filters.is_none()
        && grep_filters.is_none()
        && max_age.is_none()
        && min_age.is_none()
        && min_parents.is_none()
        && max_parents.is_none()
    {
        let limit = max_count.map(|max| skip.saturating_add(max));
        let metadata = if let Some(limit) = limit.filter(|limit| *limit > 0) {
            sley_rev::walk_commit_metadata_date_ordered_limited(
                &git_dir,
                format,
                &db,
                starts.clone(),
                first_parent,
                limit,
            )?
        } else {
            sley_rev::walk_commit_metadata(&git_dir, format, &db, starts.clone(), first_parent)?
        };
        let mut selected = metadata
            .into_iter()
            .filter(|record| !excluded.contains(&record.oid))
            .collect::<Vec<_>>();
        if limit.is_none() {
            selected = rev_list_metadata_date_order(selected);
        }
        if skip > 0 {
            selected = selected.into_iter().skip(skip).collect();
        }
        if let Some(max_count) = max_count {
            selected.truncate(max_count);
        }
        if reverse {
            selected.reverse();
        }
        let (compiled, final_newline) = match &output {
            LogOutput::Compiled {
                compiled,
                final_newline,
                ..
            } => (compiled, *final_newline),
            _ => unreachable!("metadata fast path requires compiled output"),
        };
        let mut stdout = io::stdout();
        let term: &[u8] = if null_terminate { b"\0" } else { b"\n" };
        let mut line = Vec::with_capacity(compiled.estimated_line_capacity());
        for (index, record) in selected.iter().enumerate() {
            // `--pretty=format:` separates entries with a newline (none trailing);
            // `--format=`/`tformat:`/oneline terminate each entry with one.
            if index > 0 && !final_newline {
                stdout.write_all(term)?;
            }
            line.clear();
            emit_compiled_log_format_metadata(
                record,
                compiled,
                &LogFormatContext {
                    abbrev_len,
                    decorations: &HashMap::new(),
                    marker: '>',
                    dialect: LogFormatDialect::Log,
                    source: log_format_source.as_deref(),
                    date_mode: &date_mode,
                    source_oid: None,
                    describe: None,
                    color: false,
                    output_encoding: &output_encoding,
                },
                &mut line,
            )?;
            stdout.write_all(&line)?;
            if final_newline {
                stdout.write_all(term)?;
            }
        }
        stdout.flush()?;
        return Ok(());
    }
    if walk
        && !graph
        && line_prefix.is_none()
        && matches!(ordering, RevListOrdering::Default | RevListOrdering::Date)
        && pathspecs.is_empty()
        && !full_history
        && matches!(&output, LogOutput::Compiled { compiled, show_children: false, .. }
            if log_limited_commit_format_supported(compiled))
        && decoration == LogDecorationMode::Off
        && !show_children
        && excluded.is_empty()
        && author_filters.is_none()
        && committer_filters.is_none()
        && grep_filters.is_none()
        && max_age.is_none()
        && min_age.is_none()
        && min_parents.is_none()
        && max_parents.is_none()
        && let Some(limit) = max_count.map(|max| skip.saturating_add(max))
        && limit > 0
    {
        let (compiled, final_newline) = match &output {
            LogOutput::Compiled {
                compiled,
                final_newline,
                ..
            } => (compiled, *final_newline),
            _ => unreachable!("limited commit fast path requires compiled output"),
        };
        let mut stdout = io::stdout();
        let term: &[u8] = if null_terminate { b"\0" } else { b"\n" };
        let context = LogFormatContext {
            abbrev_len,
            decorations: &HashMap::new(),
            marker: '>',
            dialect: LogFormatDialect::Log,
            source: log_format_source.as_deref(),
            date_mode: &date_mode,
            source_oid: None,
            describe: None,
            color: false,
            output_encoding: &output_encoding,
        };
        let metadata = sley_rev::walk_commit_metadata_date_ordered_limited(
            &git_dir,
            format,
            &db,
            starts.clone(),
            first_parent,
            limit,
        )?;
        let mut selected = metadata.into_iter().collect::<Vec<_>>();
        if skip > 0 {
            selected = selected.into_iter().skip(skip).collect();
        }
        selected.truncate(max_count.expect("limited log path requires max-count"));
        if reverse {
            selected.reverse();
        }
        let mut line = Vec::with_capacity(compiled.estimated_line_capacity());
        for (index, metadata) in selected.iter().enumerate() {
            if index > 0 && !final_newline {
                stdout.write_all(term)?;
            }
            line.clear();
            emit_compiled_log_format_limited_commit(
                &db,
                metadata,
                compiled,
                &context,
                &mut line,
            )?;
            let out = log_reencode_message(&line, "UTF-8", context.output_encoding);
            stdout.write_all(&out)?;
            if final_newline {
                stdout.write_all(term)?;
            }
        }
        stdout.flush()?;
        return Ok(());
    }
    let commits = if walk {
        rev_list_walk_commits(&db, format, starts, first_parent)?
    } else {
        rev_list_no_walk_commits(&db, format, starts)?
    };
    let mut child_oids = HashMap::<ObjectId, Vec<ObjectId>>::new();
    if show_children {
        for record in &commits {
            for parent in &record.parents {
                child_oids.entry(*parent).or_default().push(record.oid);
            }
        }
        for children in child_oids.values_mut() {
            children.reverse();
        }
    }
    let mut selected = Vec::new();
    for record in &commits {
        if excluded.contains(&record.oid)
            || min_parents.is_some_and(|min| record.parents.len() < min)
            || max_parents.is_some_and(|max| record.parents.len() > max)
            || !log_age_filters_match(record, max_age, min_age)?
            || !log_author_matcher_matches(record, author_filters.as_ref())
            || !log_committer_matcher_matches(record, committer_filters.as_ref())
            || !log_grep_matcher_matches(
                record,
                grep_filters.as_ref(),
                grep_all_match,
                invert_grep,
            )
        {
            continue;
        }
        selected.push(record);
    }
    selected = match ordering {
        // `--graph` implies topological ordering (upstream sets
        // `revs->topo_order = 1`); `--date-order`/`--author-date-order` pick
        // the date-keyed topo variants, which the helpers below already are.
        RevListOrdering::Default if graph => rev_list_topo_order(selected)?,
        RevListOrdering::Default if walk => rev_list_date_order(selected)?,
        RevListOrdering::Default if !no_walk_unsorted => {
            // `--no-walk[=sorted]`: a plain stable commit-time sort (upstream
            // `commit_list_sort_by_date`), newest first.
            let mut keyed = selected
                .iter()
                .map(|record| {
                    commit_identity_timestamp_i64(&record.commit.committer)
                        .map(|timestamp| (timestamp, *record))
                })
                .collect::<Result<Vec<_>>>()?;
            keyed.sort_by_key(|(timestamp, _)| std::cmp::Reverse(*timestamp));
            keyed.into_iter().map(|(_, record)| record).collect()
        }
        RevListOrdering::Default => selected,
        RevListOrdering::Topo => rev_list_topo_order(selected)?,
        RevListOrdering::Date => rev_list_date_order(selected)?,
        RevListOrdering::AuthorDate => rev_list_author_date_order(selected)?,
    };
    // Pathspec-limited / --full-history simplification (TREESAME prune + parent
    // rewriting). Owned binding outlives `selected` (a Vec of references).
    let simplified_storage;
    if !pathspecs.is_empty() || full_history {
        let pathspec = sley_rev::Pathspec::parse(
            pathspecs.iter().map(|p| p.as_bytes()),
            sley_rev::PathspecMatchMagic::default(),
        )
        .map_err(|err| GitError::Command(format!("bad pathspec: {err:?}")))?;
        let ordered_owned: Vec<sley_rev::CommitRecord> =
            selected.iter().map(|r| (*r).clone()).collect();
        simplified_storage = sley_rev::simplify_history(
            &db,
            format,
            ordered_owned,
            &pathspec,
            sley_rev::SimplifyOptions {
                full_history,
                first_parent,
            },
        )?;
        selected = simplified_storage.iter().collect();
    }
    // For `--graph`, a parent is "interesting" iff it will be shown — judged
    // against the full selection BEFORE `--skip`/`-n` truncation (matching
    // upstream `get_commit_action`, which is truncation-blind).
    let graph_shown: Option<HashSet<ObjectId>> =
        graph.then(|| selected.iter().map(|record| record.oid).collect());
    if skip > 0 {
        selected = selected.into_iter().skip(skip).collect();
    }
    if let Some(max_count) = max_count {
        selected.truncate(max_count);
    }
    if reverse {
        selected.reverse();
    }
    let custom_decoration_mode = match &output {
        LogOutput::Compiled { compiled, .. } if compiled.uses_decorations() => {
            Some(if decoration == LogDecorationMode::Full {
                LogDecorationMode::Full
            } else {
                LogDecorationMode::Short
            })
        }
        _ => None,
    };
    let decorations = if decoration == LogDecorationMode::Off && custom_decoration_mode.is_none() {
        HashMap::new()
    } else {
        log_decoration_map(
            &git_dir,
            &db,
            format,
            custom_decoration_mode.unwrap_or(decoration),
        )?
    };
    // Object access for `%(describe)`.
    let describe_ctx = LogDescribeContext {
        git_dir: &git_dir,
        db: &db,
        format,
    };
    // `%S` source labels: each commit is tagged with the start ref from which it
    // is reachable; when several starts reach it, the last one (command-line
    // order) wins — matching git's `revision.c` source naming.
    let format_uses_source =
        matches!(&output, LogOutput::Compiled { compiled, .. } if compiled.uses_source());
    let source_labels: Option<HashMap<ObjectId, String>> =
        if format_uses_source && !source_starts.is_empty() {
            let mut map = HashMap::new();
            for (start_oid, label) in &source_starts {
                for record in rev_list_walk_commits(&db, format, [*start_oid], first_parent)? {
                    map.insert(record.oid, label.clone());
                }
            }
            Some(map)
        } else {
            None
        };
    // Resolve the notes-display refs once. Notes show by default only for the
    // medium (no-`--pretty`) format; an explicit `--notes`/`--no-notes` flag
    // overrides. The empty list short-circuits all per-commit note lookups.
    let notes_store = FileRefStore::new(&git_dir, format);
    let notes_default_format = matches!(output, LogOutput::Default(LogDefaultKind::Medium));
    let display_notes_refs = if notes_display.is_active(notes_default_format) {
        notes_display.resolve_refs(&git_dir, &notes_store)?
    } else {
        Vec::new()
    };

    if let Some(shown) = &graph_shown {
        let palette = log_graph_color_palette(&config);
        let mut graph_state = sley_rev::graph::Graph::new(palette, color_always);
        let prefix: &str = line_prefix.as_deref().unwrap_or("");
        let mut out = io::stdout();
        // Whether the previous entry's message ended without a newline
        // (upstream `opt->missing_newline`), for the separator decision.
        let mut prev_missing_newline = false;
        for (index, record) in selected.iter().enumerate() {
            let mut interesting: Vec<ObjectId> = record
                .parents
                .iter()
                .filter(|parent| shown.contains(*parent))
                .copied()
                .collect();
            if first_parent {
                interesting.truncate(1);
            }
            graph_state.update(record.oid, &interesting);
            match &output {
                LogOutput::Compiled {
                    compiled,
                    final_newline,
                    ..
                } => {
                    if index > 0 && !*final_newline {
                        // `--pretty=format:` separator semantics.
                        if !prev_missing_newline {
                            graph_show_padding(&mut graph_state, prefix, &mut out)?;
                        }
                        out.write_all(b"\n")?;
                    }
                    graph_show_commit(&mut graph_state, prefix, &mut out)?;
                    let format_context = LogFormatContext {
                        abbrev_len,
                        decorations: &decorations,
                        marker: '>',
                        dialect: LogFormatDialect::Log,
                        source: log_format_source.as_deref(),
                        date_mode: &date_mode,
                        source_oid: source_labels.as_ref(),
                        describe: Some(&describe_ctx),
                        color: color_always,
                        output_encoding: &output_encoding,
                    };
                    let mut msg = Vec::with_capacity(compiled.estimated_line_capacity());
                    emit_compiled_log_format(
                        record,
                        compiled,
                        &format_context,
                        &mut msg,
                        0..compiled.tokens.len(),
                    )?;
                    if let Some(log_diff) = &log_diff {
                        let mut padding = String::new();
                        graph_state.padding_line(&mut padding);
                        let prefix_width =
                            log_prefix_display_width(&padding) + log_prefix_display_width(prefix);
                        let block = log_diff.render(record, prefix_width)?;
                        if !block.is_empty() {
                            if msg.last() != Some(&b'\n') {
                                msg.push(b'\n');
                            }
                            msg.extend_from_slice(&block);
                        }
                    }
                    graph_show_commit_msg(&mut graph_state, prefix, &msg, &mut out)?;
                    let newline_terminated = msg.last() == Some(&b'\n');
                    prev_missing_newline = !newline_terminated;
                    if *final_newline {
                        if newline_terminated {
                            graph_show_padding(&mut graph_state, prefix, &mut out)?;
                        }
                        out.write_all(b"\n")?;
                        prev_missing_newline = false;
                    }
                }
                LogOutput::Default(kind) => {
                    if index > 0 {
                        graph_show_padding(&mut graph_state, prefix, &mut out)?;
                        out.write_all(b"\n")?;
                    }
                    graph_show_commit(&mut graph_state, prefix, &mut out)?;
                    write!(
                        out,
                        "commit {}",
                        format_log_commit_header_oid(&record.oid, abbrev_commit, abbrev_len)
                    )?;
                    if let Some(labels) = decorations.get(&record.oid)
                        && !labels.is_empty()
                    {
                        write!(out, " ({})", labels.join(", "))?;
                    }
                    out.write_all(b"\n")?;
                    graph_show_oneline(&mut graph_state, prefix, &mut out)?;
                    let mut msg: Vec<u8> = Vec::new();
                    if record.parents.len() > 1 {
                        let merged: Vec<String> =
                            record.parents.iter().map(format_log_abbrev_oid).collect();
                        writeln!(msg, "Merge: {}", merged.join(" ")).map_err(io::Error::from)?;
                    }
                    writeln!(
                        msg,
                        "Author: {}",
                        commit_author_identity(&record.commit.author)
                    )
                    .map_err(io::Error::from)?;
                    if *kind == LogDefaultKind::Medium {
                        writeln!(
                            msg,
                            "Date:   {}",
                            commit_identity_date(&record.commit.author, &date_mode)
                        )
                        .map_err(io::Error::from)?;
                    }
                    msg.push(b'\n');
                    for line in String::from_utf8_lossy(&record.commit.message).lines() {
                        if line.is_empty() {
                            msg.push(b'\n');
                        } else {
                            writeln!(msg, "    {line}").map_err(io::Error::from)?;
                        }
                    }
                    if let Some(log_diff) = &log_diff {
                        // Measure the graph padding that will prefix the diff
                        // lines so the stat width math sees the same budget
                        // git's line-prefix callback gives it.
                        let mut padding = String::new();
                        graph_state.padding_line(&mut padding);
                        let prefix_width =
                            log_prefix_display_width(&padding) + log_prefix_display_width(prefix);
                        let block = log_diff.render(record, prefix_width)?;
                        if !block.is_empty() {
                            msg.extend_from_slice(diff_opts.block_separator());
                            msg.extend_from_slice(&block);
                        }
                    }
                    graph_show_commit_msg(&mut graph_state, prefix, &msg, &mut out)?;
                    prev_missing_newline = false;
                }
            }
        }
        out.flush()?;
        return Ok(());
    }

    let mut printed_entries = 0usize;
    for (index, record) in selected.iter().enumerate() {
        match output {
            LogOutput::Default(kind) => {
                // The diff block is rendered up front: whatchanged
                // (always_show_header = 0) omits the whole entry when the
                // commit's diff comes out empty.
                let diff_block = match &log_diff {
                    Some(log_diff) => {
                        let prefix_width =
                            log_prefix_display_width(line_prefix.as_deref().unwrap_or(""));
                        log_diff.render(record, prefix_width)?
                    }
                    None => Vec::new(),
                };
                if whatchanged && log_diff.is_some() && diff_block.is_empty() {
                    continue;
                }
                if printed_entries > 0 {
                    println!();
                }
                printed_entries += 1;
                print!(
                    "commit {}",
                    format_log_commit_header_oid(&record.oid, abbrev_commit, abbrev_len)
                );
                print_log_decorations(&record.oid, &decorations);
                print_log_selected_parent_oids(
                    record,
                    show_parents,
                    abbrev_commit.then_some(abbrev_len).flatten(),
                );
                println!();
                if record.parents.len() > 1 {
                    let merged: Vec<String> =
                        record.parents.iter().map(format_log_abbrev_oid).collect();
                    println!("Merge: {}", merged.join(" "));
                }
                println!("Author: {}", commit_author_identity(&record.commit.author));
                if kind == LogDefaultKind::Medium {
                    println!(
                        "Date:   {}",
                        commit_identity_date(&record.commit.author, &date_mode)
                    );
                }
                println!();
                for line in String::from_utf8_lossy(&record.commit.message).lines() {
                    println!("    {line}");
                }
                if !display_notes_refs.is_empty() {
                    let notes = render_notes_block(
                        &git_dir,
                        format,
                        &notes_store,
                        &display_notes_refs,
                        &record.oid,
                    )?;
                    io::stdout().write_all(&notes)?;
                }
                if !diff_block.is_empty() {
                    let mut stdout = io::stdout();
                    stdout.write_all(diff_opts.block_separator())?;
                    stdout.write_all(&diff_block)?;
                }
            }
            LogOutput::Compiled {
                ref compiled,
                final_newline,
                show_children: compiled_children,
                inline_children,
            } => {
                printed_entries += 1;
                let term: &[u8] = if null_terminate { b"\0" } else { b"\n" };
                if index > 0 && !final_newline {
                    io::stdout().write_all(term)?;
                }
                let format_context = LogFormatContext {
                    abbrev_len,
                    decorations: &decorations,
                    marker: '>',
                    dialect: LogFormatDialect::Log,
                    source: log_format_source.as_deref(),
                    date_mode: &date_mode,
                    source_oid: source_labels.as_ref(),
                    describe: Some(&describe_ctx),
                    color: false,
                    output_encoding: &output_encoding,
                };
                if compiled_children && inline_children {
                    print_log_format_with_children(
                        record,
                        compiled,
                        format_context,
                        &child_oids,
                        abbrev_len,
                    )?;
                } else if let Some(prefix) = &line_prefix {
                    // `--line-prefix=<p>` prefixes every output line.
                    let mut line = Vec::with_capacity(compiled.estimated_line_capacity());
                    emit_compiled_log_format(
                        record,
                        compiled,
                        &format_context,
                        &mut line,
                        0..compiled.tokens.len(),
                    )?;
                    let mut stdout = io::stdout();
                    let mut start = 0usize;
                    while start < line.len() {
                        let end = line[start..]
                            .iter()
                            .position(|&byte| byte == b'\n')
                            .map(|pos| start + pos + 1)
                            .unwrap_or(line.len());
                        stdout.write_all(prefix.as_bytes())?;
                        stdout.write_all(&line[start..end])?;
                        start = end;
                    }
                    if line.is_empty() {
                        stdout.write_all(prefix.as_bytes())?;
                    }
                } else {
                    print_log_format(record, compiled, format_context)?;
                }
                if final_newline {
                    io::stdout().write_all(term)?;
                }
                if let Some(log_diff) = &log_diff {
                    // oneline/format outputs put the diff right after the
                    // entry, with no separating blank line. `--line-prefix`
                    // narrows the stat budget and prefixes every diff line.
                    let prefix = line_prefix.as_deref().unwrap_or("");
                    let block = log_diff.render(record, log_prefix_display_width(prefix))?;
                    if !block.is_empty() {
                        let mut stdout = io::stdout();
                        if !final_newline {
                            stdout.write_all(term)?;
                        }
                        if prefix.is_empty() {
                            stdout.write_all(&block)?;
                        } else {
                            let mut start = 0usize;
                            while start < block.len() {
                                let end = block[start..]
                                    .iter()
                                    .position(|&byte| byte == b'\n')
                                    .map(|pos| start + pos + 1)
                                    .unwrap_or(block.len());
                                stdout.write_all(prefix.as_bytes())?;
                                stdout.write_all(&block[start..end])?;
                                start = end;
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

fn log_walk_reflogs(
    git_dir: &Path,
    format: ObjectFormat,
    revisions: &[String],
    max_count: Option<usize>,
    skip: usize,
    output: &LogOutput,
    reverse: bool,
) -> Result<()> {
    let reference = reflog_reference_name(revisions.first().map(String::as_str))?;
    let store = FileRefStore::new(git_dir, format);
    let mut entries = store.read_reflog(&reference)?;
    entries.reverse();
    if skip > 0 {
        entries = entries.into_iter().skip(skip).collect();
    }
    if let Some(max_count) = max_count {
        entries.truncate(max_count);
    }
    if reverse {
        entries.reverse();
    }
    let mut stdout = io::stdout();
    for (index, entry) in entries.iter().enumerate() {
        match output {
            LogOutput::Compiled {
                compiled,
                final_newline,
                ..
            } => {
                let mut line = Vec::with_capacity(compiled.estimated_line_capacity());
                emit_compiled_reflog_walk_format(compiled, entry, index, &reference, &mut line)?;
                stdout.write_all(&line)?;
                if *final_newline && !line.ends_with(b"\n") {
                    stdout.write_all(b"\n")?;
                }
            }
            LogOutput::Default(_) => {
                stdout.write_all(&entry.message)?;
                stdout.write_all(b"\n")?;
            }
        }
    }
    stdout.flush()?;
    Ok(())
}

fn compile_log_filter_matcher(
    patterns: &[String],
    kind: crate::grep_source::PatternKind,
    ignore_case: bool,
    error_context: &str,
) -> Result<Option<crate::grep_source::GrepMatcher>> {
    if patterns.is_empty() {
        return Ok(None);
    }
    crate::grep_source::GrepMatcher::compile_with_error_context(
        crate::grep_source::GrepCompileConfig {
            patterns,
            kind,
            ignore_case,
            word: false,
            line_regexp: false,
            diagnostic_verbosity: crate::grep_source::RegexDiagnosticVerbosity::from_env(),
        },
        error_context,
    )
    .map(Some)
}

fn log_author_matcher_matches(
    record: &sley_rev::CommitRecord,
    filter: Option<&crate::grep_source::GrepMatcher>,
) -> bool {
    filter.is_none_or(|filter| filter.matches_any(&record.commit.author))
}

fn log_committer_matcher_matches(
    record: &sley_rev::CommitRecord,
    filter: Option<&crate::grep_source::GrepMatcher>,
) -> bool {
    filter.is_none_or(|filter| filter.matches_any(&record.commit.committer))
}

fn log_grep_matcher_matches(
    record: &sley_rev::CommitRecord,
    filter: Option<&crate::grep_source::GrepMatcher>,
    all_match: bool,
    invert: bool,
) -> bool {
    let Some(filter) = filter else {
        return true;
    };
    let matched = if all_match {
        filter.matches_all(&record.commit.message)
    } else {
        filter.matches_any(&record.commit.message)
    };
    matched != invert
}

fn emit_compiled_reflog_walk_format(
    compiled: &CompiledLogFormat,
    entry: &ReflogEntry,
    index: usize,
    reference: &str,
    out: &mut Vec<u8>,
) -> Result<()> {
    let (reflog_name, reflog_email) = commit_identity_name_email(&entry.committer);
    for token in &compiled.tokens {
        match token {
            FormatToken::Literal(text) => out.extend_from_slice(text.as_bytes()),
            FormatToken::Percent => out.push(b'%'),
            FormatToken::ReflogGs => out.extend_from_slice(&entry.message),
            FormatToken::ReflogGd | FormatToken::ReflogGD => {
                write!(out, "{reference}@{{{index}}}").map_err(io::Error::from)?;
            }
            FormatToken::ReflogGn => out.extend_from_slice(reflog_name.as_bytes()),
            FormatToken::ReflogGe => out.extend_from_slice(reflog_email.as_bytes()),
            FormatToken::Newline => out.push(b'\n'),
            FormatToken::HexByte(byte) => out.push(*byte),
            _ => {}
        }
    }
    Ok(())
}

/// The outcome of resolving a `--pretty=`/`--format=` spec.
enum ResolvedPretty {
    Oneline,
    Default,
    /// `--pretty=reference`: `%C(auto)%h (%s, %ad)` with a default short date
    /// that an explicit `--date=` overrides (but `log.date` config does not).
    Reference,
    Compiled {
        compiled: CompiledLogFormat,
        final_newline: bool,
    },
}

/// Resolve a `--pretty=`/`--format=` spec into a compiled format, mirroring
/// git's `get_commit_format` + `pretty.<name>` alias chain. `format_kind` is the
/// `--format=`/`tformat:` flag (terminator semantics → `final_newline: true`).
fn resolve_pretty_spec(
    spec: &str,
    format_kind: bool,
    config: &GitConfig,
) -> Result<ResolvedPretty> {
    // Follow `pretty.<name>` aliases (case-insensitive) up to a bounded depth,
    // matching git's loop guard against alias cycles.
    let mut current = spec.to_string();
    // `--format=`/`tformat:` apply terminator semantics. A `--format=X` with no
    // recognized prefix is treated as a user format `tformat:X`.
    let mut terminate = format_kind;
    for _ in 0..32 {
        if let Some(rest) = current.strip_prefix("format:") {
            return Ok(ResolvedPretty::Compiled {
                compiled: CompiledLogFormat::compile(rest, LogFormatDialect::Log)?,
                final_newline: terminate,
            });
        }
        if let Some(rest) = current.strip_prefix("tformat:") {
            return Ok(ResolvedPretty::Compiled {
                compiled: CompiledLogFormat::compile(rest, LogFormatDialect::Log)?,
                final_newline: true,
            });
        }
        match current.as_str() {
            "oneline" => return Ok(ResolvedPretty::Oneline),
            "short" | "medium" => return Ok(ResolvedPretty::Default),
            "reference" => {
                return Ok(ResolvedPretty::Reference);
            }
            _ => {}
        }
        // Try a `pretty.<name>` alias (case-insensitive); aliases may chain.
        if let Some(value) = config.get("pretty", None, &current) {
            current = value.to_string();
            terminate = false;
            continue;
        }
        // No builtin or alias matched. `--format=<raw>` treats the value as a
        // user format string with terminator semantics; bare `--pretty=<raw>`
        // does too when it contains a `%` placeholder (git's heuristic).
        if terminate || current.contains('%') {
            return Ok(ResolvedPretty::Compiled {
                compiled: CompiledLogFormat::compile(&current, LogFormatDialect::Log)?,
                final_newline: true,
            });
        }
        eprintln!("fatal: invalid --pretty format: {spec}");
        return Err(GitError::Exit(128));
    }
    eprintln!("fatal: invalid --pretty format: {spec}");
    Err(GitError::Exit(128))
}

// ---------------------------------------------------------------------------
// `--graph` rendering helpers (upstream graph.c's `graph_show_*` family)
// ---------------------------------------------------------------------------

/// The `log.graphColors` palette (empty -> the renderer's ANSI default).
/// Invalid entries are warned about and skipped, like upstream
/// `parse_graph_colors_config`.
fn log_graph_color_palette(config: &GitConfig) -> Vec<String> {
    let Some(value) = config.get("log", None, "graphColors") else {
        return Vec::new();
    };
    let mut palette = Vec::new();
    for token in value.split(',') {
        if token.trim().is_empty() {
            eprintln!("warning: ignored invalid color '{token}' in log.graphColors");
            continue;
        }
        match crate::commands::config_cmd::try_format_config_color_value(token) {
            Ok(code) => palette.push(code),
            Err(()) => {
                eprintln!("warning: ignored invalid color '{token}' in log.graphColors");
            }
        }
    }
    palette
}

/// Emit graph rows up to and including the current commit's row (no trailing
/// newline), prefixing each physical line with `prefix`.

/// How merge commits participate in log diff output (`--diff-merges`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LogDiffMerges {
    /// Merges show no diff (the porcelain default).
    Off,
    /// Merges diff against their first parent (`--diff-merges=first-parent`,
    /// and the default under `--first-parent`).
    FirstParent,
}

/// Parse a `--diff-merges=<value>` into the supported modes.
fn log_parse_diff_merges(value: &str) -> Result<LogDiffMerges> {
    match value {
        "off" | "none" => Ok(LogDiffMerges::Off),
        // "on"/"m" follow the diff-merges default (separate); sley renders the
        // first-parent diff for these until separate/combined modes land.
        "first-parent" | "1" | "on" | "separate" | "m" => Ok(LogDiffMerges::FirstParent),
        "" => {
            eprintln!("fatal: invalid value for '--diff-merges': '{value}'");
            Err(GitError::Exit(128))
        }
        "combined" | "c" | "dense-combined" | "cc" | "remerge" | "r" => Err(GitError::Command(
            format!("unsupported log option --diff-merges={value}"),
        )),
        _ => {
            eprintln!("fatal: invalid value for '--diff-merges': '{value}'");
            Err(GitError::Exit(128))
        }
    }
}

/// Diff-output options accepted by `git log` (`-p`, `--stat`, `--raw`, ...).
#[derive(Debug, Clone)]
struct LogDiffOptions {
    patch: bool,
    stat: bool,
    raw: bool,
    numstat: bool,
    shortstat: bool,
    summary: bool,
    compact_summary: bool,
    stat_widths: DiffStatWidths,
    stat_count: Option<usize>,
    merges: Option<LogDiffMerges>,
    /// Whether an explicit `--diff-merges=<mode>` was given: unlike `-m`, the
    /// explicit form enables patch output for merge commits on its own.
    merges_imply_patch: bool,
}

impl Default for LogDiffOptions {
    fn default() -> Self {
        LogDiffOptions {
            patch: false,
            stat: false,
            raw: false,
            numstat: false,
            shortstat: false,
            summary: false,
            compact_summary: false,
            stat_widths: DiffStatWidths::terminal(),
            stat_count: None,
            merges: None,
            merges_imply_patch: false,
        }
    }
}

impl LogDiffOptions {
    /// The bytes separating a commit's message from its diff block in the
    /// default output: `---` when a diffstat accompanies the patch
    /// (`--patch-with-stat`), a blank line otherwise.
    fn block_separator(&self) -> &'static [u8] {
        if self.patch && (self.stat || self.compact_summary) {
            b"---\n"
        } else {
            b"\n"
        }
    }

    /// Whether any diff output was requested at all.
    fn any(&self) -> bool {
        self.patch
            || self.stat
            || self.raw
            || self.numstat
            || self.shortstat
            || self.summary
            || self.compact_summary
    }
}

/// Per-walk context for rendering each commit's diff block.
struct LogDiffContext<'a> {
    db: &'a FileObjectDatabase,
    format: ObjectFormat,
    config: &'a GitConfig,
    opts: &'a LogDiffOptions,
    merges: LogDiffMerges,
    show_root: bool,
    detect_renames: bool,
    detect_copies: bool,
    pathspec: Option<DiffPathspec>,
    patch_abbrev: usize,
    raw_abbrev: Option<usize>,
}

impl LogDiffContext<'_> {
    /// Render the diff block for one commit (against its first parent, or the
    /// empty tree for roots when log.showRoot allows). Returns an empty buffer
    /// when nothing is to be shown; otherwise the buffer holds the block's
    /// lines WITHOUT a leading blank line (the caller owns separators, which
    /// differ between the default and oneline/format outputs).
    fn render(&self, record: &sley_rev::CommitRecord, line_prefix_width: i64) -> Result<Vec<u8>> {
        // An explicit non-off --diff-merges without any diff-output option
        // shows patches for merge commits only.
        let merges_only = !self.opts.any();
        if merges_only && (record.commit.parents.len() <= 1 || self.merges == LogDiffMerges::Off) {
            return Ok(Vec::new());
        }
        let parents = &record.commit.parents;
        let parent_tree = match parents.len() {
            0 => {
                if !self.show_root {
                    return Ok(Vec::new());
                }
                None
            }
            1 => Some(self.parent_tree(&parents[0])?),
            _ => match self.merges {
                LogDiffMerges::Off => return Ok(Vec::new()),
                LogDiffMerges::FirstParent => Some(self.parent_tree(&parents[0])?),
            },
        };
        let base = sley_diff_merge::DiffNameStatusOptions {
            detect_renames: self.detect_renames,
            detect_copies: self.detect_copies,
            find_copies_harder: false,
            rename_empty: true,
        };
        let tree = &record.commit.tree;
        let entries = match (&parent_tree, self.detect_renames) {
            (Some(parent), true) => sley_diff_merge::diff_name_status_trees_with_rename_options(
                self.db,
                self.format,
                parent,
                tree,
                sley_diff_merge::RenameDetectionOptions {
                    base,
                    detect_inexact: true,
                    rename_threshold: sley_diff_merge::DEFAULT_RENAME_THRESHOLD,
                    copy_threshold: sley_diff_merge::DEFAULT_RENAME_THRESHOLD,
                },
            )?,
            (Some(parent), false) => sley_diff_merge::diff_name_status_trees_with_options(
                self.db,
                self.format,
                parent,
                tree,
                base,
            )?,
            (None, _) => sley_diff_merge::diff_name_status_empty_tree_with_options(
                self.db,
                self.format,
                tree,
                base,
            )?,
        };
        let entries = match &self.pathspec {
            Some(pathspec) => apply_diff_pathspec(entries, pathspec),
            None => entries,
        };
        if entries.is_empty() {
            return Ok(Vec::new());
        }

        let mut out: Vec<u8> = Vec::new();
        let opts = self.opts;
        let patch = opts.patch || merges_only;
        if opts.raw {
            for entry in &entries {
                write_diff_raw_entry(&mut out, entry, false, false, self.raw_abbrev, self.format)?;
            }
        }
        if opts.numstat {
            for entry in &entries {
                write_diff_numstat_entry(&mut out, entry, false, self.db, None, false)?;
            }
        }
        if opts.stat || opts.compact_summary {
            let mut widths = opts.stat_widths;
            widths.resolve_config(self.config);
            widths.line_prefix_width = line_prefix_width;
            write_diff_stat_with_widths(
                &mut out,
                &entries,
                self.db,
                None,
                false,
                DiffStatOptions {
                    compact_summary: opts.compact_summary,
                    stat_count: opts.stat_count,
                    color: false,
                },
                widths,
            )?;
        }
        if opts.shortstat {
            write_diff_shortstat(&mut out, &entries, self.db, None, false)?;
        }
        if opts.summary {
            for entry in &entries {
                write_diff_summary_entry(&mut out, entry)?;
            }
        }
        if patch {
            if opts.raw
                || opts.numstat
                || opts.stat
                || opts.compact_summary
                || opts.shortstat
                || opts.summary
            {
                out.push(b'\n');
            }
            for entry in &entries {
                write_diff_patch_entry(
                    &mut out,
                    entry,
                    DiffPatchOptions {
                        db: self.db,
                        worktree_root: None,
                        use_worktree_new: false,
                        format: self.format,
                        abbrev: self.patch_abbrev,
                        src_prefix: "a/",
                        dst_prefix: "b/",
                        context: 3,
                        userdiff: None,
                        colors: None,
                        word_diff: None,
                        no_index_contents: None,
                        dirty_submodules: None,
                    },
                )?;
            }
        }
        Ok(out)
    }

    /// Tree oid of `parent`.
    fn parent_tree(&self, parent: &ObjectId) -> Result<ObjectId> {
        let object = self.db.read_object(parent)?;
        Ok(Commit::parse_ref(self.format, &object.body)?.tree)
    }
}

/// Display width of a line prefix, skipping ANSI SGR escapes (git
/// `utf8_strnwidth(..., skip_ansi=1)`).
fn log_prefix_display_width(prefix: &str) -> i64 {
    let mut width = 0i64;
    let mut chars = prefix.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for esc in chars.by_ref() {
                    if esc.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        width += 1;
    }
    width
}

fn graph_show_commit(
    graph: &mut sley_rev::graph::Graph,
    prefix: &str,
    out: &mut dyn Write,
) -> Result<()> {
    write!(out, "{prefix}")?;
    let mut shown = false;
    while !shown && !graph.is_commit_finished() {
        let mut row = String::new();
        shown = graph.next_line(&mut row);
        out.write_all(row.as_bytes())?;
        if !shown {
            out.write_all(b"\n")?;
            write!(out, "{prefix}")?;
        }
    }
    Ok(())
}

/// Emit a single graph row (no trailing newline).
fn graph_show_oneline(
    graph: &mut sley_rev::graph::Graph,
    prefix: &str,
    out: &mut dyn Write,
) -> Result<()> {
    write!(out, "{prefix}")?;
    let mut row = String::new();
    graph.next_line(&mut row);
    out.write_all(row.as_bytes())?;
    Ok(())
}

/// Emit a padding row (no trailing newline).
fn graph_show_padding(
    graph: &mut sley_rev::graph::Graph,
    prefix: &str,
    out: &mut dyn Write,
) -> Result<()> {
    write!(out, "{prefix}")?;
    let mut row = String::new();
    graph.padding_line(&mut row);
    out.write_all(row.as_bytes())?;
    Ok(())
}

/// Emit the remaining graph rows for the current commit; ends WITHOUT a
/// trailing newline (upstream `graph_show_remainder`).
fn graph_show_remainder(
    graph: &mut sley_rev::graph::Graph,
    prefix: &str,
    out: &mut dyn Write,
) -> Result<()> {
    write!(out, "{prefix}")?;
    if graph.is_commit_finished() {
        return Ok(());
    }
    loop {
        let mut row = String::new();
        graph.next_line(&mut row);
        out.write_all(row.as_bytes())?;
        if !graph.is_commit_finished() {
            out.write_all(b"\n")?;
            write!(out, "{prefix}")?;
        } else {
            break;
        }
    }
    Ok(())
}

/// Print `msg` line by line, with a graph row before every line but the first
/// (upstream `graph_show_strbuf`).
fn graph_show_strbuf(
    graph: &mut sley_rev::graph::Graph,
    prefix: &str,
    msg: &[u8],
    out: &mut dyn Write,
) -> Result<()> {
    let mut start = 0usize;
    while start < msg.len() {
        let end = msg[start..]
            .iter()
            .position(|&byte| byte == b'\n')
            .map(|pos| start + pos + 1)
            .unwrap_or(msg.len());
        out.write_all(&msg[start..end])?;
        let ended_with_newline = msg[end - 1] == b'\n';
        if ended_with_newline && end < msg.len() {
            graph_show_oneline(graph, prefix, out)?;
        }
        start = end;
    }
    Ok(())
}

/// Print the commit message followed by any remaining graph rows (upstream
/// `graph_show_commit_msg`).
fn graph_show_commit_msg(
    graph: &mut sley_rev::graph::Graph,
    prefix: &str,
    msg: &[u8],
    out: &mut dyn Write,
) -> Result<()> {
    graph_show_strbuf(graph, prefix, msg, out)?;
    let newline_terminated = msg.last() == Some(&b'\n');
    if !graph.is_commit_finished() {
        if !newline_terminated {
            out.write_all(b"\n")?;
        }
        graph_show_remainder(graph, prefix, out)?;
        if newline_terminated {
            out.write_all(b"\n")?;
        }
    }
    Ok(())
}

fn log_fatal_unrecognized_argument(value: &str) -> Result<()> {
    eprintln!("fatal: unrecognized argument: {value}");
    Err(GitError::Exit(128))
}

fn log_diff_merges_requires_value_error() -> GitError {
    eprintln!("fatal: Option '--diff-merges' requires a value");
    GitError::Exit(128)
}

fn log_max_age_requires_value_error() -> GitError {
    eprintln!("fatal: Option '--max-age' requires a value");
    GitError::Exit(128)
}

fn log_min_age_requires_value_error() -> GitError {
    eprintln!("fatal: Option '--min-age' requires a value");
    GitError::Exit(128)
}

fn log_date_cutoff_requires_value_error(option: &str) -> GitError {
    eprintln!("fatal: Option '{option}' requires a value");
    GitError::Exit(128)
}

fn log_no_walk_invalid_argument(value: &str) -> Result<()> {
    eprintln!("error: invalid argument to --no-walk");
    eprintln!("fatal: unrecognized argument: {value}");
    Err(GitError::Exit(128))
}

fn log_parse_parent_count(value: &str) -> Result<usize> {
    value.parse::<usize>().map_err(|_| {
        eprintln!("fatal: '{value}': not an integer");
        GitError::Exit(128)
    })
}

fn log_parse_abbrev_width(value: &str) -> usize {
    value.parse::<usize>().unwrap_or(0).max(4)
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum LogDefaultKind {
    /// `medium` (the default): includes the `Date:` line.
    Medium,
    /// `--pretty=short`: omits the `Date:` line.
    Short,
}

#[derive(Debug, Clone)]
enum LogOutput {
    /// `short`/`medium` structured layout.
    Default(LogDefaultKind),
    /// `--oneline`, `--pretty=oneline`, or `--format=` resolved to a compiled stream.
    Compiled {
        compiled: CompiledLogFormat,
        final_newline: bool,
        show_children: bool,
        /// When true, `--children` oids are printed between the commit oid and subject
        /// (oneline presets only; custom `--format=` ignores `--children`).
        inline_children: bool,
    },
}

fn log_output_needs_abbrev(
    output: &LogOutput,
    abbrev_commit: bool,
    show_children: bool,
) -> bool {
    match output {
        LogOutput::Default(_) => abbrev_commit,
        LogOutput::Compiled { compiled, .. } => {
            show_children
                || compiled.tokens.iter().any(|token| {
                    matches!(
                        token,
                        FormatToken::OidAbbrev
                            | FormatToken::TreeAbbrev
                            | FormatToken::ParentsAbbrev
                    )
                })
        }
    }
}

fn log_age_filters_match(
    record: &sley_rev::CommitRecord,
    max_age: Option<i64>,
    min_age: Option<i64>,
) -> Result<bool> {
    if max_age.is_none() && min_age.is_none() {
        return Ok(true);
    }
    let timestamp = commit_identity_timestamp_i64(&record.commit.committer)?;
    Ok(max_age.is_none_or(|age| timestamp >= age) && min_age.is_none_or(|age| timestamp <= age))
}

fn print_log_selected_parent_oids(
    record: &sley_rev::CommitRecord,
    show_parents: bool,
    abbrev_len: Option<usize>,
) {
    if show_parents {
        for parent in &record.parents {
            print!(" {}", format_log_oid(parent, abbrev_len));
        }
    }
}

fn print_log_selected_child_oids(
    record: &sley_rev::CommitRecord,
    child_oids: &HashMap<ObjectId, Vec<ObjectId>>,
    show_children: bool,
    abbrev_len: Option<usize>,
) {
    if show_children && let Some(children) = child_oids.get(&record.oid) {
        for child in children {
            print!(" {}", format_log_oid(child, abbrev_len));
        }
    }
}

fn print_log_format_with_children(
    record: &sley_rev::CommitRecord,
    compiled: &CompiledLogFormat,
    context: LogFormatContext<'_>,
    child_oids: &HashMap<ObjectId, Vec<ObjectId>>,
    abbrev_len: Option<usize>,
) -> Result<()> {
    let subject_index = compiled
        .tokens
        .iter()
        .position(|token| matches!(token, FormatToken::Subject | FormatToken::SanitizedSubject));
    let child_abbrev_len = if compiled.tokens.contains(&FormatToken::OidFull) {
        None
    } else {
        abbrev_len
    };
    let Some(subject_index) = subject_index else {
        print_log_format(record, compiled, context)?;
        print_log_selected_child_oids(record, child_oids, true, child_abbrev_len);
        return Ok(());
    };
    let mut pre_subject_end = subject_index;
    while pre_subject_end > 0
        && matches!(
            compiled.tokens[pre_subject_end - 1],
            FormatToken::Literal(ref text) if text.chars().all(char::is_whitespace)
        )
    {
        pre_subject_end -= 1;
    }
    let mut line = Vec::with_capacity(compiled.estimated_line_capacity());
    emit_compiled_log_format(record, compiled, &context, &mut line, 0..pre_subject_end)?;
    io::stdout().write_all(&line)?;
    print_log_selected_child_oids(record, child_oids, true, child_abbrev_len);
    if pre_subject_end < subject_index {
        io::stdout().write_all(b" ")?;
    }
    line.clear();
    emit_compiled_log_format(
        record,
        compiled,
        &context,
        &mut line,
        subject_index..compiled.tokens.len(),
    )?;
    io::stdout().write_all(&line)?;
    io::stdout().flush()?;
    Ok(())
}
