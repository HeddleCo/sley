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
    /// `--notes=<ref>` / `--show-notes=<ref>`: enable and add a specific ref.
    fn add_ref(&mut self, reff: &str) {
        if self.use_default.is_none() {
            self.use_default = Some(true);
        }
        self.extra_refs
            .push(NotesRef::expand(reff).as_str().to_string());
        self.enabled = true;
        self.given = true;
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
            let default_ref = crate::commands::notes::raw_notes_ref(git_dir, None);
            push_unique(&mut refs, default_ref);
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
        for line in body.split(|b| *b == b'\n') {
            out.extend_from_slice(b"    ");
            out.extend_from_slice(line);
            out.push(b'\n');
        }
    }
    Ok(out)
}

pub(crate) fn cmd_log(args: &[String]) -> Result<()> {
    let mut includes = Vec::new();
    let mut excludes = Vec::new();
    let mut linear_ranges = Vec::new();
    let mut symmetric_ranges = Vec::new();
    let mut stdin_revisions = Vec::new();
    let mut default_revision = None;
    let mut max_count = None;
    let mut skip = 0usize;
    let mut output = LogOutput::Default(LogDefaultKind::Medium);
    let mut notes_display = NotesDisplay::default();
    let mut preset_oneline: Option<bool> = None;
    let mut reverse = false;
    let mut ordering = RevListOrdering::Default;
    let mut walk = true;
    let mut walk_reflogs = false;
    let mut max_age = None;
    let mut min_age = None;
    let mut first_parent = false;
    let mut min_parents = None;
    let mut max_parents = None;
    let mut show_parents = false;
    let mut show_children = false;
    let mut abbrev_commit = false;
    let mut abbrev_len = Some(7usize);
    let mut decoration = LogDecorationMode::Off;
    let mut all_refs = false;
    let mut branches = false;
    let mut branch_patterns = Vec::new();
    let mut tags = false;
    let mut tag_patterns = Vec::new();
    let mut remotes = false;
    let mut remote_patterns = Vec::new();
    let mut ref_selectors = Vec::new();
    let mut pending_ref_exclude_patterns = Vec::new();
    let mut pending_hidden_refs = None;
    let mut not = false;
    let mut read_stdin = false;
    let mut author_patterns = Vec::new();
    let mut committer_patterns = Vec::new();
    let mut grep_patterns = Vec::new();
    let mut grep_all_match = false;
    let mut invert_grep = false;
    let mut regexp_ignore_case = false;
    let mut regexp_mode = SimpleLogRegexMode::Basic;
    let mut date_mode = ForEachRefDateMode::Default;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--not" => not = !not,
            "--stdin" => read_stdin = true,
            "--default" => {
                default_revision = Some(
                    iter.next()
                        .ok_or_else(|| GitError::Command("--default requires a value".into()))?,
                );
            }
            "--reverse" => reverse = true,
            "--topo-order" => ordering = RevListOrdering::Topo,
            "--date-order" | "--author-date-order" => ordering = RevListOrdering::Date,
            "--first-parent" => first_parent = true,
            "--parents" => show_parents = true,
            "--children" => show_children = true,
            "--abbrev-commit" => abbrev_commit = true,
            "--no-abbrev-commit" => abbrev_commit = false,
            "--abbrev" => abbrev_len = Some(7),
            "--no-abbrev" => abbrev_len = None,
            "--all" => {
                if not || !pending_ref_exclude_patterns.is_empty() || pending_hidden_refs.is_some()
                {
                    ref_selectors.push(RevListRefSelector::All {
                        not,
                        excludes: mem::take(&mut pending_ref_exclude_patterns),
                        hidden: pending_hidden_refs.take(),
                    });
                } else {
                    all_refs = true;
                }
            }
            "--branches" => {
                if pending_hidden_refs.is_some() {
                    return rev_list_exclude_hidden_selector_error("--branches");
                }
                if not || !pending_ref_exclude_patterns.is_empty() {
                    ref_selectors.push(RevListRefSelector::Branches {
                        not,
                        patterns: Vec::new(),
                        include_all: true,
                        excludes: mem::take(&mut pending_ref_exclude_patterns),
                        hidden: pending_hidden_refs.take(),
                    });
                } else {
                    branches = true;
                }
            }
            value if value.starts_with("--branches=") => {
                let pattern = value["--branches=".len()..].to_string();
                if pending_hidden_refs.is_some() {
                    return rev_list_exclude_hidden_selector_error("--branches");
                }
                if not || !pending_ref_exclude_patterns.is_empty() {
                    ref_selectors.push(RevListRefSelector::Branches {
                        not,
                        patterns: vec![pattern],
                        include_all: false,
                        excludes: mem::take(&mut pending_ref_exclude_patterns),
                        hidden: pending_hidden_refs.take(),
                    });
                } else {
                    branch_patterns.push(pattern);
                }
            }
            "--tags" => {
                if pending_hidden_refs.is_some() {
                    return rev_list_exclude_hidden_selector_error("--tags");
                }
                if not || !pending_ref_exclude_patterns.is_empty() {
                    ref_selectors.push(RevListRefSelector::Tags {
                        not,
                        patterns: Vec::new(),
                        include_all: true,
                        excludes: mem::take(&mut pending_ref_exclude_patterns),
                        hidden: pending_hidden_refs.take(),
                    });
                } else {
                    tags = true;
                }
            }
            value if value.starts_with("--tags=") => {
                let pattern = value["--tags=".len()..].to_string();
                if pending_hidden_refs.is_some() {
                    return rev_list_exclude_hidden_selector_error("--tags");
                }
                if not || !pending_ref_exclude_patterns.is_empty() {
                    ref_selectors.push(RevListRefSelector::Tags {
                        not,
                        patterns: vec![pattern],
                        include_all: false,
                        excludes: mem::take(&mut pending_ref_exclude_patterns),
                        hidden: pending_hidden_refs.take(),
                    });
                } else {
                    tag_patterns.push(pattern);
                }
            }
            "--remotes" => {
                if pending_hidden_refs.is_some() {
                    return rev_list_exclude_hidden_selector_error("--remotes");
                }
                if not || !pending_ref_exclude_patterns.is_empty() {
                    ref_selectors.push(RevListRefSelector::Remotes {
                        not,
                        patterns: Vec::new(),
                        include_all: true,
                        excludes: mem::take(&mut pending_ref_exclude_patterns),
                        hidden: pending_hidden_refs.take(),
                    });
                } else {
                    remotes = true;
                }
            }
            value if value.starts_with("--remotes=") => {
                let pattern = value["--remotes=".len()..].to_string();
                if pending_hidden_refs.is_some() {
                    return rev_list_exclude_hidden_selector_error("--remotes");
                }
                if not || !pending_ref_exclude_patterns.is_empty() {
                    ref_selectors.push(RevListRefSelector::Remotes {
                        not,
                        patterns: vec![pattern],
                        include_all: false,
                        excludes: mem::take(&mut pending_ref_exclude_patterns),
                        hidden: pending_hidden_refs.take(),
                    });
                } else {
                    remote_patterns.push(pattern);
                }
            }
            "--exclude" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--exclude requires a value".into()))?;
                pending_ref_exclude_patterns.push(value.to_string());
            }
            value if value.starts_with("--exclude=") => {
                pending_ref_exclude_patterns.push(value["--exclude=".len()..].to_string());
            }
            "--glob" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--glob requires a value".into()))?;
                ref_selectors.push(RevListRefSelector::Glob {
                    not,
                    pattern: value.to_string(),
                    excludes: mem::take(&mut pending_ref_exclude_patterns),
                    hidden: pending_hidden_refs.take(),
                });
            }
            value if value.starts_with("--glob=") => {
                ref_selectors.push(RevListRefSelector::Glob {
                    not,
                    pattern: value["--glob=".len()..].to_string(),
                    excludes: mem::take(&mut pending_ref_exclude_patterns),
                    hidden: pending_hidden_refs.take(),
                });
            }
            "--exclude-hidden" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--exclude-hidden requires a value".into()))?;
                pending_hidden_refs = Some(parse_rev_list_exclude_hidden(value)?);
            }
            value if value.starts_with("--exclude-hidden=") => {
                pending_hidden_refs = Some(parse_rev_list_exclude_hidden(
                    &value["--exclude-hidden=".len()..],
                )?);
            }
            "--author" => {
                let value = iter.next().ok_or_else(log_author_requires_value_error)?;
                author_patterns.push(LogFilterPattern::new(value, "header"));
            }
            value if value.starts_with("--author=") => {
                author_patterns.push(LogFilterPattern::new(&value["--author=".len()..], "header"));
            }
            "--committer" => {
                let value = iter.next().ok_or_else(log_committer_requires_value_error)?;
                committer_patterns.push(LogFilterPattern::new(value, "header"));
            }
            value if value.starts_with("--committer=") => {
                committer_patterns.push(LogFilterPattern::new(
                    &value["--committer=".len()..],
                    "header",
                ));
            }
            "--grep" => {
                let value = iter.next().ok_or_else(log_grep_requires_value_error)?;
                grep_patterns.push(LogFilterPattern::new(value, "command line"));
            }
            value if value.starts_with("--grep=") => {
                grep_patterns.push(LogFilterPattern::new(
                    &value["--grep=".len()..],
                    "command line",
                ));
            }
            "--all-match" => grep_all_match = true,
            "--invert-grep" => invert_grep = true,
            "-i" | "--regexp-ignore-case" => regexp_ignore_case = true,
            "-F" | "--fixed-strings" => regexp_mode = SimpleLogRegexMode::Fixed,
            "-E" | "--basic-regexp" | "--extended-regexp" => {
                regexp_mode = SimpleLogRegexMode::Basic
            }
            "--do-walk" => walk = true,
            "--no-walk" => walk = false,
            "-g" | "--walk-reflogs" => walk_reflogs = true,
            "--no-walk-reflogs" => walk_reflogs = false,
            "--max-age" => {
                let value = iter.next().ok_or_else(log_max_age_requires_value_error)?;
                max_age = Some(log_parse_age(value)?);
            }
            value if value.starts_with("--max-age=") => {
                max_age = Some(log_parse_age(&value["--max-age=".len()..])?);
            }
            "--min-age" => {
                let value = iter.next().ok_or_else(log_min_age_requires_value_error)?;
                min_age = Some(log_parse_age(value)?);
            }
            value if value.starts_with("--min-age=") => {
                min_age = Some(log_parse_age(&value["--min-age=".len()..])?);
            }
            "--since" | "--after" => {
                let value = iter
                    .next()
                    .ok_or_else(|| log_date_cutoff_requires_value_error(arg))?;
                max_age = Some(log_parse_date_cutoff(value)?);
            }
            value if value.starts_with("--since=") => {
                max_age = Some(log_parse_date_cutoff(&value["--since=".len()..])?);
            }
            value if value.starts_with("--after=") => {
                max_age = Some(log_parse_date_cutoff(&value["--after=".len()..])?);
            }
            "--until" | "--before" => {
                let value = iter
                    .next()
                    .ok_or_else(|| log_date_cutoff_requires_value_error(arg))?;
                min_age = Some(log_parse_date_cutoff(value)?);
            }
            value if value.starts_with("--until=") => {
                min_age = Some(log_parse_date_cutoff(&value["--until=".len()..])?);
            }
            value if value.starts_with("--before=") => {
                min_age = Some(log_parse_date_cutoff(&value["--before=".len()..])?);
            }
            "--merges" => min_parents = Some(2),
            "--no-merges" => max_parents = Some(1),
            "--no-min-parents" => min_parents = None,
            "--no-max-parents" => max_parents = None,
            "-q"
            | "--quiet"
            | "--no-quiet"
            | "--sparse"
            | "--dense"
            | "--remove-empty"
            | "--unpacked"
            | "--full-history"
            | "--simplify-merges"
            | "--show-pulls"
            | "--no-source"
            | "--use-mailmap"
            | "--no-use-mailmap"
            | "--mailmap"
            | "--no-mailmap"
            | "--show-signature"
            | "--no-show-signature"
            | "--no-color"
            | "--color"
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
            | "--no-patch"
            | "--no-diff-merges"
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
            | "-m"
            | "-s"
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
                log_validate_diff_merges(value)?;
            }
            value if value.starts_with("--diff-merges=") => {
                log_validate_diff_merges(&value["--diff-merges=".len()..])?;
            }
            "--no-walk=sorted" | "--no-walk=unsorted" => walk = false,
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
            }
            value if value.starts_with("--date=") => {
                date_mode = log_date_mode(&value["--date=".len()..])?;
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
            value if value.starts_with("--color=") => {
                log_validate_color(&value["--color=".len()..])?;
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
                log_validate_output_indicator("output-indicator-new", value)?;
            }
            "--output-indicator-old" => {
                let value = iter
                    .next()
                    .ok_or_else(|| log_option_requires_value_error("output-indicator-old"))?;
                log_validate_output_indicator("output-indicator-old", value)?;
            }
            "--output-indicator-context" => {
                let value = iter
                    .next()
                    .ok_or_else(|| log_option_requires_value_error("output-indicator-context"))?;
                log_validate_output_indicator("output-indicator-context", value)?;
            }
            value if value.starts_with("--output-indicator-new=") => {
                log_validate_output_indicator(
                    "output-indicator-new",
                    &value["--output-indicator-new=".len()..],
                )?;
            }
            value if value.starts_with("--output-indicator-old=") => {
                log_validate_output_indicator(
                    "output-indicator-old",
                    &value["--output-indicator-old=".len()..],
                )?;
            }
            value if value.starts_with("--output-indicator-context=") => {
                log_validate_output_indicator(
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
                notes_display.add_ref(&value["--show-notes=".len()..]);
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
            "--oneline" => preset_oneline = Some(false),
            "--pretty=oneline" | "--format=oneline" => preset_oneline = Some(true),
            "--pretty=short" | "--format=short" => {
                output = LogOutput::Default(LogDefaultKind::Short)
            }
            "--pretty=medium" | "--format=medium" => {
                output = LogOutput::Default(LogDefaultKind::Medium)
            }
            "-n" | "--max-count" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command(format!("{arg} requires a value")))?;
                max_count = Some(parse_log_count(value)?);
            }
            value if value.starts_with("--max-count=") => {
                let value = value
                    .strip_prefix("--max-count=")
                    .ok_or_else(|| GitError::Command("--max-count requires a value".into()))?;
                max_count = Some(parse_log_count(value)?);
            }
            "--skip" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--skip requires a value".into()))?;
                skip = parse_log_count(value)?;
            }
            value if value.starts_with("--skip=") => {
                let value = value
                    .strip_prefix("--skip=")
                    .ok_or_else(|| GitError::Command("--skip requires a value".into()))?;
                skip = parse_log_count(value)?;
            }
            value if value.starts_with("--format=") => {
                output = LogOutput::Compiled {
                    compiled: CompiledLogFormat::compile(
                        &value["--format=".len()..],
                        LogFormatDialect::Log,
                    )?,
                    final_newline: true,
                    show_children: false,
                    inline_children: false,
                };
            }
            value if value.starts_with("--pretty=format:") => {
                output = LogOutput::Compiled {
                    compiled: CompiledLogFormat::compile(
                        &value["--pretty=format:".len()..],
                        LogFormatDialect::Log,
                    )?,
                    final_newline: false,
                    show_children: false,
                    inline_children: false,
                };
            }
            value if value.starts_with("-n") && value.len() > 2 => {
                max_count = Some(parse_log_count(&value[2..])?);
            }
            value
                if value.starts_with('-')
                    && value[1..].bytes().all(|byte| byte.is_ascii_digit()) =>
            {
                max_count = Some(parse_log_count(&value[1..])?);
            }
            value if value.starts_with('^') && value.len() > 1 => {
                if not {
                    includes.push(value[1..].to_string());
                } else {
                    excludes.push(value[1..].to_string());
                }
            }
            value if value.starts_with('-') => {
                return Err(GitError::Command(format!("unsupported log option {value}")));
            }
            value => add_rev_list_revision_arg(
                value,
                not,
                &mut includes,
                &mut excludes,
                &mut linear_ranges,
                &mut symmetric_ranges,
            )?,
        }
    }
    if read_stdin {
        let mut input = String::new();
        io::stdin().read_to_string(&mut input)?;
        stdin_revisions.extend(
            input
                .lines()
                .filter(|line| !line.is_empty())
                .map(str::to_string),
        );
        let mut stdin_not = false;
        for line in &stdin_revisions {
            if line == "--not" {
                stdin_not = !stdin_not;
                continue;
            }
            add_rev_list_revision_arg(
                line,
                stdin_not,
                &mut includes,
                &mut excludes,
                &mut linear_ranges,
                &mut symmetric_ranges,
            )?;
        }
    }
    if show_parents && show_children {
        eprintln!("fatal: options '--parents' and '--children' cannot be used together");
        return Err(GitError::Exit(128));
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
    let author_filters = parse_log_filter_patterns(&author_patterns, regexp_mode)?;
    let committer_filters = parse_log_filter_patterns(&committer_patterns, regexp_mode)?;
    let grep_filters = parse_log_filter_patterns(&grep_patterns, regexp_mode)?;
    let git_dir = discover_git_dir(env::current_dir()?)?;
    let format = repository_object_format(&git_dir)?;
    let config = read_repo_config(&git_dir)?;
    let hidden_refs = RevListHiddenRefs::from_config(&config);
    let db = FileObjectDatabase::from_git_dir(&git_dir, format);
    let has_ref_selectors = all_refs
        || branches
        || !branch_patterns.is_empty()
        || tags
        || !tag_patterns.is_empty()
        || remotes
        || !remote_patterns.is_empty()
        || !ref_selectors.is_empty();
    if includes.is_empty()
        && excludes.is_empty()
        && linear_ranges.is_empty()
        && symmetric_ranges.is_empty()
        && !has_ref_selectors
        && let Some(default_revision) = default_revision
    {
        add_rev_list_revision_arg(
            default_revision,
            false,
            &mut includes,
            &mut excludes,
            &mut linear_ranges,
            &mut symmetric_ranges,
        )?;
    }
    if includes.is_empty()
        && excludes.is_empty()
        && linear_ranges.is_empty()
        && symmetric_ranges.is_empty()
        && !has_ref_selectors
    {
        includes.push("HEAD".to_string());
    }
    if walk_reflogs {
        return log_walk_reflogs(
            &git_dir, format, &includes, max_count, skip, &output, reverse,
        );
    }
    let log_format_source = if !has_ref_selectors
        && includes.len() == 1
        && linear_ranges.is_empty()
        && symmetric_ranges.is_empty()
    {
        Some(includes[0].to_string())
    } else {
        None
    };
    let mut starts = Vec::new();
    for rev in includes {
        let start = resolve_revision(&git_dir, format, &rev)?;
        starts.push(sley_rev::peel_to_commit(&db, format, &start)?);
    }
    let mut symmetric_excludes = Vec::new();
    for (left, right, not) in linear_ranges {
        let left_oid = resolve_revision(&git_dir, format, &left)?;
        let left_oid = sley_rev::peel_to_commit(&db, format, &left_oid)?;
        let right_oid = resolve_revision(&git_dir, format, &right)?;
        let right_oid = sley_rev::peel_to_commit(&db, format, &right_oid)?;
        if not {
            starts.push(left_oid);
            symmetric_excludes.push(right_oid);
        } else {
            symmetric_excludes.push(left_oid);
            starts.push(right_oid);
        }
    }
    for (left, right, not) in symmetric_ranges {
        let left_oid = resolve_revision(&git_dir, format, &left)?;
        let left_oid = sley_rev::peel_to_commit(&db, format, &left_oid)?;
        let right_oid = resolve_revision(&git_dir, format, &right)?;
        let right_oid = sley_rev::peel_to_commit(&db, format, &right_oid)?;
        let merge_bases = merge_bases(&db, format, &left_oid, &right_oid)?;
        if not {
            starts.extend(merge_bases);
            symmetric_excludes.push(left_oid);
            symmetric_excludes.push(right_oid);
        } else {
            starts.push(left_oid);
            starts.push(right_oid);
            symmetric_excludes.extend(merge_bases);
        }
    }
    if has_ref_selectors {
        let store = FileRefStore::new(&git_dir, format);
        for reference in store.list_refs()? {
            let (selector_include, selector_exclude) =
                rev_list_ref_selection(&reference.name, &ref_selectors, &hidden_refs);
            let include_ref = all_refs
                || rev_list_ref_selector_matches(
                    &reference.name,
                    "refs/heads/",
                    branches,
                    &branch_patterns,
                )
                || rev_list_ref_selector_matches(
                    &reference.name,
                    "refs/tags/",
                    tags,
                    &tag_patterns,
                )
                || rev_list_ref_selector_matches(
                    &reference.name,
                    "refs/remotes/",
                    remotes,
                    &remote_patterns,
                )
                || selector_include;
            if !include_ref && !selector_exclude {
                continue;
            }
            let RefTarget::Direct(oid) = reference.target else {
                continue;
            };
            if let Ok(commit) = sley_rev::peel_to_commit(&db, format, &oid) {
                if include_ref {
                    starts.push(commit);
                }
                if selector_exclude {
                    symmetric_excludes.push(commit);
                }
            }
        }
    }
    let mut excluded = HashSet::new();
    for oid in symmetric_excludes {
        for record in rev_list_walk_commits(&db, format, [oid], first_parent)? {
            excluded.insert(record.oid);
        }
    }
    for rev in excludes {
        let oid = resolve_revision(&git_dir, format, &rev)?;
        let oid = sley_rev::peel_to_commit(&db, format, &oid)?;
        for record in rev_list_walk_commits(&db, format, [oid], first_parent)? {
            excluded.insert(record.oid);
        }
    }
    if walk
        && matches!(ordering, RevListOrdering::Default | RevListOrdering::Date)
        && matches!(&output, LogOutput::Compiled { compiled, show_children: false, .. }
            if compiled.is_metadata_emitable() && compiled.uses_oid() && !compiled.uses_decorations())
        && decoration == LogDecorationMode::Off
        && !show_children
        && author_filters.is_empty()
        && committer_filters.is_empty()
        && grep_filters.is_empty()
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
        let compiled = match &output {
            LogOutput::Compiled { compiled, .. } => compiled,
            _ => unreachable!("metadata fast path requires compiled output"),
        };
        let mut stdout = io::stdout();
        for record in &selected {
            let mut line = Vec::with_capacity(compiled.estimated_line_capacity());
            emit_compiled_log_format_metadata(
                record,
                compiled,
                &LogFormatContext {
                    abbrev_len,
                    decorations: &HashMap::new(),
                    marker: '>',
                    dialect: LogFormatDialect::Log,
                    source: log_format_source.as_deref(),
                    date_mode,
                },
                &mut line,
            )?;
            stdout.write_all(&line)?;
            stdout.write_all(b"\n")?;
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
            || !log_author_filters_match(record, &author_filters, regexp_ignore_case)
            || !log_committer_filters_match(record, &committer_filters, regexp_ignore_case)
            || !log_grep_filters_match(
                record,
                &grep_filters,
                grep_all_match,
                invert_grep,
                regexp_ignore_case,
            )
        {
            continue;
        }
        selected.push(record);
    }
    selected = match ordering {
        RevListOrdering::Default if walk => rev_list_date_order(selected)?,
        RevListOrdering::Default => selected,
        RevListOrdering::Topo => rev_list_topo_order(selected),
        RevListOrdering::Date => rev_list_date_order(selected)?,
    };
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

    for (index, record) in selected.iter().enumerate() {
        match output {
            LogOutput::Default(kind) => {
                if index > 0 {
                    println!();
                }
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
                println!("Author: {}", commit_author_identity(&record.commit.author));
                if kind == LogDefaultKind::Medium {
                    println!(
                        "Date:   {}",
                        commit_identity_date(&record.commit.author, date_mode)
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
            }
            LogOutput::Compiled {
                ref compiled,
                final_newline,
                show_children: compiled_children,
                inline_children,
            } => {
                if index > 0 && !final_newline {
                    println!();
                }
                let format_context = LogFormatContext {
                    abbrev_len,
                    decorations: &decorations,
                    marker: '>',
                    dialect: LogFormatDialect::Log,
                    source: log_format_source.as_deref(),
                    date_mode,
                };
                if compiled_children && inline_children {
                    print_log_format_with_children(
                        record,
                        compiled,
                        format_context,
                        &child_oids,
                        abbrev_len,
                    )?;
                } else {
                    print_log_format(record, compiled, format_context)?;
                }
                if final_newline {
                    println!();
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

fn log_fatal_unrecognized_argument(value: &str) -> Result<()> {
    eprintln!("fatal: unrecognized argument: {value}");
    Err(GitError::Exit(128))
}

fn log_diff_merges_requires_value_error() -> GitError {
    eprintln!("fatal: Option '--diff-merges' requires a value");
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
    let child_abbrev_len = if compiled
        .tokens
        .iter()
        .any(|token| *token == FormatToken::OidFull)
    {
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
