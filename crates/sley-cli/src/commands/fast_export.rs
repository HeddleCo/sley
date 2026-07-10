//! `git fast-export` — emit a fast-import stream for the given revisions.

use crate::*;
use sley::plumbing::sley_diff_merge::{
    DiffNameStatusOptions, NameStatus, NameStatusEntry, diff_name_status_empty_tree_with_options,
    diff_name_status_trees_with_options,
};
use sley::plumbing::sley_rev::revlist::{rev_list_topo_order, rev_list_walk_commits_with_missing};
use sley::plumbing::sley_rev::{
    CommitRecord, SimplifyOptions, ancestry_path_on_set, peel_to_commit,
    simplify_history_with_bottoms,
};
use sley_pathspec::normalized_revwalk_pathspec;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum SignMode {
    #[default]
    Abort,
    Warn,
    WarnVerbatim,
    Verbatim,
    WarnStrip,
    Strip,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum TagOfFilteredMode {
    #[default]
    Abort,
    Drop,
    Rewrite,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum ReencodeMode {
    #[default]
    Abort,
    Yes,
    No,
}

struct FastExportOptions {
    progress: Option<usize>,
    signed_tags: SignMode,
    signed_commits: SignMode,
    tag_of_filtered: TagOfFilteredMode,
    reencode: ReencodeMode,
    export_marks: Option<PathBuf>,
    import_marks: Option<PathBuf>,
    import_marks_if_exists: Option<PathBuf>,
    fake_missing_tagger: bool,
    full_tree: bool,
    use_done_feature: bool,
    no_data: bool,
    reference_excluded_parents: bool,
    show_original_ids: bool,
    mark_tags: bool,
    diff: DiffNameStatusOptions,
    refspecs: Vec<String>,
    end_of_options: bool,
}

impl Default for FastExportOptions {
    fn default() -> Self {
        Self {
            signed_commits: SignMode::Strip,
            diff: DiffNameStatusOptions {
                detect_renames: false,
                detect_copies: false,
                find_copies_harder: false,
                rename_empty: true,
                detect_inexact: false,
                rename_threshold: sley_diff_merge::DEFAULT_RENAME_THRESHOLD,
                copy_threshold: sley_diff_merge::DEFAULT_RENAME_THRESHOLD,
                rename_limit: 0,
            },
            progress: None,
            signed_tags: SignMode::Abort,
            tag_of_filtered: TagOfFilteredMode::Abort,
            reencode: ReencodeMode::Abort,
            export_marks: None,
            import_marks: None,
            import_marks_if_exists: None,
            fake_missing_tagger: false,
            full_tree: false,
            use_done_feature: false,
            no_data: false,
            reference_excluded_parents: false,
            show_original_ids: false,
            mark_tags: false,
            refspecs: Vec::new(),
            end_of_options: false,
        }
    }
}

#[derive(Clone)]
enum PendingRefTarget {
    Commit(ObjectId),
    Tag(ObjectId),
}

#[derive(Clone)]
struct PendingRef {
    name: String,
    target: PendingRefTarget,
}

struct ExportedCommitMessage {
    bytes: Vec<u8>,
    preserve_encoding: bool,
}

pub(crate) fn cmd_fast_export(
    cli_session: &crate::session::CliSession,
    args: &[String],
) -> Result<()> {
    let git_dir = cli_session.git_dir()?;
    let format = repository_object_format(&git_dir)?;
    let db = FileObjectDatabase::from_git_dir(&git_dir, format);
    let store = FileRefStore::new(&git_dir, format);
    let config = read_repo_config(&git_dir)?;
    let cwd = env::current_dir()?;
    let worktree_root = worktree_root_for_git_dir(&git_dir).ok();

    let (options, setup_args) = parse_fast_export_args(args)?;
    if options.import_marks.is_some() && options.import_marks_if_exists.is_some() {
        eprintln!(
            "fatal: options '--import-marks' and '--import-marks-if-exists' cannot be used together"
        );
        return Err(GitError::Exit(128));
    }

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
            "unsupported fast-export option {leftover}"
        )));
    }

    let mut revision_options = setup.options;
    revision_options.order = sley_rev::RevisionOrder::Topo;
    if !revision_options.has_revisions() {
        if has_delete_refspec(&options) {
            let mut out = io::BufWriter::new(io::stdout());
            if options.use_done_feature {
                writeln!(out, "feature done")?;
            }
            emit_delete_refspecs(&mut out, &options.refspecs, format)?;
            if options.use_done_feature {
                writeln!(out, "done")?;
            }
            out.flush()?;
            return Ok(());
        }
        return Err(GitError::Command(
            "fast-export requires at least one revision".into(),
        ));
    }

    validate_fast_export_tag_refs(&db, &store, format, &list_all_tag_refs(&store)?)?;

    let mut imported_commit_marks = HashMap::new();
    let mut import_mark_start = 0u64;
    if let Some(path) = &options.import_marks {
        import_mark_start = import_marks_into(&db, format, path, &mut imported_commit_marks)?;
    } else if let Some(path) = &options.import_marks_if_exists {
        if path.exists() {
            import_mark_start = import_marks_into(&db, format, path, &mut imported_commit_marks)?;
        }
    }

    let shown: HashSet<ObjectId> = imported_commit_marks.keys().copied().collect();
    let mut exporter = FastExporter {
        db,
        store,
        format,
        options,
        next_mark: import_mark_start.saturating_add(1).max(1),
        commit_marks: imported_commit_marks,
        blob_marks: HashMap::new(),
        tag_marks: HashMap::new(),
        shown,
        revision_sources: HashMap::new(),
        default_ref_name: None,
        initialized_refs: HashSet::new(),
        path_limited: !setup.pathspecs.is_empty(),
        pending_refs: Vec::new(),
        nested_tag_refs: Vec::new(),
        progress_counter: 0,
        out: io::BufWriter::new(io::stdout()),
    };

    if exporter.options.use_done_feature {
        writeln!(exporter.out, "feature done")?;
    }

    exporter.seed_pending_refs(&revision_options.positives)?;

    let commits =
        exporter.collect_commits(&revision_options, &setup.pathspecs, &worktree_root, &cwd)?;

    for record in commits {
        exporter.export_commit(&record)?;
    }

    exporter
        .pending_refs
        .sort_by(|left, right| left.name.cmp(&right.name));
    for pending in exporter.pending_refs.clone() {
        exporter.emit_pending_ref(&pending)?;
    }
    for (name, tag_oid) in exporter.nested_tag_refs.clone() {
        exporter.emit_tag_ref(&name, tag_oid)?;
    }
    exporter.emit_deletes()?;

    if let Some(path) = &exporter.options.export_marks {
        if exporter.next_mark != import_mark_start {
            export_marks(path, &exporter.commit_marks, exporter.format)?;
        }
    }

    if exporter.options.use_done_feature {
        writeln!(exporter.out, "done")?;
    }
    exporter.out.flush()?;
    Ok(())
}

