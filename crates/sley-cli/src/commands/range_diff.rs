//! `git range-diff` — compare two commit series by patch similarity.

use crate::*;
use sley_diff_merge::range::{PatchRef, assign_patch_series};
use sley_notes::{NotesRef, read_note_bytes};

const DEFAULT_CREATION_FACTOR: i32 = 60;

#[derive(Clone)]
struct RangeDiffOptions {
    creation_factor: i32,
    abbrev: usize,
    color: bool,
    dual_color: bool,
    left_only: bool,
    right_only: bool,
    notes: NotesMode,
    include_merges: bool,
    diff: InterdiffOptions,
}

#[derive(Clone)]
struct InterdiffOptions {
    patch: bool,
    stat: bool,
    context: usize,
}

#[derive(Clone)]
enum NotesMode {
    Default,
    None,
    Refs(Vec<String>),
}

struct ParsedRangeDiff {
    range1: Vec<String>,
    range2: Vec<String>,
    pathspecs: Vec<String>,
}

#[derive(Clone)]
struct PatchRecord {
    oid: ObjectId,
    index: usize,
    subject: String,
    patch: Vec<u8>,
    diff_offset: usize,
    diff_size: i32,
    matching: Option<usize>,
    shown: bool,
}

impl PatchRecord {
    fn diff(&self) -> &[u8] {
        &self.patch[self.diff_offset..]
    }
}

pub(crate) fn cmd_range_diff(args: &[String]) -> Result<()> {
    let repo = RepositoryContext::discover_current()?;
    let mut options = default_options(&repo)?;
    let parsed = parse_range_diff_args(&repo, args, &mut options)?;
    if options.left_only && options.right_only {
        eprintln!("error: options '--left-only' and '--right-only' cannot be used together");
        return Err(GitError::Exit(129));
    }
    let notes_refs = resolve_notes_refs(&repo, &options.notes)?;
    let rendered = render_range_diff(&repo, &parsed, &options, &notes_refs)?;
    io::stdout().write_all(&rendered)?;
    Ok(())
}

fn default_options(repo: &RepositoryContext) -> Result<RangeDiffOptions> {
    Ok(RangeDiffOptions {
        creation_factor: DEFAULT_CREATION_FACTOR,
        abbrev: repo.abbrev()?.unwrap_or(7).min(repo.format().hex_len()),
        color: false,
        dual_color: false,
        left_only: false,
        right_only: false,
        notes: NotesMode::Default,
        include_merges: false,
        diff: InterdiffOptions {
            patch: true,
            stat: false,
            context: 3,
        },
    })
}

pub(crate) fn render_format_patch_range_diff(
    repo: &RepositoryContext,
    previous: &str,
    new_range_args: &[String],
    pathspecs: &[String],
    notes_refs: &[String],
) -> Result<Vec<u8>> {
    let mut options = default_options(repo)?;
    options.notes = NotesMode::None;
    let range1 = match normalize_range_arg(repo, previous)? {
        Some(range) if range.len() != 1 || is_commit_range(repo, previous) => range,
        _ => {
            let previous_oid = repo.resolve_revision(previous)?;
            let new_tip = range_tip(repo, new_range_args)?;
            let bases = merge_bases(
                repo.git_dir(),
                repo.objects(),
                repo.format(),
                &previous_oid,
                &new_tip,
            )?;
            let mut range = bases.iter().map(|base| format!("^{base}")).collect::<Vec<_>>();
            range.push(previous.to_string());
            range
        }
    };
    let parsed = ParsedRangeDiff {
        range1,
        range2: new_range_args.to_vec(),
        pathspecs: pathspecs.to_vec(),
    };
    render_range_diff(repo, &parsed, &options, notes_refs)
}

fn range_tip(repo: &RepositoryContext, setup_args: &[String]) -> Result<ObjectId> {
    let setup = sley_rev::setup_revisions(
        setup_args,
        &sley_rev::RevisionSetupContext {
            git_dir: repo.git_dir(),
            worktree_root: repo.worktree_root().ok(),
            cwd: repo.cwd(),
            format: repo.format(),
            reader: repo.objects(),
            config: Some(repo.config()),
        },
    )?;
    setup
        .options
        .positives
        .last()
        .map(|tip| tip.oid)
        .ok_or_else(|| GitError::Command("range-diff requires a positive revision".into()))
}

