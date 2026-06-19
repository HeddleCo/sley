use super::*;

/// Bundle of the context `run_line_log_output` needs (avoids a 20-arg fn).
pub(super) struct LineLogOutputCtx<'a> {
    pub(super) git_dir: &'a Path,
    pub(super) db: &'a FileObjectDatabase,
    pub(super) format: ObjectFormat,
    pub(super) config: &'a GitConfig,
    pub(super) tip: ObjectId,
    pub(super) args: &'a [crate::commands::line_log::LineLogArg],
    pub(super) output: &'a LogOutput,
    pub(super) diff_opts: &'a LogDiffOptions,
    pub(super) date_mode: &'a DateMode,
    pub(super) abbrev_len: Option<usize>,
    pub(super) abbrev_commit: bool,
    pub(super) detect_renames: bool,
    pub(super) first_parent: bool,
    pub(super) max_count: Option<usize>,
    pub(super) reverse: bool,
    pub(super) show_parents: bool,
    pub(super) decoration: LogDecorationMode,
    pub(super) output_encoding: &'a str,
    /// `--src-prefix`/`--no-prefix` override for the `-L` patch (`None` keeps
    /// the `a/` default).
    pub(super) src_prefix: Option<&'a str>,
    /// `--dst-prefix`/`--no-prefix` override (`None` keeps `b/`).
    pub(super) dst_prefix: Option<&'a str>,
    /// `--full-index`: emit the full object names on the `index` line.
    pub(super) full_index: bool,
    /// Whether `--abbrev=<n>` was given explicitly (so the diff `index` line
    /// honors it instead of the config-derived diff abbreviation).
    pub(super) abbrev_len_explicit: bool,
    /// `--since`/`--after` lower time bound (commits older than this are pruned
    /// from the line-log walk). `None` == no lower bound.
    pub(super) max_age: Option<i64>,
    /// `--until`/`--before` upper time bound (commits newer than this are
    /// pruned). `None` == no upper bound.
    pub(super) min_age: Option<i64>,
    /// `-S`/`-G`/`--find-object` pickaxe: like upstream's `-L` + pickaxe, this
    /// suppresses the *diff pairs* of a commit whose whole-file diff does not
    /// match (the commit header still prints, matching git's pipeline where
    /// show_log runs before diffcore_std's pickaxe). `None` == no pickaxe.
    pub(super) pickaxe: Option<&'a CompiledPickaxe>,
    pub(super) pickaxe_ignore_case: bool,
    pub(super) pickaxe_text: bool,
    pub(super) pickaxe_detect_renames: bool,
}