struct FastExporter {
    db: FileObjectDatabase,
    store: FileRefStore,
    format: ObjectFormat,
    options: FastExportOptions,
    next_mark: u64,
    commit_marks: HashMap<ObjectId, u64>,
    blob_marks: HashMap<ObjectId, u64>,
    tag_marks: HashMap<ObjectId, u64>,
    shown: HashSet<ObjectId>,
    revision_sources: HashMap<ObjectId, String>,
    default_ref_name: Option<String>,
    initialized_refs: HashSet<String>,
    path_limited: bool,
    pending_refs: Vec<PendingRef>,
    nested_tag_refs: Vec<(String, ObjectId)>,
    progress_counter: usize,
    out: io::BufWriter<io::Stdout>,
}

impl FastExporter {
    fn seed_pending_refs(&mut self, positives: &[sley_rev::RevisionTip]) -> Result<()> {
        for tip in positives {
            let Some(source_name) = tip.source_name.as_ref() else {
                continue;
            };
            let object = self.db.read_object(&tip.oid)?;
            match object.object_type {
                ObjectType::Commit => {
                    let ref_name = self.export_ref_name_for_source(source_name)?;
                    if self.default_ref_name.is_none() {
                        self.default_ref_name = Some(ref_name.clone());
                    }
                    self.note_revision_source(tip.oid, ref_name.clone());
                    self.pending_refs.push(PendingRef {
                        name: ref_name,
                        target: PendingRefTarget::Commit(tip.oid),
                    });
                }
                ObjectType::Tag => {
                    let commit = peel_to_commit(&self.db, self.format, &tip.oid)?;
                    let ref_name = self.export_ref_name_for_source(source_name)?;
                    if self.default_ref_name.is_none() {
                        self.default_ref_name = Some(ref_name.clone());
                    }
                    self.note_revision_source(commit, ref_name.clone());
                    self.nested_tag_refs.push((ref_name, tip.oid));
                }
                ObjectType::Blob => {
                    self.export_blob(tip.oid)?;
                }
                ObjectType::Tree => {}
            }
        }
        Ok(())
    }

    fn note_revision_source(&mut self, commit: ObjectId, source_name: String) {
        self.revision_sources.entry(commit).or_insert(source_name);
    }

    fn export_ref_name_for_source(&self, source_name: &str) -> Result<String> {
        let ref_name = self.normalize_source_ref(source_name)?;
        Ok(self.apply_refspecs(&ref_name))
    }

    fn normalize_source_ref(&self, source_name: &str) -> Result<String> {
        if source_name == "HEAD" {
            if let Some(branch) = self.store.current_branch_ref()? {
                return Ok(branch);
            }
            return Ok(source_name.to_string());
        }
        if source_name.starts_with("refs/") {
            return Ok(source_name.to_string());
        }
        for candidate in [
            format!("refs/{source_name}"),
            format!("refs/tags/{source_name}"),
            format!("refs/heads/{source_name}"),
            format!("refs/remotes/{source_name}"),
            format!("refs/remotes/{source_name}/HEAD"),
        ] {
            if self.store.read_ref(&candidate)?.is_some() {
                return Ok(candidate);
            }
        }
        Ok(source_name.to_string())
    }

    fn apply_refspecs(&self, ref_name: &str) -> String {
        for refspec in &self.options.refspecs {
            let Some((src, dst)) = refspec.split_once(':') else {
                continue;
            };
            if src.is_empty() || dst.is_empty() {
                continue;
            }
            if src == ref_name {
                return dst.to_string();
            }
        }
        ref_name.to_string()
    }

    fn collect_commits(
        &self,
        revision_options: &sley_rev::RevisionOptions,
        pathspecs: &[String],
        worktree_root: &Option<PathBuf>,
        cwd: &Path,
    ) -> Result<Vec<CommitRecord>> {
        let mut include = Vec::new();
        for tip in &revision_options.positives {
            if let Ok(commit) = peel_to_commit(&self.db, self.format, &tip.oid) {
                include.push(commit);
            }
        }
        if include.is_empty() {
            return Ok(Vec::new());
        }

        let first_parent = revision_options.first_parent;
        let records = rev_list_walk_commits_with_missing(
            &self.db,
            self.format,
            include,
            first_parent,
            sley_rev::revlist::RevListMissingAction::Error,
        )?;

        let mut excluded = HashSet::new();
        for oid in &revision_options.negatives {
            for record in rev_list_walk_commits_with_missing(
                &self.db,
                self.format,
                [*oid],
                first_parent,
                sley_rev::revlist::RevListMissingAction::Error,
            )? {
                excluded.insert(record.oid);
            }
        }

        let mut selected: Vec<CommitRecord> = records
            .into_iter()
            .filter(|record| !excluded.contains(&record.oid))
            .filter(|record| !self.commit_marks.contains_key(&record.oid))
            .collect();

        if revision_options.ancestry_path && !revision_options.negatives.is_empty() {
            let on_path = ancestry_path_on_set(
                selected
                    .iter()
                    .map(|record| (record.oid, record.parents.clone())),
                &revision_options.negatives,
            );
            selected.retain(|record| on_path.contains(&record.oid));
        }

        if !pathspecs.is_empty()
            || revision_options.full_history
            || revision_options.simplify_merges
        {
            let pathspec = normalized_revwalk_pathspec(
                cwd,
                worktree_root.as_deref(),
                pathspecs,
                effective_pathspec_flags(),
            )?;
            let simplify = SimplifyOptions {
                full_history: revision_options.full_history,
                first_parent: revision_options.first_parent,
                simplify_merges: revision_options.simplify_merges,
                show_pulls: revision_options.show_pulls,
                ancestry_path: revision_options.ancestry_path,
                want_ancestry: true,
            };
            let bottoms: HashSet<ObjectId> = revision_options.negatives.iter().copied().collect();
            selected = simplify_history_with_bottoms(
                &self.db,
                self.format,
                selected,
                &pathspec,
                simplify,
                &bottoms,
            )?;
        }

        let refs: Vec<&CommitRecord> = selected.iter().collect();
        let ordered = rev_list_topo_order(refs)?;
        let mut commits = ordered.into_iter().cloned().collect::<Vec<_>>();
        commits.reverse();
        Ok(commits)
    }