fn render_range_diff(
    repo: &RepositoryContext,
    parsed: &ParsedRangeDiff,
    options: &RangeDiffOptions,
    notes_refs: &[String],
) -> Result<Vec<u8>> {
    let mut left = read_patches(repo, &parsed.range1, &parsed.pathspecs, options, notes_refs)?;
    let mut right = read_patches(repo, &parsed.range2, &parsed.pathspecs, options, notes_refs)?;
    assign_correspondences(&mut left, &mut right, options.creation_factor);
    let mut out = Vec::new();
    output_range_diff(&mut out, repo, &mut left, &mut right, options)?;
    Ok(out)
}

fn parse_range_diff_args(
    repo: &RepositoryContext,
    args: &[String],
    options: &mut RangeDiffOptions,
) -> Result<ParsedRangeDiff> {
    let mut positionals = Vec::new();
    let mut pathspecs = Vec::new();
    let mut after_dashdash = false;
    let mut idx = 0;
    while idx < args.len() {
        let arg = &args[idx];
        if after_dashdash {
            pathspecs.push(arg.clone());
            idx += 1;
            continue;
        }
        if arg == "--" {
            after_dashdash = true;
            idx += 1;
            continue;
        }
        match arg.as_str() {
            "--no-color" | "--color=never" => options.color = false,
            "--color" | "--color=always" => options.color = true,
            "--dual-color" => options.dual_color = true,
            "--no-dual-color" => options.dual_color = false,
            "--left-only" => options.left_only = true,
            "--right-only" => options.right_only = true,
            "--no-notes" => options.notes = NotesMode::None,
            "--notes" => options.notes = NotesMode::Default,
            "--no-patch" | "-s" => {
                options.diff.patch = false;
                options.diff.stat = false;
            }
            "--stat" => {
                options.diff.stat = true;
                options.diff.patch = false;
            }
            "--diff-merges=1" | "--diff-merges=first-parent" | "-m" => {
                options.include_merges = true;
            }
            "--submodule=log" | "--submodule=short" | "--submodule=diff" => {}
            "--abbrev" => options.abbrev = 7.min(repo.format().hex_len()),
            "-U" | "--unified" => {
                idx += 1;
                let value = args.get(idx).ok_or_else(|| {
                    GitError::Command("option `unified' requires a value".into())
                })?;
                options.diff.context = parse_usize_option(value, "unified")?;
                options.diff.patch = true;
            }
            value if value.starts_with("-U") && value.len() > 2 => {
                options.diff.context = parse_usize_option(&value[2..], "unified")?;
                options.diff.patch = true;
            }
            value if let Some(value) = value.strip_prefix("--unified=") => {
                options.diff.context = parse_usize_option(value, "unified")?;
                options.diff.patch = true;
            }
            value if let Some(value) = value.strip_prefix("--creation-factor=") => {
                options.creation_factor = parse_i32_option(value, "creation-factor")?;
            }
            "--creation-factor" => {
                idx += 1;
                let value = args.get(idx).ok_or_else(|| {
                    GitError::Command("option `creation-factor' requires a value".into())
                })?;
                options.creation_factor = parse_i32_option(value, "creation-factor")?;
            }
            value if let Some(value) = value.strip_prefix("--abbrev=") => {
                options.abbrev = parse_usize_option(value, "abbrev")?.min(repo.format().hex_len());
            }
            value if let Some(value) = value.strip_prefix("--notes=") => {
                match &mut options.notes {
                    NotesMode::Refs(refs) => refs.push(value.to_string()),
                    _ => options.notes = NotesMode::Refs(vec![value.to_string()]),
                }
            }
            value if value.starts_with('-') => {
                return Err(GitError::Command(format!(
                    "unsupported range-diff option {value}"
                )));
            }
            _ => positionals.push(arg.clone()),
        }
        idx += 1;
    }

    match positionals.len() {
        1 => {
            let Some((left, right)) = positionals[0].split_once("...") else {
                eprintln!("fatal: need two commit ranges");
                return Err(GitError::Exit(129));
            };
            let left = if left.is_empty() { "HEAD" } else { left };
            let right = if right.is_empty() { "HEAD" } else { right };
            let left_oid = repo.resolve_revision(left)?;
            let right_oid = repo.resolve_revision(right)?;
            let bases = merge_bases(repo.git_dir(), repo.objects(), repo.format(), &left_oid, &right_oid)?;
            if bases.is_empty() {
                eprintln!("fatal: no merge base between '{left}' and '{right}'");
                return Err(GitError::Exit(128));
            }
            let mut range1 = bases.iter().map(|base| format!("^{base}")).collect::<Vec<_>>();
            range1.push(left.to_string());
            let mut range2 = bases.iter().map(|base| format!("^{base}")).collect::<Vec<_>>();
            range2.push(right.to_string());
            Ok(ParsedRangeDiff {
                range1,
                range2,
                pathspecs,
            })
        }
        2 => {
            let range1 = normalize_range_arg(repo, &positionals[0])?;
            let range2 = normalize_range_arg(repo, &positionals[1])?;
            match (range1, range2) {
                (Some(range1), Some(range2)) => Ok(ParsedRangeDiff {
                    range1,
                    range2,
                    pathspecs,
                }),
                _ => {
                    eprintln!("fatal: need two commit ranges");
                    eprintln!("fatal: not a commit range");
                    Err(GitError::Exit(129))
                }
            }
        }
        3 => Ok(ParsedRangeDiff {
            range1: vec![format!("^{}", positionals[0]), positionals[1].clone()],
            range2: vec![format!("^{}", positionals[0]), positionals[2].clone()],
            pathspecs,
        }),
        _ => {
            eprintln!("fatal: need two commit ranges");
            Err(GitError::Exit(129))
        }
    }
}