/// `git log -L`: walk history with the line-log engine and emit each commit that
/// touched a tracked range, with its patch clipped to that range. Mirrors git's
/// `line_log_filter` + the log-tree output loop (the `-s`/`-p`/format cases the
/// test suite exercises).
pub(super) fn run_line_log_output(ctx: LineLogOutputCtx<'_>) -> Result<()> {
    let LineLogOutputCtx {
        git_dir,
        db,
        format,
        config,
        tip,
        args,
        output,
        diff_opts,
        date_mode,
        abbrev_len,
        abbrev_commit,
        detect_renames,
        first_parent,
        max_count,
        reverse,
        show_parents,
        decoration,
        output_encoding,
        src_prefix,
        dst_prefix,
        full_index,
        abbrev_len_explicit,
        max_age,
        min_age,
        pickaxe,
        pickaxe_ignore_case,
        pickaxe_text,
        pickaxe_detect_renames,
    } = ctx;

    // `-L` line-log shares git's `log.mailmap` default (true) and the default
    // pretty-format identity mapping.
    let use_mailmap = config.get_bool("log", None, "mailmap").unwrap_or(true);
    let mailmap = commands::utility::Mailmap::load_default(git_dir, format)?;

    // Reachable commits from the tip, in topological order (child before
    // parent) — git forces `topo_order` for `-L`. The line-log engine walks the
    // FULL ancestry to map ranges; `--since`/`--until` only prune which of the
    // resulting interesting commits are displayed (applied to `selected` below).
    let reachable = rev_list_walk_commits(db, format, [tip], first_parent)?;
    let refs: Vec<&sley_rev::CommitRecord> = reachable.iter().collect();
    let ordered_refs = rev_list_topo_order(refs)?;
    let ordered: Vec<sley_rev::CommitRecord> =
        ordered_refs.into_iter().cloned().collect();

    let result = crate::commands::line_log::run_line_log(
        db,
        format,
        &ordered,
        &tip,
        args,
        detect_renames,
        first_parent,
    )?;

    // The interesting list is already in topo order (newest first). Apply
    // `-n`/`--reverse`.
    let mut selected: Vec<&sley_rev::CommitRecord> = {
        let by_oid: HashMap<ObjectId, &sley_rev::CommitRecord> =
            ordered.iter().map(|r| (r.oid, r)).collect();
        result
            .interesting
            .iter()
            .filter_map(|oid| by_oid.get(oid).copied())
            .collect()
    };
    // `--since`/`--until` (`--after`/`--before`) limit which interesting commits
    // are displayed, by committer date — applied before `-n` so the count runs
    // over the post-filter range (mirrors the ordinary log walk).
    if max_age.is_some() || min_age.is_some() {
        selected.retain(|record| log_age_filters_match(record, max_age, min_age).unwrap_or(true));
    }
    if let Some(max_count) = max_count {
        selected.truncate(max_count);
    }
    if reverse {
        selected.reverse();
    }

    // `--parents`: rewrite each interesting commit's parents to its nearest
    // interesting ancestors (revision.c rewrite_parents over the line-log
    // history). An uninteresting parent is replaced by the union of *its*
    // nearest-interesting ancestors, so commits that did not touch the tracked
    // range collapse out of the displayed parent chain.
    let rewritten_parents: Option<HashMap<ObjectId, Vec<ObjectId>>> = if show_parents {
        let interesting_set: HashSet<ObjectId> = result.interesting.iter().copied().collect();
        let by_oid: HashMap<ObjectId, &sley_rev::CommitRecord> =
            ordered.iter().map(|r| (r.oid, r)).collect();
        // Nearest interesting ancestors of `oid` (exclusive of `oid` itself),
        // dedup-preserving order. Walks parent edges, collapsing uninteresting
        // commits.
        fn nearest_interesting(
            oid: &ObjectId,
            interesting: &HashSet<ObjectId>,
            by_oid: &HashMap<ObjectId, &sley_rev::CommitRecord>,
            out: &mut Vec<ObjectId>,
            seen: &mut HashSet<ObjectId>,
        ) {
            let Some(record) = by_oid.get(oid) else {
                return;
            };
            for parent in &record.parents {
                if interesting.contains(parent) {
                    if seen.insert(*parent) {
                        out.push(*parent);
                    }
                } else {
                    nearest_interesting(parent, interesting, by_oid, out, seen);
                }
            }
        }
        let mut map = HashMap::new();
        for oid in &interesting_set {
            let mut out = Vec::new();
            let mut seen = HashSet::new();
            nearest_interesting(oid, &interesting_set, &by_oid, &mut out, &mut seen);
            map.insert(*oid, out);
        }
        Some(map)
    } else {
        None
    };

    // Decorations (only when `--decorate` is on; the tests redirect output so
    // the default is off).
    let decorations: HashMap<ObjectId, Vec<String>> = if decoration == LogDecorationMode::Off {
        HashMap::new()
    } else {
        let include = ["HEAD", "refs/heads/", "refs/tags/", "refs/remotes/", "refs/stash", "refs/replace/"]
            .map(str::to_string);
        let filter = DecorationFilter::new(&include, &[], &[]);
        log_decoration_map(git_dir, db, format, decoration, &filter)?
    };
    let describe_ctx = LogDescribeContext {
        git_dir,
        db,
        format,
    };
    let patch_abbrev = if full_index {
        // `--full-index`: the `index` line carries the full object names.
        format.hex_len()
    } else if abbrev_len_explicit {
        // `--abbrev=<n>` overrides the diff abbreviation directly.
        abbrev_len.unwrap_or(7).min(format.hex_len())
    } else {
        repository_abbrev_from_config(git_dir, format, config)?
            .unwrap_or(7)
            .min(format.hex_len())
    };

    // Effective diff path prefixes: `--src-prefix`/`--dst-prefix`/`--no-prefix`
    // override the `a/`/`b/` defaults.
    let eff_src_prefix = src_prefix.unwrap_or("a/");
    let eff_dst_prefix = dst_prefix.unwrap_or("b/");

    let mut stdout = io::stdout();
    let mut patch_block = Vec::new();
    let mut printed_entries = 0usize;
    for record in &selected {
        // `-S`/`-G`/`--find-object`: suppress this commit's diff pairs when its
        // whole-file diff does not match the pickaxe (the header still prints,
        // matching git's show_log-before-diffcore_std pipeline). Computed on the
        // ORIGINAL record so the real parent edge drives the diff.
        let pickaxe_suppresses = match pickaxe {
            Some(pickaxe) => !pickaxe_commit_matches(
                db,
                format,
                record,
                pickaxe,
                pickaxe_ignore_case,
                pickaxe_text,
                pickaxe_detect_renames,
                None,
            )?,
            None => false,
        };
        let files = if pickaxe_suppresses {
            None
        } else {
            result.printed.get(&record.oid)
        };
        // Under `--parents`, render the rewritten parent set (collapsing
        // commits that did not touch the tracked range) for both the inline
        // parent list and any `%p`/`%P` format placeholders.
        let rewritten = rewritten_parents
            .as_ref()
            .and_then(|map| map.get(&record.oid));
        let owned_record;
        let record: &sley_rev::CommitRecord = match rewritten {
            Some(parents) if parents.as_slice() != record.parents.as_slice() => {
                let mut clone = (*record).clone();
                clone.parents = parents.clone();
                owned_record = clone;
                &owned_record
            }
            _ => record,
        };
        match output {
            LogOutput::Default(kind) => {
                if printed_entries > 0 {
                    writeln!(stdout)?;
                }
                printed_entries += 1;
                write!(
                    stdout,
                    "commit {}",
                    format_log_commit_header_oid(&record.oid, abbrev_commit, abbrev_len)
                )?;
                if let Some(labels) = decorations.get(&record.oid)
                    && !labels.is_empty()
                {
                    write!(stdout, " ({})", labels.join(", "))?;
                }
                if show_parents {
                    write!(stdout, " ")?;
                    let merged: Vec<String> =
                        record.parents.iter().map(format_log_abbrev_oid).collect();
                    write!(stdout, "{}", merged.join(" "))?;
                }
                writeln!(stdout)?;
                if record.parents.len() > 1 {
                    let merged: Vec<String> =
                        record.parents.iter().map(format_log_abbrev_oid).collect();
                    writeln!(stdout, "Merge: {}", merged.join(" "))?;
                }
                writeln!(
                    stdout,
                    "Author: {}",
                    commit_identity_mailmapped(&record.commit.author, use_mailmap.then_some(&mailmap))
                )?;
                if *kind == LogDefaultKind::Medium {
                    writeln!(
                        stdout,
                        "Date:   {}",
                        commit_identity_date(&record.commit.author, date_mode)
                    )?;
                }
                writeln!(stdout)?;
                for line in String::from_utf8_lossy(&record.commit.message).lines() {
                    if line.is_empty() {
                        writeln!(stdout)?;
                    } else {
                        writeln!(stdout, "    {line}")?;
                    }
                }
                if diff_opts.patch {
                    patch_block.clear();
                    render_line_log_patch(
                        &mut patch_block,
                        db,
                        format,
                        files,
                        diff_opts,
                        patch_abbrev,
                        eff_src_prefix,
                        eff_dst_prefix,
                    )?;
                    if !patch_block.is_empty() {
                        stdout.write_all(diff_opts.block_separator())?;
                        stdout.write_all(&patch_block)?;
                    }
                }
            }
            LogOutput::Compiled {
                compiled,
                final_newline,
                ..
            } => {
                if printed_entries > 0 && !*final_newline {
                    stdout.write_all(b"\n")?;
                }
                printed_entries += 1;
                let format_context = LogFormatContext {
                    abbrev_len,
                    decorations: &decorations,
                    marker: '>',
                    dialect: LogFormatDialect::Log,
                    source: None,
                    date_mode,
                    source_oid: None,
                    describe: Some(&describe_ctx),
                    color: false,
                    output_encoding,
                    mailmap: &mailmap,
                    use_mailmap,
                };
                let mut line = Vec::with_capacity(compiled.estimated_line_capacity());
                emit_compiled_log_format(
                    record,
                    compiled,
                    &format_context,
                    &mut line,
                    0..compiled.tokens.len(),
                )?;
                let line = log_reencode_message(&line, "UTF-8", output_encoding);
                stdout.write_all(&line)?;
                if *final_newline {
                    stdout.write_all(b"\n")?;
                }
                if diff_opts.patch {
                    patch_block.clear();
                    render_line_log_patch(
                        &mut patch_block,
                        db,
                        format,
                        files,
                        diff_opts,
                        patch_abbrev,
                        eff_src_prefix,
                        eff_dst_prefix,
                    )?;
                    if !patch_block.is_empty() {
                        if !*final_newline {
                            stdout.write_all(b"\n")?;
                        }
                        stdout.write_all(&patch_block)?;
                    }
                }
            }
        }
    }
    stdout.flush()?;
    Ok(())
}