    fn export_commit(&mut self, record: &CommitRecord) -> Result<()> {
        if !self.shown.insert(record.oid) {
            return Ok(());
        }

        let ref_name = self
            .revision_sources
            .get(&record.oid)
            .cloned()
            .or_else(|| self.default_ref_name.clone())
            .unwrap_or_else(|| "refs/heads/main".to_string());
        self.pending_refs.retain(|pending| pending.name != ref_name);

        let parent_tree = record.parents.first().and_then(|parent| {
            self.db
                .read_object(parent)
                .ok()
                .and_then(|object| Commit::parse(self.format, &object.body).ok())
                .map(|commit| commit.tree)
        });

        let use_parent_diff = record.parents.first().is_some_and(|parent| {
            self.commit_marks.contains_key(parent) || self.options.reference_excluded_parents
        }) && !self.options.full_tree;

        let changes = if use_parent_diff {
            let parent_tree = parent_tree.ok_or_else(|| {
                GitError::Command(format!(
                    "fast-export: missing parent tree for {}",
                    record.oid
                ))
            })?;
            diff_name_status_trees_with_options(
                &self.db,
                self.format,
                &parent_tree,
                &record.commit.tree,
                self.options.diff,
            )?
        } else {
            diff_name_status_empty_tree_with_options(
                &self.db,
                self.format,
                &record.commit.tree,
                self.options.diff,
            )?
        };

        for entry in &changes {
            if let Some(oid) = entry.new_oid {
                if entry
                    .new_mode
                    .is_some_and(|mode| mode & 0o170000 == 0o160000)
                {
                    continue;
                }
                self.export_blob(oid)?;
            }
        }

        let first_commit_on_ref = self.initialized_refs.insert(ref_name.clone());
        if first_commit_on_ref && self.commit_ref_needs_reset(&ref_name)? {
            writeln!(self.out, "reset {ref_name}")?;
        } else if record.parents.is_empty() {
            writeln!(self.out, "reset {ref_name}")?;
        }

        let mark = self.alloc_mark();
        self.commit_marks.insert(record.oid, mark);

        writeln!(self.out, "commit {ref_name}")?;
        writeln!(self.out, "mark :{mark}")?;
        if self.options.show_original_ids {
            writeln!(self.out, "original-oid {}", record.oid.to_hex())?;
        }

        let message = self.exported_commit_message(&record.commit, record.oid)?;
        self.write_commit_identities(&record.commit, message.preserve_encoding)?;
        self.write_commit_message(&message.bytes)?;

        for (index, parent) in record.parents.iter().enumerate() {
            let prefix = if index == 0 { "from" } else { "merge" };
            if let Some(parent_mark) = self.commit_marks.get(parent) {
                writeln!(self.out, "{prefix} :{parent_mark}")?;
            } else if self.options.reference_excluded_parents {
                writeln!(self.out, "{prefix} {}", parent.to_hex())?;
            }
        }

        if self.options.full_tree {
            writeln!(self.out, "deleteall")?;
        }

        let mut changed_paths = HashSet::<Vec<u8>>::new();
        self.emit_file_changes(&changes, &mut changed_paths)?;
        writeln!(self.out)?;
        self.show_progress();
        Ok(())
    }

    fn write_commit_identities(&mut self, commit: &Commit, preserve_encoding: bool) -> Result<()> {
        self.out.write_all(b"author ")?;
        self.out.write_all(&commit.author)?;
        self.out.write_all(b"\ncommitter ")?;
        self.out.write_all(&commit.committer)?;
        self.out.write_all(b"\n")?;
        if preserve_encoding && let Some(encoding) = &commit.encoding {
            self.out.write_all(b"encoding ")?;
            self.out.write_all(encoding)?;
            self.out.write_all(b"\n")?;
        }
        Ok(())
    }

    fn commit_ref_needs_reset(&self, ref_name: &str) -> Result<bool> {
        let Some(RefTarget::Direct(oid)) = self.store.read_ref(ref_name)? else {
            return Ok(false);
        };
        let object = self.db.read_object(&oid)?;
        Ok(object.object_type != ObjectType::Commit)
    }

    fn exported_commit_message(
        &self,
        commit: &Commit,
        commit_oid: ObjectId,
    ) -> Result<ExportedCommitMessage> {
        let (bytes, preserve_encoding) = match (&commit.encoding, self.options.reencode) {
            (Some(encoding), ReencodeMode::Yes) => {
                let from = String::from_utf8_lossy(encoding);
                match reencode_commit_message(&commit.message, from.as_ref(), "UTF-8") {
                    Some(reencoded) => (reencoded, false),
                    None => (commit.message.clone(), true),
                }
            }
            (Some(encoding), ReencodeMode::Abort) => {
                eprintln!(
                    "fatal: encountered commit-specific encoding {} in commit {}; use --reencode=[yes|no] to handle it",
                    String::from_utf8_lossy(encoding),
                    commit_oid,
                );
                return Err(GitError::Exit(128));
            }
            (Some(_), ReencodeMode::No) => (commit.message.clone(), true),
            _ => (commit.message.clone(), false),
        };
        Ok(ExportedCommitMessage {
            bytes,
            preserve_encoding,
        })
    }

