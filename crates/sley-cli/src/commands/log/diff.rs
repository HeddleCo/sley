use super::*;
use sley::plumbing::{sley_diff_merge, sley_rev};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LogDiffMerges {
    /// Merges show no diff (the porcelain default).
    Off,
    /// Merges diff against their first parent (`--diff-merges=first-parent`,
    /// and the default under `--first-parent`).
    FirstParent,
    /// Merges are shown once per parent (`-m`, `--diff-merges=separate`).
    Separate,
    /// Combined merge diff: `-c` (`dense=false`) / `--cc` (`dense=true`).
    Combined { dense: bool },
    /// Re-merge the parents and diff that against the commit (`--remerge-diff`).
    Remerge,
}

/// Parse a `--diff-merges=<value>` into the supported modes.
pub(super) fn log_parse_diff_merges(value: &str) -> Result<LogDiffMerges> {
    match value {
        "off" | "none" => Ok(LogDiffMerges::Off),
        "first-parent" | "1" => Ok(LogDiffMerges::FirstParent),
        "on" | "separate" | "m" => Ok(LogDiffMerges::Separate),
        "combined" | "c" => Ok(LogDiffMerges::Combined { dense: false }),
        "dense-combined" | "cc" => Ok(LogDiffMerges::Combined { dense: true }),
        "remerge" | "r" => Ok(LogDiffMerges::Remerge),
        "" => {
            eprintln!("fatal: invalid value for '--diff-merges': '{value}'");
            Err(GitError::Exit(128))
        }
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
        "first-parent" | "1" => Ok(LogDiffMerges::FirstParent),
        "on" | "separate" | "m" => Ok(LogDiffMerges::Separate),
        "combined" | "c" => Ok(LogDiffMerges::Combined { dense: false }),
        "dense-combined" | "cc" => Ok(LogDiffMerges::Combined { dense: true }),
        _ => {
            eprintln!("fatal: bad config variable 'log.diffMerges'");
            Err(GitError::Exit(128))
        }
    }
}

/// Diff-output options accepted by `git log` (`-p`, `--stat`, `--raw`, ...).
#[derive(Clone)]
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
    pub(super) ignore_regexes: Vec<sley_grep::Regex>,
    /// `--word-diff[=<mode>]` rendering request.
    pub(super) word_diff_mode: Option<commands::diff_words::WordDiffMode>,
    /// `--word-diff-regex=<regex>` override.
    pub(super) word_diff_regex: Option<String>,
    /// Hunk body line indicators (` `, `-`, `+` by default).
    pub(super) line_indicators: sley_diff_merge::render::LineIndicators,
    /// Unified diff context (`-U<n>` / `diff.context`), resolved before render.
    pub(super) context: Option<usize>,
    /// `-a`/`--text`: treat all files as text (affects `-G` binary skipping).
    pub(super) text: bool,
    /// `-O<file>`: reorder diff entries according to an orderfile.
    pub(super) order_file: Option<String>,
    /// `--rotate-to=<path>` / `--skip-to=<path>`: rotate (or, with `rotate_skip`,
    /// drop) each commit's path-sorted diff so it begins at `<path>`. `git log`
    /// is non-strict: a target naming no diffed path pivots at the first path
    /// lexically `>=` it and silently no-ops when none qualifies.
    pub(super) rotate_to: Option<String>,
    /// `true` when the rotate request came from `--skip-to` rather than
    /// `--rotate-to`.
    pub(super) rotate_skip: bool,
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
            word_diff_mode: None,
            word_diff_regex: None,
            line_indicators: sley_diff_merge::render::LineIndicators::default(),
            context: None,
            text: false,
            order_file: None,
            rotate_to: None,
            rotate_skip: false,
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
    /// Repository git dir (needed for remerge-diff merge-base walks).
    pub(super) git_dir: &'a Path,
    /// Resolves `diff.<driver>.textconv` (and binary/funcname drivers) for the
    /// `-p` patch path so `git log -p` honors textconv like `git diff` does.
    pub(super) userdiff: &'a commands::userdiff::UserdiffResolver,
    pub(super) opts: &'a LogDiffOptions,
    pub(super) merges: LogDiffMerges,
    pub(super) show_root: bool,
    pub(super) detect_renames: bool,
    pub(super) detect_copies: bool,
    /// `--find-copies-harder` / implied by `--follow` (git always sets
    /// `find_copies_harder` in `try_to_follow_renames`).
    pub(super) find_copies_harder: bool,
    pub(super) pathspec: Option<DiffPathspec>,
    /// When `--follow` rewound the path across renames/copies, map each
    /// selected commit oid to the pathspec that was active for that commit.
    /// Entries are filtered against this path (and its rename/copy source)
    /// instead of the original CLI pathspec.
    pub(super) follow_paths: Option<&'a HashMap<ObjectId, Vec<u8>>>,
    pub(super) patch_abbrev: usize,
    pub(super) raw_abbrev: Option<usize>,
    pub(super) pickaxe: Option<&'a CompiledPickaxe>,
    pub(super) pickaxe_ignore_case: bool,
    pub(super) pickaxe_text: bool,
    pub(super) pickaxe_all: bool,
    pub(super) lazy_fetch: bool,
}