fn normalize_range_arg(repo: &RepositoryContext, arg: &str) -> Result<Option<Vec<String>>> {
    if let Some(base) = arg.strip_suffix("^!") {
        let oid = repo.resolve_revision(base)?;
        let commit = read_commit_record(repo.objects(), repo.format(), &oid)?;
        let mut out = commit
            .parents
            .iter()
            .map(|parent| format!("^{parent}"))
            .collect::<Vec<_>>();
        out.push(base.to_string());
        return Ok(Some(out));
    }
    if let Some(base) = arg.strip_suffix("^-") {
        return Ok(Some(vec![format!("{base}^..{base}")]));
    }
    if let Some((base, parent)) = arg.rsplit_once("^-")
        && !parent.is_empty()
        && parent.bytes().all(|b| b.is_ascii_digit())
    {
        return Ok(Some(vec![format!("{base}^{parent}..{base}")]));
    }
    Ok(is_commit_range(repo, arg).then(|| vec![arg.to_string()]))
}

fn read_commit_record(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    oid: &ObjectId,
) -> Result<sley_rev::CommitRecord> {
    let object = db.read_object(oid)?;
    if object.object_type != ObjectType::Commit {
        return Err(GitError::InvalidObject(format!("{oid} is not a commit")));
    }
    let commit = Commit::parse(format, &object.body)?;
    Ok(sley_rev::CommitRecord {
        oid: *oid,
        parents: commit.parents.clone(),
        commit,
    })
}

fn is_commit_range(repo: &RepositoryContext, arg: &str) -> bool {
    let setup = sley_rev::setup_revisions(
        &[arg.to_string()],
        &sley_rev::RevisionSetupContext {
            git_dir: repo.git_dir(),
            worktree_root: repo.worktree_root().ok(),
            cwd: repo.cwd(),
            format: repo.format(),
            reader: repo.objects(),
            config: Some(repo.config()),
        },
    );
    let Ok(setup) = setup else {
        return false;
    };
    !setup.options.positives.is_empty() && !setup.options.negatives.is_empty()
}

fn parse_usize_option(value: &str, name: &str) -> Result<usize> {
    value
        .parse::<usize>()
        .map_err(|_| GitError::Command(format!("option `{name}' expects a numerical value")))
}