    fn write_commit_message(&mut self, message: &[u8]) -> Result<()> {
        writeln!(self.out, "data {}", message.len())?;
        self.out.write_all(message)?;
        Ok(())
    }

    fn emit_file_changes(
        &mut self,
        changes: &[NameStatusEntry],
        changed_paths: &mut HashSet<Vec<u8>>,
    ) -> Result<()> {
        let mut sorted = changes.to_vec();
        sorted.sort_by(|left, right| depth_first_cmp(left, right));

        for entry in sorted {
            match entry.status {
                NameStatus::Deleted => {
                    write!(self.out, "D ")?;
                    write_fast_export_path(&mut self.out, entry.path.as_bytes())?;
                    writeln!(self.out)?;
                    changed_paths.insert(entry.path.to_vec());
                }
                NameStatus::Renamed(_) | NameStatus::Copied(_) => {
                    let old_path = entry.old_path.as_ref().map(|path| path.as_bytes());
                    let can_declare = old_path.is_none_or(|old| !changed_paths.contains(old));
                    if can_declare {
                        write!(self.out, "{} ", entry.status.code())?;
                        if let Some(old_path) = old_path {
                            write_fast_export_path(&mut self.out, old_path)?;
                            self.out.write_all(b" ")?;
                        }
                        write_fast_export_path(&mut self.out, entry.path.as_bytes())?;
                        writeln!(self.out)?;
                        changed_paths.insert(entry.path.to_vec());
                        if same_blob_and_mode(&entry) {
                            continue;
                        }
                    }
                    self.emit_modify_line(&entry, changed_paths)?;
                }
                NameStatus::Added
                | NameStatus::Modified
                | NameStatus::TypeChanged
                | NameStatus::Unmerged => {
                    self.emit_modify_line(&entry, changed_paths)?;
                }
            }
        }
        Ok(())
    }

    fn emit_modify_line(
        &mut self,
        entry: &NameStatusEntry,
        changed_paths: &mut HashSet<Vec<u8>>,
    ) -> Result<()> {
        let mode = entry.new_mode.unwrap_or(0o100644);
        let oid = entry
            .new_oid
            .ok_or_else(|| GitError::Command("fast-export: file change missing new oid".into()))?;
        write!(self.out, "M {:o} ", mode)?;
        if self.options.no_data || mode & 0o170000 == 0o160000 {
            write!(self.out, "{} ", oid.to_hex())?;
        } else {
            let mark = self.blob_marks.get(&oid).copied().ok_or_else(|| {
                GitError::Command(format!("fast-export: missing blob mark for {oid}"))
            })?;
            write!(self.out, ":{mark} ")?;
        }
        write_fast_export_path(&mut self.out, entry.path.as_bytes())?;
        writeln!(self.out)?;
        changed_paths.insert(entry.path.to_vec());
        Ok(())
    }

    fn export_blob(&mut self, oid: ObjectId) -> Result<u64> {
        if self.options.no_data {
            return Err(GitError::Command(
                "fast-export: internal blob export with --no-data".into(),
            ));
        }
        if let Some(mark) = self.blob_marks.get(&oid).copied() {
            return Ok(mark);
        }
        let object = self.db.read_object(&oid)?;
        if object.object_type != ObjectType::Blob {
            return Err(GitError::Command(format!(
                "fast-export: {oid} is not a blob"
            )));
        }
        let mark = self.alloc_mark();
        self.blob_marks.insert(oid, mark);
        writeln!(self.out, "blob")?;
        writeln!(self.out, "mark :{mark}")?;
        if self.options.show_original_ids {
            writeln!(self.out, "original-oid {}", oid.to_hex())?;
        }
        writeln!(self.out, "data {}", object.body.len())?;
        self.out.write_all(&object.body)?;
        self.out.write_all(b"\n")?;
        self.show_progress();
        Ok(mark)
    }

    fn emit_pending_ref(&mut self, pending: &PendingRef) -> Result<()> {
        match &pending.target {
            PendingRefTarget::Commit(commit) => {
                let rewritten = self.rewrite_commit_for_ref(*commit)?;
                let Some(commit) = rewritten else {
                    write!(
                        self.out,
                        "reset {}\nfrom {}\n\n",
                        pending.name,
                        ObjectId::null(self.format).to_hex()
                    )?;
                    return Ok(());
                };
                if let Some(mark) = self.commit_marks.get(&commit) {
                    write!(self.out, "reset {}\nfrom :{mark}\n\n", pending.name)?;
                    self.show_progress();
                } else if self.options.reference_excluded_parents {
                    write!(
                        self.out,
                        "reset {}\nfrom {}\n\n",
                        pending.name,
                        commit.to_hex()
                    )?;
                } else {
                    write!(
                        self.out,
                        "reset {}\nfrom {}\n\n",
                        pending.name,
                        ObjectId::null(self.format).to_hex()
                    )?;
                }
            }
            PendingRefTarget::Tag(tag_oid) => {
                self.emit_tag_ref(&pending.name, *tag_oid)?;
            }
        }
        Ok(())
    }