/// Render the restricted patch block for one line-log commit: each printed file
/// is emitted via the shared patch writer with its hunks clipped to the tracked
/// post-image line ranges.
#[allow(clippy::too_many_arguments)]
fn render_line_log_patch(
    out: &mut Vec<u8>,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    files: Option<&Vec<crate::commands::line_log::PrintedFile>>,
    diff_opts: &LogDiffOptions,
    patch_abbrev: usize,
    src_prefix: &str,
    dst_prefix: &str,
) -> Result<()> {
    let files = match files {
        Some(f) if !f.is_empty() => f,
        _ => return Ok(()),
    };
    for file in files {
        let entry = sley_diff_merge::NameStatusEntry {
            status: file.status,
            path: sley_rev::BString::from(file.new_path.as_bytes().to_vec()),
            old_path: if file.old_path != file.new_path {
                Some(sley_rev::BString::from(file.old_path.as_bytes().to_vec()))
            } else {
                None
            },
            old_mode: file.old_mode,
            new_mode: file.new_mode,
            old_oid: file.old_oid,
            new_oid: file.new_oid,
        };
        crate::write_diff_patch_entry(
            out,
            &entry,
            crate::DiffPatchOptions {
                db,
                worktree_root: None,
                use_worktree_new: false,
                format,
                abbrev: patch_abbrev,
                src_prefix,
                dst_prefix,
                context: 3,
                userdiff: None,
                colors: None,
                word_diff: None,
                no_index_contents: None,
                submodule_format: commands::diff_options::SubmoduleDiffFormat::Short,
                submodule_dirt: None,
                ws_error: None,
                interhunk: 0,
                ws_ignore: diff_opts.ws_ignore,
                diff_algorithm: diff_opts.diff_algorithm,
                ignore_blank_lines: diff_opts.ignore_blank_lines,
                ignore_regexes: &diff_opts.ignore_regexes,
                line_ranges: Some(&file.line_ranges),
                indent_heuristic: diff_opts.indent_heuristic,
            },
        )?;
    }
    Ok(())
}