impl LogDiffContext<'_> {
    /// Filter name-status entries by the active pathspec. Under `--follow`, use
    /// the per-commit followed path (after rename/copy rewinds) so older commits
    /// that only touch the pre-rename path still render.
    fn filter_diff_entries(
        &self,
        entries: Vec<sley_diff_merge::NameStatusEntry>,
        oid: &ObjectId,
    ) -> Vec<sley_diff_merge::NameStatusEntry> {
        if let Some(follow_paths) = self.follow_paths
            && let Some(path) = follow_paths.get(oid)
        {
            return entries
                .into_iter()
                .filter(|entry| {
                    entry.path.as_bytes() == path.as_slice()
                        || entry
                            .old_path
                            .as_ref()
                            .is_some_and(|old| old.as_bytes() == path.as_slice())
                })
                .collect();
        }
        match &self.pathspec {
            Some(pathspec) => apply_diff_pathspec(entries, pathspec),
            None => entries,
        }
    }

    /// Render the diff block for one commit (against its first parent, or the
    /// empty tree for roots when log.showRoot allows). Returns an empty buffer
    /// when nothing is to be shown; otherwise the buffer holds the block's
    /// lines WITHOUT a leading blank line (the caller owns separators, which
    /// differ between the default and oneline/format outputs).
    pub(super) fn render(
        &self,
        record: &sley_rev::CommitRecord,
        line_prefix_width: i64,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        self.render_against_parent(record, None, line_prefix_width, out)
    }

    pub(super) fn separate_parent_count(&self, record: &sley_rev::CommitRecord) -> Option<usize> {
        (record.commit.parents.len() > 1 && self.merges == LogDiffMerges::Separate)
            .then_some(record.commit.parents.len())
    }

    pub(super) fn render_parent(
        &self,
        record: &sley_rev::CommitRecord,
        parent_index: usize,
        line_prefix_width: i64,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        self.render_against_parent(record, Some(parent_index), line_prefix_width, out)
    }

    fn render_against_parent(
        &self,
        record: &sley_rev::CommitRecord,
        parent_index: Option<usize>,
        line_prefix_width: i64,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        out.clear();
        // An explicit non-off --diff-merges without any diff-output option
        // shows patches for merge commits only.
        let merges_only = !self.opts.any();
        if merges_only && (record.commit.parents.len() <= 1 || self.merges == LogDiffMerges::Off) {
            return Ok(());
        }
        let parents = &record.commit.parents;
        // A combined merge takes a separate render path (the result diffed
        // against every parent at once). Detect it before the two-tree setup.
        if parents.len() > 1
            && parent_index.is_none()
            && let LogDiffMerges::Combined { dense } = self.merges
        {
            return self.render_combined_merge(record, dense, line_prefix_width, out);
        }
        if parents.len() > 1 && parent_index.is_none() && self.merges == LogDiffMerges::Remerge {
            return self.render_remerge_diff(record, line_prefix_width, out);
        }
        let parent_tree = if let Some(parent_index) = parent_index {
            let Some(parent) = parents.get(parent_index) else {
                return Ok(());
            };
            Some(self.parent_tree(parent)?)
        } else {
            match parents.len() {
                0 => {
                    if !self.show_root {
                        return Ok(());
                    }
                    None
                }
                1 => Some(self.parent_tree(&parents[0])?),
                _ => match self.merges {
                    LogDiffMerges::Off => return Ok(()),
                    LogDiffMerges::FirstParent | LogDiffMerges::Separate => {
                        Some(self.parent_tree(&parents[0])?)
                    }
                    // Handled above.
                    LogDiffMerges::Combined { .. } | LogDiffMerges::Remerge => unreachable!(),
                },
            }
        };
        let base = sley_diff_merge::DiffNameStatusOptions {
            detect_renames: self.detect_renames,
            detect_copies: self.detect_copies,
            find_copies_harder: self.find_copies_harder,
            rename_empty: true,
            ..Default::default()
        };
        let rename_options = sley_diff_merge::DiffNameStatusOptions {
            detect_renames: self.detect_renames,
            detect_copies: self.detect_copies,
            find_copies_harder: self.find_copies_harder,
            rename_empty: true,
            detect_inexact: true,
            rename_threshold: sley_diff_merge::DEFAULT_RENAME_THRESHOLD,
            copy_threshold: sley_diff_merge::DEFAULT_RENAME_THRESHOLD,
            rename_limit: 0,
            ..Default::default()
        };
        let tree = &record.commit.tree;
        let entries = match (&parent_tree, self.detect_renames) {
            (Some(parent), true) => sley_diff_merge::diff_name_status_trees_with_options(
                self.db,
                self.format,
                parent,
                tree,
                rename_options,
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
        let entries = self.filter_diff_entries(entries, &record.oid);
        let entries = if let Some(pickaxe) = self.pickaxe
            && !self.pickaxe_all
        {
            pickaxe_filter_entries(
                self.db,
                entries,
                pickaxe,
                self.pickaxe_ignore_case,
                self.pickaxe_text,
            )?
        } else {
            entries
        };
        let mut entries = apply_diff_order_file(entries, self.opts.order_file.as_deref())?;
        if let Some(target) = self.opts.rotate_to.as_deref() {
            // `git log`/`git show` are non-strict (`rotate_to_strict == 0`).
            commands::diff_order::rotate_entries(
                &mut entries,
                target.as_bytes(),
                self.opts.rotate_skip,
                false,
            )?;
        }
        if entries.is_empty() {
            return Ok(());
        }

        let opts = self.opts;
        let stat_entries = if opts.numstat || opts.stat || opts.compact_summary || opts.shortstat {
            collect_diff_stat_entries(&entries, self.db, None, false, crate::diff_lazy_fetch(self.lazy_fetch))?
        } else {
            Vec::new()
        };
        let patch = opts.patch || merges_only;
        if opts.raw {
            for entry in &entries {
                write_diff_raw_entry(out, entry, false, false, self.raw_abbrev, self.format)?;
            }
        }
        if opts.name_status {
            for entry in &entries {
                writeln!(out, "{}", entry.line())?;
            }
        }
        if opts.name_only {
            for entry in &entries {
                writeln!(out, "{}", String::from_utf8_lossy(entry.path.as_bytes()))?;
            }
        }
        if opts.numstat {
            for entry in &stat_entries {
                write_diff_numstat_materialized_entry(out, entry.entry, entry.stats, false)?;
            }
        }
        if opts.stat || opts.compact_summary {
            let mut widths = opts.stat_widths;
            widths.resolve_config(self.config);
            widths.line_prefix_width = line_prefix_width;
            write_diff_stat_materialized_with_widths(
                out,
                &stat_entries,
                DiffStatOptions {
                    compact_summary: opts.compact_summary,
                    stat_count: opts.stat_count,
                    color: false,
                    quote_path_fully: true,
                },
                widths,
            )?;
        }
        if opts.shortstat {
            write_diff_shortstat_materialized(out, &stat_entries)?;
        }
        if opts.summary {
            for entry in &entries {
                write_diff_summary_entry(out, entry)?;
            }
        }
        if patch {
            let word_request = opts.word_diff_mode.map(|mode| WordDiffRequest {
                mode,
                cli_regex: opts.word_diff_regex.as_deref(),
            });
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
                    out,
                    entry,
                    DiffRenderOptions {
                binary: false,
                anchors: &[],
                allow_textconv: true,
                db: self.db,
                lazy_fetch: crate::diff_lazy_fetch(self.lazy_fetch),
                worktree_root: None,
                use_worktree_new: false,
                format: self.format,
                abbrev: self.patch_abbrev,
                src_prefix: "a/",
                dst_prefix: "b/",
                context: self.opts.context.unwrap_or(3),
                userdiff: Some(self.userdiff),
                funcname: None,
                colors: None,
                word_diff: word_request.as_ref(),
                line_indicators: opts.line_indicators,
                suppress_blank_empty: self
                            .config
                            .get_bool("diff", None, "suppressblankempty")
                            .unwrap_or(false),
                no_index_contents: None,
                submodule_format: commands::diff_options::SubmoduleDiffFormat::Short,
                submodule_dirt: None,
                ws_error: None,
                color_moved: None,
                interhunk: 0,
                ws_ignore: self.opts.ws_ignore,
                diff_algorithm: self.opts.diff_algorithm,
                ignore_blank_lines: self.opts.ignore_blank_lines,
                ignore_regexes: &self.opts.ignore_regexes,
                line_ranges: None,
                indent_heuristic: self.opts.indent_heuristic,
                big_file_threshold: crate::diff_big_file_threshold(self.db),
                submodule_render: crate::cli_submodule_render()
            },
                )?;
            }
        }
        Ok(())
    }

    /// `--remerge-diff`: re-merge the two parents and diff that tree against the
    /// commit, emitting `remerge CONFLICT (...)` headers for paths that
    /// conflicted in the re-merge (git's do_remerge_diff).
    fn render_remerge_diff(
        &self,
        record: &sley_rev::CommitRecord,
        line_prefix_width: i64,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        out.clear();
        let parents = &record.commit.parents;
        if parents.len() != 2 {
            // Octopus: git prints a warning and skips the remerge.
            if parents.len() > 2 {
                writeln!(
                    out,
                    "diff: warning: Skipping remerge-diff for octopus merges."
                )?;
            }
            return Ok(());
        }
        let parent1 = parents[0];
        let parent2 = parents[1];
        let label1 = remerge_parent_label(self.db, self.format, &parent1, self.patch_abbrev)?;
        let label2 = remerge_parent_label(self.db, self.format, &parent2, self.patch_abbrev)?;
        let bases = sley_rev::merge_bases(self.git_dir, self.format, self.db, &parent1, &parent2)?;
        let base_map = if bases.is_empty() {
            sley_diff_merge::MergeEntryMap::new()
        } else {
            // Reverse like try_merge_strategy so virtual fold is oldest-first.
            let mut ordered = bases;
            ordered.reverse();
            sley_diff_merge::virtual_ancestor_entry_map_with_style(
                self.db,
                self.format,
                &ordered,
                sley_diff_merge::ConflictStyle::Merge,
                |left, right| {
                    sley_rev::merge_bases(self.git_dir, self.format, self.db, left, right)
                },
            )?
        };
        let tree1 = self.parent_tree(&parent1)?;
        let tree2 = self.parent_tree(&parent2)?;
        let ours_map = sley_diff_merge::flatten_tree(self.db, self.format, &tree1)?;
        let theirs_map = sley_diff_merge::flatten_tree(self.db, self.format, &tree2)?;
        let merge = sley_diff_merge::merge_entry_maps(
            self.db,
            self.format,
            &base_map,
            &ours_map,
            &theirs_map,
            &sley_diff_merge::MergeTreesOptions {
                ours_label: &label1,
                theirs_label: &label2,
                ancestor_label: "merged common ancestors",
                detect_renames: true,
                rename_threshold: sley_diff_merge::DEFAULT_RENAME_THRESHOLD,
                ..Default::default()
            },
        )?;
        let mut conflict_headers: std::collections::BTreeMap<Vec<u8>, String> =
            std::collections::BTreeMap::new();
        for path_result in &merge.paths {
            if let Some(conflict) = &path_result.conflict {
                let header = remerge_conflict_header(conflict, &path_result.path);
                conflict_headers.insert(path_result.path.clone(), header);
            }
        }
        let remerge_tree = merge.tree;
        let commit_tree = record.commit.tree;
        // Diff remerge_tree (old) → commit_tree (new).
        let base = sley_diff_merge::DiffNameStatusOptions {
            detect_renames: self.detect_renames,
            detect_copies: self.detect_copies,
            find_copies_harder: self.find_copies_harder,
            rename_empty: true,
            ..Default::default()
        };
        let rename_options = sley_diff_merge::DiffNameStatusOptions {
            detect_renames: self.detect_renames,
            detect_copies: self.detect_copies,
            find_copies_harder: self.find_copies_harder,
            rename_empty: true,
            detect_inexact: true,
            rename_threshold: sley_diff_merge::DEFAULT_RENAME_THRESHOLD,
            copy_threshold: sley_diff_merge::DEFAULT_RENAME_THRESHOLD,
            rename_limit: 0,
            ..Default::default()
        };
        let entries = if self.detect_renames {
            sley_diff_merge::diff_name_status_trees_with_options(
                self.db,
                self.format,
                &remerge_tree,
                &commit_tree,
                rename_options,
            )?
        } else {
            sley_diff_merge::diff_name_status_trees_with_options(
                self.db,
                self.format,
                &remerge_tree,
                &commit_tree,
                base,
            )?
        };
        let entries = self.filter_diff_entries(entries, &record.oid);
        if entries.is_empty() {
            return Ok(());
        }
        // Apply pickaxe after collecting entries.
        let entries = if let Some(pickaxe) = self.pickaxe
            && !self.pickaxe_all
        {
            pickaxe_filter_entries(
                self.db,
                entries,
                pickaxe,
                self.pickaxe_ignore_case,
                self.pickaxe_text,
            )?
        } else {
            entries
        };
        if entries.is_empty() {
            return Ok(());
        }
        let _ = line_prefix_width;
        let opts = self.opts;
        let merges_only = !opts.any();
        let patch = opts.patch || merges_only || opts.merges_imply_patch;
        if patch {
            let word_request = opts.word_diff_mode.map(|mode| WordDiffRequest {
                mode,
                cli_regex: opts.word_diff_regex.as_deref(),
            });
            for entry in &entries {
                let mut file_out = Vec::new();
                write_diff_patch_entry(
                    &mut file_out,
                    entry,
                    DiffRenderOptions {
                binary: false,
                anchors: &[],
                allow_textconv: true,
                db: self.db,
                lazy_fetch: crate::diff_lazy_fetch(self.lazy_fetch),
                worktree_root: None,
                use_worktree_new: false,
                format: self.format,
                abbrev: self.patch_abbrev,
                src_prefix: "a/",
                dst_prefix: "b/",
                context: self.opts.context.unwrap_or(3),
                userdiff: Some(self.userdiff),
                funcname: None,
                colors: None,
                word_diff: word_request.as_ref(),
                line_indicators: opts.line_indicators,
                suppress_blank_empty: self
                            .config
                            .get_bool("diff", None, "suppressblankempty")
                            .unwrap_or(false),
                no_index_contents: None,
                submodule_format: commands::diff_options::SubmoduleDiffFormat::Short,
                submodule_dirt: None,
                ws_error: None,
                color_moved: None,
                interhunk: 0,
                ws_ignore: self.opts.ws_ignore,
                diff_algorithm: self.opts.diff_algorithm,
                ignore_blank_lines: self.opts.ignore_blank_lines,
                ignore_regexes: &self.opts.ignore_regexes,
                line_ranges: None,
                indent_heuristic: self.opts.indent_heuristic,
                big_file_threshold: crate::diff_big_file_threshold(self.db),
                submodule_render: crate::cli_submodule_render()
            },
                )?;
                // Inject remerge CONFLICT header after the first "diff --git" line.
                if let Some(header) = conflict_headers.get(entry.path.as_bytes()) {
                    if let Some(pos) = file_out.iter().position(|&b| b == b'\n') {
                        let mut injected = file_out[..=pos].to_vec();
                        injected.extend_from_slice(header.as_bytes());
                        injected.push(b'\n');
                        injected.extend_from_slice(&file_out[pos + 1..]);
                        out.extend_from_slice(&injected);
                    } else {
                        out.extend_from_slice(&file_out);
                    }
                } else {
                    out.extend_from_slice(&file_out);
                }
            }
        }
        Ok(())
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
        out: &mut Vec<u8>,
    ) -> Result<()> {
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
        let stat_active = opts.raw
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
                ..Default::default()
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
            return Ok(());
        }

        if opts.raw {
            // git shows combined raw (`::`) for log --raw on a combined merge.
            let render_ctx = self.combined_ctx(dense);
            for path in &paths {
                commands::combined::write_combined_raw(out, &render_ctx, path, false)?;
            }
        }
        if opts.name_status {
            for path in &paths {
                commands::combined::write_combined_name_status(out, path, false, false)?;
            }
        }
        if opts.name_only {
            for path in &paths {
                writeln!(out, "{}", String::from_utf8_lossy(&path.path))?;
            }
        }
        let stat_entries = if opts.numstat || opts.stat || opts.compact_summary || opts.shortstat {
            collect_diff_stat_entries(&first_parent_entries, self.db, None, false, crate::diff_lazy_fetch(self.lazy_fetch))?
        } else {
            Vec::new()
        };
        if opts.numstat {
            for entry in &stat_entries {
                write_diff_numstat_materialized_entry(out, entry.entry, entry.stats, false)?;
            }
        }
        if opts.stat || opts.compact_summary {
            let mut widths = opts.stat_widths;
            widths.resolve_config(self.config);
            widths.line_prefix_width = line_prefix_width;
            write_diff_stat_materialized_with_widths(
                out,
                &stat_entries,
                DiffStatOptions {
                    compact_summary: opts.compact_summary,
                    stat_count: opts.stat_count,
                    color: false,
                    quote_path_fully: true,
                },
                widths,
            )?;
        }
        if opts.shortstat {
            write_diff_shortstat_materialized(out, &stat_entries)?;
        }
        if opts.summary {
            for entry in &first_parent_entries {
                write_diff_summary_entry(out, entry)?;
            }
        }
        if patch && !paths.is_empty() {
            if stat_active {
                out.push(b'\n');
            }
            let render_ctx = self.combined_ctx(dense);
            for path in &paths {
                commands::combined::write_combined_patch(out, &render_ctx, path)?;
            }
        }
        Ok(())
    }

    /// Build the shared combined-render context for this log invocation.
    fn combined_ctx(&self, dense: bool) -> commands::combined::CombinedRenderCtx<'_> {
        commands::combined::CombinedRenderCtx {
            db: self.db,
            format: self.format,
            dense,
            all_paths: false,
            context: self.opts.context.unwrap_or(3),
            ws_ignore: self.opts.ws_ignore,
            diff_algorithm: self.opts.diff_algorithm,
            src_prefix: "a/",
            dst_prefix: "b/",
            patch_abbrev: self.patch_abbrev,
            raw_abbrev: self.raw_abbrev,
            lazy_fetch: self.lazy_fetch,
        }
    }

    /// Tree oid of `parent`.
    fn parent_tree(&self, parent: &ObjectId) -> Result<ObjectId> {
        let object = self.db.read_object(parent)?;
        Ok(Commit::parse_ref(self.format, &object.body)?.tree)
    }
}