    fn emit_tag_ref(&mut self, full_name: &str, tag_oid: ObjectId) -> Result<()> {
        let object = self.db.read_object(&tag_oid)?;
        if object.object_type != ObjectType::Tag {
            return Ok(());
        }
        let tag = Tag::parse(self.format, &object.body)?;
        let mut tagged_oid = tag.object;
        let mut tagged_type = tag.object_type;
        while tagged_type == ObjectType::Tag {
            let nested = self.db.read_object(&tagged_oid)?;
            let nested_tag = Tag::parse(self.format, &nested.body)?;
            tagged_oid = nested_tag.object;
            tagged_type = nested_tag.object_type;
        }
        if tagged_type == ObjectType::Tree {
            eprintln!(
                "warning: omitting tag {tag_oid},\nsince tags of trees (or tags of tags of trees, etc.) are not supported."
            );
            return Ok(());
        }

        let tagged_mark = match tagged_type {
            ObjectType::Commit => self.commit_marks.get(&tagged_oid).copied(),
            ObjectType::Blob => self.blob_marks.get(&tagged_oid).copied(),
            ObjectType::Tag => self.tag_marks.get(&tagged_oid).copied(),
            ObjectType::Tree => None,
        };
        if tagged_mark.is_none() {
            return self.handle_filtered_tag(full_name, tag_oid, &tag, tagged_oid, tagged_type);
        }

        let display_name = full_name.strip_prefix("refs/tags/").unwrap_or(full_name);
        writeln!(self.out, "tag {display_name}")?;
        if self.options.mark_tags {
            let mark = self.alloc_mark();
            self.tag_marks.insert(tag_oid, mark);
            writeln!(self.out, "mark :{mark}")?;
        }
        if let Some(mark) = tagged_mark {
            writeln!(self.out, "from :{mark}")?;
        } else {
            writeln!(self.out, "from {}", tagged_oid.to_hex())?;
        }
        if self.options.show_original_ids {
            writeln!(self.out, "original-oid {}", tag_oid.to_hex())?;
        }
        if let Some(tagger) = &tag.tagger {
            self.out.write_all(b"tagger ")?;
            self.out.write_all(tagger)?;
            self.out.write_all(b"\n")?;
        } else if self.options.fake_missing_tagger {
            writeln!(
                self.out,
                "tagger Unspecified Tagger <unspecified-tagger> 0 +0000"
            )?;
        }
        let message = self.tag_message_for_export(&tag.message)?;
        writeln!(self.out, "data {}", message.len())?;
        self.out.write_all(&message)?;
        writeln!(self.out)?;
        Ok(())
    }

    fn handle_filtered_tag(
        &mut self,
        full_name: &str,
        tag_oid: ObjectId,
        tag: &Tag,
        tagged_oid: ObjectId,
        tagged_type: ObjectType,
    ) -> Result<()> {
        match self.options.tag_of_filtered {
            TagOfFilteredMode::Abort => {
                eprintln!(
                    "fatal: tag {tag_oid} tags unexported object; use --tag-of-filtered-object=<mode> to handle it"
                );
                Err(GitError::Exit(128))
            }
            TagOfFilteredMode::Drop => Ok(()),
            TagOfFilteredMode::Rewrite => match tagged_type {
                ObjectType::Commit => {
                    let rewritten = if self.path_limited {
                        self.rewrite_filtered_tag_commit(tagged_oid)?
                    } else {
                        self.rewrite_commit_for_ref(tagged_oid)?
                    };
                    let Some(commit) = rewritten else {
                        let display_name =
                            full_name.strip_prefix("refs/tags/").unwrap_or(full_name);
                        writeln!(
                            self.out,
                            "reset {}\nfrom {}\n\n",
                            display_name,
                            ObjectId::null(self.format).to_hex()
                        )?;
                        return Ok(());
                    };
                    if let Some(mark) = self.commit_marks.get(&commit) {
                        self.emit_rewritten_tag(full_name, tag_oid, tag, *mark)?;
                    } else {
                        self.emit_rewritten_tag_oid(full_name, tag_oid, tag, commit)?;
                    }
                    Ok(())
                }
                ObjectType::Tag if !self.options.mark_tags => {
                    eprintln!("fatal: cannot export nested tags unless --mark-tags is specified.");
                    Err(GitError::Exit(128))
                }
                ObjectType::Blob => {
                    self.emit_rewritten_tag(full_name, tag_oid, tag, 0)?;
                    Ok(())
                }
                _ => Ok(()),
            },
        }
    }

    fn emit_rewritten_tag_oid(
        &mut self,
        full_name: &str,
        tag_oid: ObjectId,
        tag: &Tag,
        from_oid: ObjectId,
    ) -> Result<()> {
        let display_name = full_name.strip_prefix("refs/tags/").unwrap_or(full_name);
        writeln!(self.out, "tag {display_name}")?;
        if self.options.mark_tags {
            let mark = self.alloc_mark();
            self.tag_marks.insert(tag_oid, mark);
            writeln!(self.out, "mark :{mark}")?;
        }
        writeln!(self.out, "from {}", from_oid.to_hex())?;
        if self.options.show_original_ids {
            writeln!(self.out, "original-oid {}", tag_oid.to_hex())?;
        }
        if let Some(tagger) = &tag.tagger {
            self.out.write_all(b"tagger ")?;
            self.out.write_all(tagger)?;
            self.out.write_all(b"\n")?;
        } else if self.options.fake_missing_tagger {
            writeln!(
                self.out,
                "tagger Unspecified Tagger <unspecified-tagger> 0 +0000"
            )?;
        }
        let message = self.tag_message_for_export(&tag.message)?;
        writeln!(self.out, "data {}", message.len())?;
        self.out.write_all(&message)?;
        writeln!(self.out)?;
        Ok(())
    }

    fn emit_rewritten_tag(
        &mut self,
        full_name: &str,
        tag_oid: ObjectId,
        tag: &Tag,
        from_mark: u64,
    ) -> Result<()> {
        let display_name = full_name.strip_prefix("refs/tags/").unwrap_or(full_name);
        writeln!(self.out, "tag {display_name}")?;
        if self.options.mark_tags {
            let mark = self.alloc_mark();
            self.tag_marks.insert(tag_oid, mark);
            writeln!(self.out, "mark :{mark}")?;
        }
        if from_mark != 0 {
            writeln!(self.out, "from :{from_mark}")?;
        } else {
            writeln!(self.out, "from {}", tag.object.to_hex())?;
        }
        if let Some(tagger) = &tag.tagger {
            self.out.write_all(b"tagger ")?;
            self.out.write_all(tagger)?;
            self.out.write_all(b"\n")?;
        } else if self.options.fake_missing_tagger {
            writeln!(
                self.out,
                "tagger Unspecified Tagger <unspecified-tagger> 0 +0000"
            )?;
        }
        let message = self.tag_message_for_export(&tag.message)?;
        writeln!(self.out, "data {}", message.len())?;
        self.out.write_all(&message)?;
        writeln!(self.out)?;
        Ok(())
    }

