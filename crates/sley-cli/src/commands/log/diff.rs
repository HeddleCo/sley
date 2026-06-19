use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LogDiffMerges {
    /// Merges show no diff (the porcelain default).
    Off,
    /// Merges diff against their first parent (`--diff-merges=first-parent`,
    /// and the default under `--first-parent`).
    FirstParent,
    /// Combined merge diff: `-c` (`dense=false`) / `--cc` (`dense=true`).
    Combined { dense: bool },
}

/// Parse a `--diff-merges=<value>` into the supported modes.
pub(super) fn log_parse_diff_merges(value: &str) -> Result<LogDiffMerges> {
    match value {
        "off" | "none" => Ok(LogDiffMerges::Off),
        // "on"/"m" follow the diff-merges default (separate); sley renders the
        // first-parent diff for these until the separate mode lands.
        "first-parent" | "1" | "on" | "separate" | "m" => Ok(LogDiffMerges::FirstParent),
        "combined" | "c" => Ok(LogDiffMerges::Combined { dense: false }),
        "dense-combined" | "cc" => Ok(LogDiffMerges::Combined { dense: true }),
        "" => {
            eprintln!("fatal: invalid value for '--diff-merges': '{value}'");
            Err(GitError::Exit(128))
        }
        "remerge" | "r" => Err(GitError::Command(format!(
            "unsupported log option --diff-merges={value}"
        ))),
        _ => {
            eprintln!("fatal: invalid value for '--diff-merges': '{value}'");
            Err(GitError::Exit(128))
        }
    }
}

/// Parse a `log.diffMerges` config value into a [`LogDiffMerges`] default.
/// git's `diff_merges_config` rejects an unknown value, which `git log`
/// surfaces as a fatal config error (exit 128).
pub(super) fn log_parse_diff_merges_config(value: &str) -> Result<LogDiffMerges> {
    match value {
        "off" | "none" => Ok(LogDiffMerges::Off),
        "first-parent" | "1" | "on" | "separate" | "m" => Ok(LogDiffMerges::FirstParent),
        "combined" | "c" => Ok(LogDiffMerges::Combined { dense: false }),
        "dense-combined" | "cc" => Ok(LogDiffMerges::Combined { dense: true }),
        _ => {
            eprintln!("fatal: bad config variable 'log.diffMerges'");
            Err(GitError::Exit(128))
        }
    }
}

/// Diff-output options accepted by `git log` (`-p`, `--stat`, `--raw`, ...).
#[derive(Debug, Clone)]
pub(super) struct LogDiffOptions {
    pub(super) patch: bool,
    pub(super) stat: bool,
    pub(super) raw: bool,
    pub(super) name_only: bool,
    pub(super) name_status: bool,
    pub(super) numstat: bool,
    pub(super) shortstat: bool,
    pub(super) summary: bool,
    pub(super) compact_summary: bool,
    pub(super) stat_widths: DiffStatWidths,
    pub(super) stat_count: Option<usize>,
    pub(super) merges: Option<LogDiffMerges>,
    /// Whether an explicit `--diff-merges=<mode>` was given: unlike `-m`, the
    /// explicit form enables patch output for merge commits on its own.
    pub(super) merges_imply_patch: bool,
    /// Whitespace-ignore flags (`-w`, `-b`, `--ignore-space-at-eol`,
    /// `--ignore-cr-at-eol`).
    pub(super) ws_ignore: sley_diff_merge::WsIgnore,
    /// The line-diff algorithm (`--patience` / `--histogram` / Myers default).
    pub(super) diff_algorithm: sley_diff_merge::DiffAlgorithm,
    /// `--ignore-blank-lines`.
    pub(super) ignore_blank_lines: bool,
    /// Compiled `-I<regex>` (`--ignore-matching-lines`) patterns.
    pub(super) ignore_regexes: Vec<crate::grep_source::Regex>,
    /// `-a`/`--text`: treat all files as text (affects `-G` binary skipping).
    pub(super) text: bool,
    /// Resolved `--indent-heuristic` / `diff.indentHeuristic` (default
    /// git-enabled).
    pub(super) indent_heuristic: bool,
}