fn parse_i32_option(value: &str, name: &str) -> Result<i32> {
    value
        .parse::<i32>()
        .map_err(|_| GitError::Command(format!("option `{name}' expects a numerical value")))
}

fn read_patches(
    repo: &RepositoryContext,
    setup_args: &[String],
    pathspecs: &[String],
    options: &RangeDiffOptions,
    notes_refs: &[String],
) -> Result<Vec<PatchRecord>> {
    let format = repo.format();
    let db = repo.objects();
    let mut args = setup_args.to_vec();
    if !pathspecs.is_empty() {
        args.push("--".to_string());
        args.extend(pathspecs.iter().cloned());
    }
    let setup = sley_rev::setup_revisions(
        &args,
        &sley_rev::RevisionSetupContext {
            git_dir: repo.git_dir(),
            worktree_root: repo.worktree_root().ok(),
            cwd: repo.cwd(),
            format,
            reader: db,
            config: Some(repo.config()),
        },
    )?;
    let starts = setup
        .options
        .positives
        .iter()
        .map(|tip| sley_rev::peel_to_commit(db, format, &tip.oid))
        .collect::<Result<Vec<_>>>()?;
    let mut excluded = HashSet::new();
    for negative in setup.options.negatives {
        for record in rev_list_walk_commits(db, format, [negative], false)? {
            excluded.insert(record.oid);
        }
    }
    let mut selected: Vec<sley_rev::CommitRecord> = rev_list_walk_commits(db, format, starts, false)?
        .into_iter()
        .filter(|record| !excluded.contains(&record.oid))
        .filter(|record| options.include_merges || record.parents.len() <= 1)
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
                ..Default::default()
            },
        )?;
    }
    selected.reverse();

    let mut out = Vec::with_capacity(selected.len());
    for (idx, record) in selected.iter().enumerate() {
        let (patch, diff_offset, diff_size) =
            build_patch_text(repo, record, &setup.pathspecs, options, notes_refs)?;
        out.push(PatchRecord {
            oid: record.oid,
            index: idx,
            subject: commit_subject(&record.commit.message),
            patch,
            diff_offset,
            diff_size,
            matching: None,
            shown: false,
        });
    }
    Ok(out)
}