    fn tag_message_for_export(&self, message: &[u8]) -> Result<Vec<u8>> {
        let sig_start = tag_signature_start(message);
        let exported = &message[..sig_start];
        match self.options.signed_tags {
            SignMode::Abort if sig_start < message.len() => {
                eprintln!("fatal: encountered signed tag; use --signed-tags=<mode> to handle it");
                Err(GitError::Exit(128))
            }
            SignMode::Warn | SignMode::WarnVerbatim if sig_start < message.len() => {
                eprintln!("warning: exporting signed tag");
                Ok(message.to_vec())
            }
            SignMode::Verbatim if sig_start < message.len() => Ok(message.to_vec()),
            SignMode::WarnStrip | SignMode::Strip if sig_start < message.len() => {
                eprintln!("warning: stripping signature from tag");
                Ok(exported.to_vec())
            }
            _ => Ok(exported.to_vec()),
        }
    }

    fn rewrite_filtered_tag_commit(&self, mut commit: ObjectId) -> Result<Option<ObjectId>> {
        loop {
            if self.commit_marks.contains_key(&commit) {
                return Ok(Some(commit));
            }
            let object = self.db.read_object(&commit)?;
            let parsed = Commit::parse(self.format, &object.body)?;
            let Some(parent) = parsed.parents.first() else {
                return Ok(None);
            };
            commit = *parent;
        }
    }

    fn rewrite_commit_for_ref(&self, mut commit: ObjectId) -> Result<Option<ObjectId>> {
        loop {
            let object = self.db.read_object(&commit)?;
            let parsed = Commit::parse(self.format, &object.body)?;
            if parsed.parents.len() > 1 {
                return Ok(Some(commit));
            }
            if !self.commit_marks.contains_key(&commit) {
                return Ok(Some(commit));
            }
            let Some(parent) = parsed.parents.first() else {
                return Ok(None);
            };
            let parent_tree = self
                .db
                .read_object(parent)
                .ok()
                .and_then(|obj| Commit::parse(self.format, &obj.body).ok())
                .map(|c| c.tree);
            let Some(parent_tree) = parent_tree else {
                return Ok(Some(commit));
            };
            let same = diff_name_status_trees_with_options(
                &self.db,
                self.format,
                &parent_tree,
                &parsed.tree,
                DiffNameStatusOptions {
                    detect_renames: false,
                    detect_copies: false,
                    ..Default::default()
                },
            )
            .map(|changes| changes.is_empty())
            .unwrap_or(false);
            if !same {
                return Ok(Some(commit));
            }
            commit = *parent;
        }
    }

    fn emit_deletes(&mut self) -> Result<()> {
        emit_delete_refspecs(&mut self.out, &self.options.refspecs, self.format)
    }

    fn alloc_mark(&mut self) -> u64 {
        let mark = self.next_mark;
        self.next_mark += 1;
        mark
    }

    fn show_progress(&mut self) {
        let Some(step) = self.options.progress else {
            return;
        };
        self.progress_counter += 1;
        if self.progress_counter % step == 0 {
            let _ = writeln!(self.out, "progress {} objects", self.progress_counter);
        }
    }
}

fn parse_fast_export_args(args: &[String]) -> Result<(FastExportOptions, Vec<String>)> {
    let mut options = FastExportOptions::default();
    let mut setup_args = Vec::new();
    let mut end_of_options = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if end_of_options {
            setup_args.push(arg.clone());
            continue;
        }
        match arg.as_str() {
            "--end-of-options" => {
                end_of_options = true;
                options.end_of_options = true;
                setup_args.push(arg.clone());
            }
            "--" => {
                setup_args.push(arg.clone());
                setup_args.extend(iter.cloned());
                break;
            }
            "--progress" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--progress requires a value".into()))?;
                options.progress =
                    Some(value.parse().map_err(|_| {
                        GitError::Command(format!("invalid --progress value {value}"))
                    })?);
            }
            value if value.starts_with("--progress=") => {
                let rest = &value["--progress=".len()..];
                options.progress =
                    Some(rest.parse().map_err(|_| {
                        GitError::Command(format!("invalid --progress value {rest}"))
                    })?);
            }
            "--signed-tags" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--signed-tags requires a value".into()))?;
                options.signed_tags = parse_sign_mode(value, "signed-tags")?;
            }
            value if value.starts_with("--signed-tags=") => {
                options.signed_tags =
                    parse_sign_mode(&value["--signed-tags=".len()..], "signed-tags")?;
            }
            "--signed-commits" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--signed-commits requires a value".into()))?;
                options.signed_commits = parse_sign_mode(value, "signed-commits")?;
            }
            value if value.starts_with("--signed-commits=") => {
                options.signed_commits =
                    parse_sign_mode(&value["--signed-commits=".len()..], "signed-commits")?;
            }
            "--tag-of-filtered-object" => {
                let value = iter.next().ok_or_else(|| {
                    GitError::Command("--tag-of-filtered-object requires a value".into())
                })?;
                options.tag_of_filtered = parse_tag_of_filtered_mode(value)?;
            }
            value if value.starts_with("--tag-of-filtered-object=") => {
                options.tag_of_filtered =
                    parse_tag_of_filtered_mode(&value["--tag-of-filtered-object=".len()..])?;
            }
            "--reencode" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--reencode requires a value".into()))?;
                options.reencode = parse_reencode_mode(value)?;
            }
            value if value.starts_with("--reencode=") => {
                options.reencode = parse_reencode_mode(&value["--reencode=".len()..])?;
            }
            "--export-marks" => {
                options.export_marks =
                    Some(PathBuf::from(iter.next().ok_or_else(|| {
                        GitError::Command("--export-marks requires a value".into())
                    })?));
            }
            value if value.starts_with("--export-marks=") => {
                options.export_marks = Some(PathBuf::from(&value["--export-marks=".len()..]));
            }
            "--import-marks" => {
                options.import_marks =
                    Some(PathBuf::from(iter.next().ok_or_else(|| {
                        GitError::Command("--import-marks requires a value".into())
                    })?));
            }
            value if value.starts_with("--import-marks=") => {
                options.import_marks = Some(PathBuf::from(&value["--import-marks=".len()..]));
            }
            "--import-marks-if-exists" => {
                options.import_marks_if_exists =
                    Some(PathBuf::from(iter.next().ok_or_else(|| {
                        GitError::Command("--import-marks-if-exists requires a value".into())
                    })?));
            }
            value if value.starts_with("--import-marks-if-exists=") => {
                options.import_marks_if_exists =
                    Some(PathBuf::from(&value["--import-marks-if-exists=".len()..]));
            }
            "--fake-missing-tagger" => options.fake_missing_tagger = true,
            "--full-tree" => options.full_tree = true,
            "--use-done-feature" => options.use_done_feature = true,
            "--no-data" => options.no_data = true,
            "--reference-excluded-parents" => options.reference_excluded_parents = true,
            "--show-original-ids" => options.show_original_ids = true,
            "--mark-tags" => options.mark_tags = true,
            "--anonymize" | "--anonymize-map" => {
                return Err(GitError::Unsupported(format!("{arg} is not supported yet")));
            }
            "-M" => options.diff.detect_renames = true,
            "-C" => {
                options.diff.detect_renames = true;
                options.diff.detect_copies = true;
            }
            "--find-renames" => options.diff.detect_renames = true,
            value if value.starts_with("--find-renames=") => options.diff.detect_renames = true,
            "--find-copies-harder" => {
                options.diff.detect_renames = true;
                options.diff.detect_copies = true;
                options.diff.find_copies_harder = true;
            }
            "--refspec" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--refspec requires a value".into()))?;
                options.refspecs.push(value.clone());
            }
            value if value.starts_with("--refspec=") => {
                options
                    .refspecs
                    .push(value["--refspec=".len()..].to_string());
            }
            "--all"
            | "--branches"
            | "--tags"
            | "--remotes"
            | "--not"
            | "--first-parent"
            | "--full-history"
            | "--simplify-merges"
            | "--ancestry-path"
            | "--reverse"
            | "--topo-order"
            | "--date-order"
            | "--author-date-order"
            | "--no-walk"
            | "--do-walk"
            | "--ignore-missing" => setup_args.push(arg.clone()),
            "--default" | "-n" | "--max-count" | "--skip" => {
                setup_args.push(arg.clone());
                setup_args.push(
                    iter.next()
                        .ok_or_else(|| GitError::Command(format!("{arg} requires a value")))?
                        .clone(),
                );
            }
            value
                if value.starts_with("--max-count=")
                    || value.starts_with("--skip=")
                    || value.starts_with("--branches=")
                    || value.starts_with("--tags=")
                    || value.starts_with("--remotes=")
                    || value.starts_with("--glob=")
                    || value.starts_with("--exclude=")
                    || (value.starts_with("-n") && value.len() > 2) =>
            {
                setup_args.push(arg.clone());
            }
            other => setup_args.push(other.to_string()),
        }
    }
    Ok((options, setup_args))
}