/// Parent label for remerge-diff conflict markers: `"%h (%s)"`.
fn remerge_parent_label(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    oid: &ObjectId,
    abbrev: usize,
) -> Result<String> {
    let object = db.read_object(oid)?;
    let commit = Commit::parse_ref(format, &object.body)?;
    let hex = oid.to_hex();
    let short = &hex[..abbrev.min(hex.len())];
    let subject = commit
        .message
        .split(|&b| b == b'\n')
        .next()
        .map(|line| String::from_utf8_lossy(line).into_owned())
        .unwrap_or_default();
    Ok(format!("{short} ({subject})"))
}

/// Format a remerge additional-header line for a conflicted path.
fn remerge_conflict_header(kind: &sley_diff_merge::MergeConflictKind, path: &[u8]) -> String {
    let path_str = String::from_utf8_lossy(path);
    match kind {
        sley_diff_merge::MergeConflictKind::Content { add_add: true } => {
            format!("remerge CONFLICT (add/add): Merge conflict in {path_str}")
        }
        sley_diff_merge::MergeConflictKind::Content { .. }
        | sley_diff_merge::MergeConflictKind::RenameContent { .. } => {
            format!("remerge CONFLICT (content): Merge conflict in {path_str}")
        }
        sley_diff_merge::MergeConflictKind::ModifyDelete { .. } => {
            format!("remerge CONFLICT (modify/delete): {path_str}")
        }
        sley_diff_merge::MergeConflictKind::RenameDelete { .. } => {
            format!("remerge CONFLICT (rename/delete): {path_str}")
        }
        sley_diff_merge::MergeConflictKind::RenameRenameTwoToOne { .. }
        | sley_diff_merge::MergeConflictKind::RenameRenameOneToTwo { .. } => {
            format!("remerge CONFLICT (rename/rename): {path_str}")
        }
        sley_diff_merge::MergeConflictKind::FileDirectory { .. } => {
            format!("remerge CONFLICT (file/directory): {path_str}")
        }
        sley_diff_merge::MergeConflictKind::DistinctTypes { .. } => {
            format!("remerge CONFLICT (distinct types): {path_str}")
        }
        _ => format!("remerge CONFLICT (content): Merge conflict in {path_str}"),
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