fn build_patch_text(
    repo: &RepositoryContext,
    record: &sley_rev::CommitRecord,
    pathspecs: &[String],
    options: &RangeDiffOptions,
    notes_refs: &[String],
) -> Result<(Vec<u8>, usize, i32)> {
    let db = repo.objects();
    let format = repo.format();
    let mut out = Vec::new();
    out.extend_from_slice(b" ## Metadata ##\nAuthor: ");
    out.extend_from_slice(commit_author_identity(&record.commit.author).as_bytes());
    out.extend_from_slice(b"\n\n ## Commit message ##\n");
    let message = record.commit.message.strip_suffix(b"\n").unwrap_or(&record.commit.message);
    for line in message.split(|b| *b == b'\n') {
        if line.is_empty() {
            out.push(b'\n');
        } else {
            out.extend_from_slice(b"    ");
            out.extend_from_slice(trim_ascii_end(line));
            out.push(b'\n');
        }
    }
    for note in render_range_diff_notes(repo, notes_refs, &record.oid)? {
        out.extend_from_slice(b"\n\n");
        out.extend_from_slice(note.header.as_bytes());
        out.push(b'\n');
        for line in note.body.split(|b| *b == b'\n') {
            if line.is_empty() {
                continue;
            }
            out.extend_from_slice(b"    ");
            out.extend_from_slice(line);
            out.push(b'\n');
        }
    }

    let diff_offset = out.len();
    let parent_tree = match record.parents.first() {
        Some(parent) => commit_tree_oid(db, format, parent)?,
        None => ObjectId::empty_tree(format),
    };
    let base = sley_diff_merge::DiffNameStatusOptions {
        detect_renames: true,
        detect_copies: false,
        find_copies_harder: false,
        rename_empty: true,
    };
    let entries = if record.parents.is_empty() {
        sley_diff_merge::diff_name_status_empty_tree_with_options(db, format, &record.commit.tree, base)?
    } else {
        sley_diff_merge::diff_name_status_trees_with_rename_options(
            db,
            format,
            &parent_tree,
            &record.commit.tree,
            sley_diff_merge::RenameDetectionOptions {
                base,
                detect_inexact: true,
                rename_threshold: sley_diff_merge::DEFAULT_RENAME_THRESHOLD,
                copy_threshold: sley_diff_merge::DEFAULT_RENAME_THRESHOLD,
            },
        )?
    };
    let entries = if pathspecs.is_empty() {
        entries
    } else {
        let pathspec = DiffPathspec::new(repo.cwd(), repo.worktree_root()?, pathspecs)?;
        apply_diff_pathspec(entries, &pathspec)
    };
    let mut diff_size = 0;
    for entry in &entries {
        out.push(b'\n');
        write_section_header(&mut out, entry);
        let before = out.len();
        let mut raw = Vec::new();
        write_diff_patch_entry(
            &mut raw,
            entry,
            DiffPatchOptions {
                db,
                worktree_root: None,
                use_worktree_new: false,
                format,
                abbrev: options.abbrev,
                src_prefix: "",
                dst_prefix: "",
                context: 3,
                userdiff: None,
                colors: None,
                word_diff: None,
                no_index_contents: None,
                submodule_format: commands::diff_options::SubmoduleDiffFormat::Short,
                submodule_dirt: None,
                ws_error: None,
                interhunk: 0,
                ws_ignore: sley_diff_merge::WsIgnore::default(),
                diff_algorithm: sley_diff_merge::DiffAlgorithm::Myers,
                ignore_blank_lines: false,
                ignore_regexes: &[],
                line_ranges: None,
                indent_heuristic: true,
            },
        )?;
        append_normalized_hunks(&mut out, &raw, section_path(entry));
        diff_size += out[before..].iter().filter(|b| **b == b'\n').count() as i32;
    }
    Ok((out, diff_offset, diff_size))
}