fn parse_sign_mode(value: &str, option: &str) -> Result<SignMode> {
    match value {
        "abort" => Ok(SignMode::Abort),
        "warn" | "warn-verbatim" => Ok(SignMode::WarnVerbatim),
        "verbatim" => Ok(SignMode::Verbatim),
        "warn-strip" => Ok(SignMode::WarnStrip),
        "strip" => Ok(SignMode::Strip),
        other => {
            eprintln!("fatal: unknown {option} mode: {other}");
            Err(GitError::Exit(128))
        }
    }
}

fn parse_tag_of_filtered_mode(value: &str) -> Result<TagOfFilteredMode> {
    match value {
        "abort" => Ok(TagOfFilteredMode::Abort),
        "drop" => Ok(TagOfFilteredMode::Drop),
        "rewrite" => Ok(TagOfFilteredMode::Rewrite),
        other => {
            eprintln!("fatal: unknown tag-of-filtered mode: {other}");
            Err(GitError::Exit(128))
        }
    }
}

fn parse_reencode_mode(value: &str) -> Result<ReencodeMode> {
    match value {
        "yes" | "true" | "1" => Ok(ReencodeMode::Yes),
        "no" | "false" | "0" => Ok(ReencodeMode::No),
        "abort" => Ok(ReencodeMode::Abort),
        other => {
            eprintln!("fatal: unknown reencoding mode: {other}");
            Err(GitError::Exit(128))
        }
    }
}

fn import_marks_into(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    path: &Path,
    marks: &mut HashMap<ObjectId, u64>,
) -> Result<u64> {
    let contents = fs::read_to_string(path).map_err(|err| {
        eprintln!("fatal: unable to open marks file {path:?} for reading: {err}");
        GitError::Exit(128)
    })?;
    import_marks_from_str(db, format, &contents, marks)
}

fn import_marks_from_str(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    contents: &str,
    marks: &mut HashMap<ObjectId, u64>,
) -> Result<u64> {
    let mut last = 0u64;
    for line in contents.lines() {
        let Some((mark_text, oid_text)) =
            line.strip_prefix(':').and_then(|rest| rest.split_once(' '))
        else {
            eprintln!("fatal: corrupt mark line: {line}");
            return Err(GitError::Exit(128));
        };
        let mark: u64 = mark_text.parse().map_err(|_| {
            eprintln!("fatal: corrupt mark line: {line}");
            GitError::Exit(128)
        })?;
        if mark == 0 {
            eprintln!("fatal: corrupt mark line: {line}");
            return Err(GitError::Exit(128));
        }
        let oid = ObjectId::from_hex(format, oid_text.trim()).map_err(|_| {
            eprintln!("fatal: corrupt mark line: {line}");
            GitError::Exit(128)
        })?;
        let object = db.read_object(&oid).map_err(|_| {
            eprintln!("fatal: object not found: {oid_text}");
            GitError::Exit(128)
        })?;
        if object.object_type != ObjectType::Commit {
            continue;
        }
        marks.insert(oid, mark);
        last = last.max(mark);
    }
    Ok(last)
}

fn export_marks(path: &Path, marks: &HashMap<ObjectId, u64>, format: ObjectFormat) -> Result<()> {
    let mut entries: Vec<_> = marks.iter().map(|(oid, mark)| (*mark, *oid)).collect();
    entries.sort_by_key(|(mark, _)| *mark);
    let mut file = fs::File::create(path).map_err(|err| {
        eprintln!("fatal: unable to open marks file {path:?} for writing: {err}");
        GitError::Exit(128)
    })?;
    for (mark, oid) in entries {
        writeln!(file, ":{mark} {}", oid.to_hex())?;
    }
    Ok(())
}