impl Default for LogDiffOptions {
    fn default() -> Self {
        LogDiffOptions {
            patch: false,
            stat: false,
            raw: false,
            name_only: false,
            name_status: false,
            numstat: false,
            shortstat: false,
            summary: false,
            compact_summary: false,
            stat_widths: DiffStatWidths::terminal(),
            stat_count: None,
            merges: None,
            merges_imply_patch: false,
            ws_ignore: sley_diff_merge::WsIgnore::default(),
            diff_algorithm: sley_diff_merge::DiffAlgorithm::Myers,
            ignore_blank_lines: false,
            ignore_regexes: Vec::new(),
            text: false,
            indent_heuristic: true,
        }
    }
}

impl LogDiffOptions {
    /// The bytes separating a commit's message from its diff block in the
    /// default output: `---` when a diffstat accompanies the patch
    /// (`--patch-with-stat`), a blank line otherwise.
    pub(super) fn block_separator(&self) -> &'static [u8] {
        if self.patch && (self.stat || self.compact_summary) {
            b"---\n"
        } else {
            b"\n"
        }
    }

    /// Per-record variant of [`block_separator`](Self::block_separator). A
    /// combined merge always uses a blank-line separator before its block (git
    /// never prefixes a combined stat-then-patch block with `---`).
    pub(super) fn block_separator_for(&self, record: &sley_rev::CommitRecord) -> &'static [u8] {
        if record.commit.parents.len() > 1
            && matches!(self.merges, Some(LogDiffMerges::Combined { .. }))
        {
            return b"\n";
        }
        self.block_separator()
    }

    /// Whether any diff output was requested at all.
    pub(super) fn any(&self) -> bool {
        self.patch
            || self.stat
            || self.raw
            || self.name_only
            || self.name_status
            || self.numstat
            || self.shortstat
            || self.summary
            || self.compact_summary
    }
}

pub(super) struct LogDiffContext<'a> {
    pub(super) db: &'a FileObjectDatabase,
    pub(super) format: ObjectFormat,
    pub(super) config: &'a GitConfig,
    pub(super) opts: &'a LogDiffOptions,
    pub(super) merges: LogDiffMerges,
    pub(super) show_root: bool,
    pub(super) detect_renames: bool,
    pub(super) detect_copies: bool,
    pub(super) pathspec: Option<DiffPathspec>,
    pub(super) patch_abbrev: usize,
    pub(super) raw_abbrev: Option<usize>,
}