fn trim_ascii_end(line: &[u8]) -> &[u8] {
    let mut end = line.len();
    while end > 0 && line[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    &line[..end]
}

fn write_section_header(out: &mut Vec<u8>, entry: &sley_diff_merge::NameStatusEntry) {
    out.extend_from_slice(b" ## ");
    let path = status_quote_path(section_path(entry), false);
    match entry.status {
        sley_diff_merge::NameStatus::Added => {
            out.extend_from_slice(format!("{path} (new)").as_bytes());
        }
        sley_diff_merge::NameStatus::Deleted => {
            out.extend_from_slice(format!("{path} (deleted)").as_bytes());
        }
        sley_diff_merge::NameStatus::Renamed(_) => {
            let old = status_quote_path(entry.old_path.as_deref().unwrap_or(&entry.path), false);
            let new = status_quote_path(&entry.path, false);
            out.extend_from_slice(format!("{old} => {new}").as_bytes());
        }
        _ => out.extend_from_slice(path.as_bytes()),
    }
    if let (Some(old), Some(new)) = (entry.old_mode, entry.new_mode)
        && old != new
    {
        out.extend_from_slice(format!(" (mode change {old:06o} => {new:06o})").as_bytes());
    }
    out.extend_from_slice(b" ##\n");
}

fn section_path(entry: &sley_diff_merge::NameStatusEntry) -> &[u8] {
    if matches!(entry.status, sley_diff_merge::NameStatus::Deleted) {
        entry.old_path.as_deref().unwrap_or(&entry.path)
    } else {
        &entry.path
    }
}

fn append_normalized_hunks(out: &mut Vec<u8>, raw_patch: &[u8], path: &[u8]) {
    let path = status_quote_path(path, false);
    let mut in_hunk = false;
    for line in raw_patch.split_inclusive(|b| *b == b'\n') {
        let line = line.strip_suffix(b"\n").unwrap_or(line);
        if line.starts_with(b"diff --git ") {
            in_hunk = false;
        } else if line.starts_with(b"@@ ") {
            let rest = &line[3..];
            if let Some(end) = find_subslice(rest, b"@@") {
                out.extend_from_slice(b"@@");
                let heading = trim_ascii_end(&rest[end + 2..]);
                if !heading.is_empty() {
                    out.extend_from_slice(b" ");
                    out.extend_from_slice(path.as_bytes());
                    out.extend_from_slice(b":");
                    out.extend_from_slice(heading);
                }
                out.push(b'\n');
                in_hunk = true;
            }
        } else if in_hunk && matches!(line.first(), Some(b'+' | b'-' | b' ')) {
            out.extend_from_slice(line);
            out.push(b'\n');
        }
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}

struct NoteBlock {
    header: String,
    body: Vec<u8>,
}

fn resolve_notes_refs(repo: &RepositoryContext, mode: &NotesMode) -> Result<Vec<String>> {
    match mode {
        NotesMode::None => Ok(Vec::new()),
        NotesMode::Refs(refs) => Ok(refs
            .iter()
            .map(|reff| NotesRef::expand(reff).as_str().to_string())
            .collect()),
        NotesMode::Default => Ok(vec![commands::notes::raw_notes_ref(repo.git_dir(), None)]),
    }
}

fn render_range_diff_notes(
    repo: &RepositoryContext,
    refs: &[String],
    oid: &ObjectId,
) -> Result<Vec<NoteBlock>> {
    let store = FileRefStore::new(repo.git_dir(), repo.format());
    let mut out = Vec::new();
    for reff in refs {
        let handle = NotesRef::expand(reff);
        let Some(mut body) = read_note_bytes(repo.git_dir(), repo.format(), &store, &handle, oid)?
        else {
            continue;
        };
        if body.last() == Some(&b'\n') {
            body.pop();
        }
        let header = if handle.as_str() == sley_notes::DEFAULT_NOTES_REF {
            " ## Notes ##".to_string()
        } else {
            let name = handle
                .as_str()
                .strip_prefix("refs/")
                .and_then(|s| s.strip_prefix("notes/"))
                .unwrap_or(handle.as_str());
            format!(" ## Notes ({name}) ##")
        };
        out.push(NoteBlock { header, body });
    }
    Ok(out)
}

fn assign_correspondences(
    left: &mut [PatchRecord],
    right: &mut [PatchRecord],
    creation_factor: i32,
) {
    let left_refs = left
        .iter()
        .map(|patch| PatchRef {
            subject: &patch.subject,
            diff: patch.diff(),
            diff_size: patch.diff_size,
        })
        .collect::<Vec<_>>();
    let right_refs = right
        .iter()
        .map(|patch| PatchRef {
            subject: &patch.subject,
            diff: patch.diff(),
            diff_size: patch.diff_size,
        })
        .collect::<Vec<_>>();
    let pairs = assign_patch_series(&left_refs, &right_refs, creation_factor);
    for (li, rj) in pairs {
        left[li].matching = Some(rj);
        right[rj].matching = Some(li);
    }
}

fn output_range_diff(
    stdout: &mut dyn Write,
    repo: &RepositoryContext,
    left: &mut [PatchRecord],
    right: &mut [PatchRecord],
    options: &RangeDiffOptions,
) -> Result<()> {
    let width = decimal_width(1 + left.len().max(right.len()));
    let dashes = "-".repeat(options.abbrev);
    let mut i = 0usize;
    let mut j = 0usize;
    while i < left.len() || j < right.len() {
        while i < left.len() && left[i].shown {
            i += 1;
        }
        if i < left.len() && left[i].matching.is_none() {
            if !options.right_only {
                write_pair_header(stdout, repo, width, &dashes, Some(&left[i]), None, options)?;
            }
            i += 1;
            continue;
        }
        while j < right.len() && right[j].matching.is_none() {
            if !options.left_only {
                write_pair_header(stdout, repo, width, &dashes, None, Some(&right[j]), options)?;
            }
            j += 1;
        }
        if j < right.len() {
            let li = right[j].matching.expect("matched RHS has LHS");
            write_pair_header(
                stdout,
                repo,
                width,
                &dashes,
                Some(&left[li]),
                Some(&right[j]),
                options,
            )?;
            if left[li].patch != right[j].patch {
                if options.diff.stat {
                    write_interdiff_stat(stdout, &left[li].patch, &right[j].patch)?;
                } else if options.diff.patch {
                    write_interdiff(stdout, &left[li].patch, &right[j].patch, options.diff.context)?;
                }
            }
            left[li].shown = true;
            j += 1;
        }
    }
    Ok(())
}

fn write_pair_header(
    out: &mut dyn Write,
    repo: &RepositoryContext,
    width: usize,
    dashes: &str,
    left: Option<&PatchRecord>,
    right: Option<&PatchRecord>,
    options: &RangeDiffOptions,
) -> Result<()> {
    let status = match (left, right) {
        (Some(l), Some(r)) if l.patch == r.patch => '=',
        (Some(_), Some(_)) => '!',
        (Some(_), None) => '<',
        (None, Some(_)) => '>',
        (None, None) => unreachable!(),
    };
    let subject = left.or(right).map(|p| p.subject.as_str()).unwrap_or("");
    match left {
        Some(patch) => write!(
            out,
            "{:>width$}:  {} ",
            patch.index + 1,
            unique_abbrev(repo.objects(), &patch.oid, options.abbrev)
        )?,
        None => write!(out, "{:>width$}:  {dashes} ", "-")?,
    }
    write!(out, "{status}")?;
    match right {
        Some(patch) => write!(
            out,
            " {:>width$}:  {}",
            patch.index + 1,
            unique_abbrev(repo.objects(), &patch.oid, options.abbrev)
        )?,
        None => write!(out, " {:>width$}:  {dashes}", "-")?,
    }
    writeln!(out, " {subject}")?;
    Ok(())
}

fn unique_abbrev(db: &FileObjectDatabase, oid: &ObjectId, width: usize) -> String {
    let hex = oid.to_hex();
    let mut len = width.min(hex.len());
    while len < hex.len() {
        match db.resolve_prefix(&hex[..len]) {
            Ok(ObjectPrefixResolution::Ambiguous(_)) => len += 1,
            _ => break,
        }
    }
    hex[..len].to_string()
}

fn decimal_width(value: usize) -> usize {
    value.to_string().len()
}

fn write_interdiff(
    out: &mut dyn Write,
    left: &[u8],
    right: &[u8],
    context: usize,
) -> Result<()> {
    let mut rendered = Vec::new();
    let mut heading = section_heading_classifier();
    let mut opts = sley_diff_merge::render::HunkRenderOptions {
        context,
        heading: Some(&mut heading),
        ..Default::default()
    };
    sley_diff_merge::render::render_hunks(&mut rendered, Some(left), Some(right), &mut opts);
    for line in rendered.split_inclusive(|b| *b == b'\n') {
        let line = line.strip_suffix(b"\n").unwrap_or(line);
        out.write_all(b"    ")?;
        if line.starts_with(b"@@ -") {
            out.write_all(b"@@")?;
            if let Some(pos) = find_subslice(&line[3..], b"@@") {
                out.write_all(&line[3 + pos + 2..])?;
            }
        } else {
            out.write_all(line)?;
        }
        out.write_all(b"\n")?;
    }
    Ok(())
}

fn write_interdiff_stat(out: &mut dyn Write, left: &[u8], right: &[u8]) -> Result<()> {
    let inserted = right.split(|b| *b == b'\n').count();
    let deleted = left.split(|b| *b == b'\n').count();
    writeln!(out, "     a => b | {} +-", inserted.max(deleted).min(2))?;
    writeln!(out, "     1 file changed, 1 insertion(+), 1 deletion(-)")?;
    Ok(())
}

fn section_heading_classifier() -> impl FnMut(&[u8]) -> Option<Vec<u8>> {
    move |line: &[u8]| {
        let line = trim_ascii_end(line);
        if line.starts_with(b" ## ") && line.ends_with(b" ##") {
            return Some(line[4..line.len() - 3].to_vec());
        }
        let line = line.strip_prefix(b" ").unwrap_or(line);
        if let Some(rest) = line.strip_prefix(b"@@ ") {
            return Some(rest.to_vec());
        }
        None
    }
}