fn list_all_tag_refs(store: &FileRefStore) -> Result<Vec<Ref>> {
    let mut refs = store.list_all_refs()?;
    refs.retain(|reference| reference.name.starts_with("refs/tags/"));
    Ok(refs)
}

fn validate_fast_export_tag_refs(
    db: &FileObjectDatabase,
    store: &FileRefStore,
    format: ObjectFormat,
    refs: &[Ref],
) -> Result<()> {
    let mut severity = sley_fsck::content::SeverityConfig::new(true);
    severity.set("extraHeaderEntry", "error");
    severity.set("badGpgsig", "error");

    for reference in refs {
        let RefTarget::Direct(oid) = reference.target else {
            continue;
        };
        let object = db.read_object(&oid)?;
        if object.object_type != ObjectType::Tag {
            continue;
        }
        let findings =
            sley_fsck::content::check_object_content(ObjectType::Tag, &object.body, &severity);
        if findings.is_empty() {
            let tag = Tag::parse_ref(format, &object.body)?;
            let read_oid = apply_replace_object(store, &tag.object)?;
            let target = match db.read_object(&read_oid) {
                Ok(target) => target,
                Err(_) => {
                    eprintln!("fatal: could not read tagged object '{}'", tag.object);
                    return Err(GitError::Exit(128));
                }
            };
            if target.object_type != tag.object_type {
                eprintln!(
                    "fatal: object '{}' tagged as '{}', but is a '{}' type",
                    tag.object,
                    tag.object_type.as_str(),
                    target.object_type.as_str()
                );
                return Err(GitError::Exit(128));
            }
            continue;
        }
        let mut has_error = false;
        for finding in findings {
            let prefix = match finding.severity {
                sley_fsck::content::Severity::Error => {
                    has_error = true;
                    "error in"
                }
                sley_fsck::content::Severity::Warn => "warning in",
                sley_fsck::content::Severity::Ignore => continue,
            };
            if let Some(raw) = &finding.raw_stderr {
                eprintln!("error: {raw}");
            }
            eprintln!(
                "{prefix} tag {oid}: {}: {}",
                finding.msg_id.camel(),
                finding.detail
            );
        }
        if has_error {
            return Err(GitError::Exit(128));
        }
    }
    Ok(())
}

fn has_delete_refspec(options: &FastExportOptions) -> bool {
    options.refspecs.iter().any(|refspec| {
        refspec
            .split_once(':')
            .is_some_and(|(src, dst)| src.is_empty() && !dst.is_empty())
    })
}

fn emit_delete_refspecs(
    out: &mut impl Write,
    refspecs: &[String],
    format: ObjectFormat,
) -> Result<()> {
    for refspec in refspecs {
        let Some((src, dst)) = refspec.split_once(':') else {
            continue;
        };
        if src.is_empty() && !dst.is_empty() {
            writeln!(
                out,
                "reset {}\nfrom {}\n",
                dst,
                ObjectId::null(format).to_hex()
            )?;
        }
    }
    Ok(())
}

fn write_fast_export_path(out: &mut impl Write, path: &[u8]) -> Result<()> {
    let quoted = status_quote_path(path, true);
    out.write_all(quoted.as_bytes())?;
    Ok(())
}

fn depth_first_cmp(left: &NameStatusEntry, right: &NameStatusEntry) -> std::cmp::Ordering {
    let left_path = left
        .old_path
        .as_ref()
        .map(|path| path.as_bytes())
        .unwrap_or_else(|| left.path.as_bytes());
    let right_path = right
        .old_path
        .as_ref()
        .map(|path| path.as_bytes())
        .unwrap_or_else(|| right.path.as_bytes());
    let common = left_path.len().min(right_path.len());
    let cmp = left_path[..common].cmp(&right_path[..common]);
    if cmp != std::cmp::Ordering::Equal {
        return cmp;
    }
    let len_cmp = right_path.len().cmp(&left_path.len());
    if len_cmp != std::cmp::Ordering::Equal {
        return len_cmp;
    }
    let left_is_rename = matches!(left.status, NameStatus::Renamed(_));
    let right_is_rename = matches!(right.status, NameStatus::Renamed(_));
    left_is_rename.cmp(&right_is_rename)
}

fn same_blob_and_mode(entry: &NameStatusEntry) -> bool {
    entry.old_oid.is_some()
        && entry.new_oid.is_some()
        && entry.old_oid == entry.new_oid
        && entry.old_mode == entry.new_mode
}

fn reencode_commit_message(message: &[u8], from: &str, to: &str) -> Option<Vec<u8>> {
    if encoding_is_none(to) || from.trim().eq_ignore_ascii_case(to.trim()) {
        return Some(message.to_vec());
    }
    let from_encoding = encoding_for_name(from)?;
    let to_encoding = encoding_for_name(to)?;
    if from_encoding == to_encoding {
        return Some(message.to_vec());
    }
    let (decoded, _, had_decode_errors) = from_encoding.decode(message);
    if had_decode_errors {
        return None;
    }
    let (encoded, _, had_encode_errors) = to_encoding.encode(&decoded);
    if had_encode_errors {
        return None;
    }
    Some(encoded.into_owned())
}

fn tag_signature_start(message: &[u8]) -> usize {
    const MARKERS: [&[u8]; 4] = [
        b"-----BEGIN PGP SIGNATURE-----",
        b"-----BEGIN PGP MESSAGE-----",
        b"-----BEGIN SIGNED MESSAGE-----",
        b"-----BEGIN SSH SIGNATURE-----",
    ];
    let mut start = 0usize;
    let mut sig = message.len();
    while start < message.len() {
        let line = &message[start..];
        if MARKERS.iter().any(|marker| line.starts_with(marker)) {
            sig = start;
        }
        match line.iter().position(|byte| *byte == b'\n') {
            Some(offset) => start += offset + 1,
            None => break,
        }
    }
    sig
}