impl LogDiffContext<'_> {
    /// Render the diff block for one commit (against its first parent, or the
    /// empty tree for roots when log.showRoot allows). Returns an empty buffer
    /// when nothing is to be shown; otherwise the buffer holds the block's
    /// lines WITHOUT a leading blank line (the caller owns separators, which
    /// differ between the default and oneline/format outputs).
    pub(super) fn render(
        &self,
        record: &sley_rev::CommitRecord,
        line_prefix_width: i64,
    ) -> Result<Vec<u8>> {
        // An explicit non-off --diff-merges without any diff-output option
        // shows patches for merge commits only.
        let merges_only = !self.opts.any();
        if merges_only && (record.commit.parents.len() <= 1 || self.merges == LogDiffMerges::Off) {
            return Ok(Vec::new());
        }
        let parents = &record.commit.parents;
        // A combined merge takes a separate render path (the result diffed
        // against every parent at once). Detect it before the two-tree setup.
        if parents.len() > 1
            && let LogDiffMerges::Combined { dense } = self.merges
        {
            return self.render_combined_merge(record, dense, line_prefix_width);
        }
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
                // Handled above.
                LogDiffMerges::Combined { .. } => unreachable!(),
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
        if opts.name_status {
            for entry in &entries {
                writeln!(out, "{}", entry.line())?;
            }
        }
        if opts.name_only {
            for entry in &entries {
                writeln!(
                    out,
                    "{}",
                    String::from_utf8_lossy(entry.path.as_bytes())
                )?;
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
                || opts.name_status
                || opts.name_only
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
                        submodule_format: commands::diff_options::SubmoduleDiffFormat::Short,
                        submodule_dirt: None,
                        ws_error: None,
                        interhunk: 0,
                        ws_ignore: self.opts.ws_ignore,
                        diff_algorithm: self.opts.diff_algorithm,
                        ignore_blank_lines: self.opts.ignore_blank_lines,
                        ignore_regexes: &self.opts.ignore_regexes,
                        line_ranges: None,
                        indent_heuristic: self.opts.indent_heuristic,
                    },
                )?;
            }
        }
        Ok(out)
    }

    /// Render the combined merge diff (`log -c`/`--cc`) for one merge commit:
    /// the first-parent stat family (when requested) followed by the combined
    /// patch. Returns the block bytes WITHOUT a leading blank line (the caller
    /// owns separators).
    fn render_combined_merge(
        &self,
        record: &sley_rev::CommitRecord,
        dense: bool,
        line_prefix_width: i64,
    ) -> Result<Vec<u8>> {
        let opts = self.opts;
        let merges_only = !opts.any();
        let patch = opts.patch || merges_only;
        let result_tree = &record.commit.tree;
        let parent_trees = record
            .commit
            .parents
            .iter()
            .map(|parent| self.parent_tree(parent))
            .collect::<Result<Vec<_>>>()?;

        // Stat family is computed against the first parent (STAT_FORMAT_MASK).
        let stat_active =
            opts.raw
                || opts.name_status
                || opts.name_only
                || opts.numstat
                || opts.stat
                || opts.compact_summary
                || opts.shortstat
                || opts.summary;
        let first_parent_entries = if stat_active {
            let base = sley_diff_merge::DiffNameStatusOptions {
                detect_renames: self.detect_renames,
                detect_copies: self.detect_copies,
                find_copies_harder: false,
                rename_empty: true,
            };
            sley_diff_merge::diff_name_status_trees_with_options(
                self.db,
                self.format,
                &parent_trees[0],
                result_tree,
                base,
            )?
        } else {
            Vec::new()
        };

        let paths =
            commands::combined::combined_paths(self.db, self.format, result_tree, &parent_trees)?;
        if paths.is_empty() && first_parent_entries.is_empty() {
            return Ok(Vec::new());
        }

        let mut out: Vec<u8> = Vec::new();
        if opts.raw {
            // git shows combined raw (`::`) for log --raw on a combined merge.
            let render_ctx = self.combined_ctx(dense);
            for path in &paths {
                commands::combined::write_combined_raw(&mut out, &render_ctx, path, false)?;
            }
        }
        if opts.name_status {
            for path in &paths {
                commands::combined::write_combined_name_status(&mut out, path, false)?;
            }
        }
        if opts.name_only {
            for path in &paths {
                writeln!(
                    out,
                    "{}",
                    String::from_utf8_lossy(&path.path)
                )?;
            }
        }
        if opts.numstat {
            for entry in &first_parent_entries {
                write_diff_numstat_entry(&mut out, entry, false, self.db, None, false)?;
            }
        }
        if opts.stat || opts.compact_summary {
            let mut widths = opts.stat_widths;
            widths.resolve_config(self.config);
            widths.line_prefix_width = line_prefix_width;
            write_diff_stat_with_widths(
                &mut out,
                &first_parent_entries,
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
            write_diff_shortstat(&mut out, &first_parent_entries, self.db, None, false)?;
        }
        if opts.summary {
            for entry in &first_parent_entries {
                write_diff_summary_entry(&mut out, entry)?;
            }
        }
        if patch && !paths.is_empty() {
            if stat_active {
                out.push(b'\n');
            }
            let render_ctx = self.combined_ctx(dense);
            for path in &paths {
                commands::combined::write_combined_patch(&mut out, &render_ctx, path)?;
            }
        }
        Ok(out)
    }

    /// Build the shared combined-render context for this log invocation.
    fn combined_ctx(&self, dense: bool) -> commands::combined::CombinedRenderCtx<'_> {
        commands::combined::CombinedRenderCtx {
            db: self.db,
            format: self.format,
            dense,
            all_paths: false,
            context: 3,
            ws_ignore: self.opts.ws_ignore,
            diff_algorithm: self.opts.diff_algorithm,
            src_prefix: "a/",
            dst_prefix: "b/",
            patch_abbrev: self.patch_abbrev,
            raw_abbrev: self.raw_abbrev,
        }
    }

    /// Tree oid of `parent`.
    fn parent_tree(&self, parent: &ObjectId) -> Result<ObjectId> {
        let object = self.db.read_object(parent)?;
        Ok(Commit::parse_ref(self.format, &object.body)?.tree)
    }
}

/// Display width of a line prefix, skipping ANSI SGR escapes (git
/// `utf8_strnwidth(..., skip_ansi=1)`).
pub(super) fn log_prefix_display_width(prefix: &str) -> i64 {
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
