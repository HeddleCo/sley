//! Minimum-viable `fast-import` (sley#21).
//!
//! Scope: exactly the stream the upstream `test_commit_bulk` helper emits
//! (`t/test-lib-functions.sh`):
//!
//! ```text
//! commit <ref>
//! author <ident>
//! committer <ident>
//! data <<EOF
//! <message>
//! EOF
//! [from <ref>^0]
//! M 644 inline <path>
//! data <<EOF
//! <content>
//! EOF
//! <blank line>
//! ```
//!
//! Plus the surrounding `git -c fastimport.unpacklimit=<n> fast-import`
//! invocation: Sley stages stream objects, then writes them loose or preserves
//! one import pack according to the configured unpack limit.
//!
//! Supported subset of the grammar: `commit`, `author`, `committer`, `data`
//! (both delimited `data <<DELIM` and counted `data <n>` forms), `from`, `mark`,
//! `M <mode> inline <path>`, and `M <mode> <oid> <path>` (referencing an existing
//! blob or a prior `blob`/`mark`). `blob` with a leading `mark`, `reset`, and
//! `done` are accepted. Anything else is a hard `unsupported command` error so a
//! caller never silently gets a wrong result — full grammar is out of scope.
//!
//! Native Rust only: objects are persisted through the ODB and refs are
//! published transactionally at checkpoint/end-of-stream; no shell-out.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use crate::*;
use sley::plumbing::{sley_config, sley_odb, sley_pack, sley_refs, sley_rev};
use std::cell::RefCell;
use std::io::{BufRead, Write};
use std::sync::Arc;

#[derive(Default)]
struct FastImportOptions {
    import_marks: Vec<MarksImport>,
    export_marks: Option<PathBuf>,
    export_pack_edges: Option<PathBuf>,
    date_format: FastImportDateFormat,
    force: bool,
    rewrite_specs: Vec<SubmoduleRewriteOption>,
    submodule_rewrites: HashMap<ObjectId, ObjectId>,
    allow_unsafe_features: bool,
    feature_import_seen: bool,
    relative_marks: bool,
    cat_blob_fd: Option<i32>,
}

struct MarksImport {
    path: PathBuf,
    missing_ok: bool,
}

struct SubmoduleRewriteOption {
    name: String,
    path: PathBuf,
    direction: SubmoduleRewriteDirection,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SubmoduleRewriteDirection {
    From,
    To,
}

#[derive(Clone, Copy)]
enum FastImportRefState {
    Empty,
    Tip(ObjectId),
}

#[derive(Clone, Copy, Default)]
enum FastImportDateFormat {
    #[default]
    Raw,
    RawPermissive,
    Rfc2822,
    Now,
}

/// A parent/tree resolution: either an existing commit (from `from`) or none.
struct CommitBuild {
    parents: Vec<ObjectId>,
    /// Tree entries keyed by full path, seeded from the parent commit's tree and
    /// mutated by `M`/`D` filemodify lines.
    tree: BTreeMap<Vec<u8>, TreeEntry>,
    /// The inherited tree can be referenced directly when the commit made no
    /// file changes. `None` means the map must be materialized (a new root or
    /// any mutating file command).
    unchanged_tree_oid: Option<ObjectId>,
    author: Vec<u8>,
    committer: Vec<u8>,
    encoding: Option<Vec<u8>>,
    message: Vec<u8>,
}

struct FastImportCommitTree {
    oid: ObjectId,
    entries: Arc<BTreeMap<Vec<u8>, TreeEntry>>,
}

struct FastImportPackState {
    force_pack: bool,
    unpack_limit: Option<usize>,
    pending: Vec<ObjectId>,
    pending_set: HashSet<ObjectId>,
    pending_loose: HashSet<ObjectId>,
    pending_edges: BTreeMap<String, ObjectId>,
    export_edges: Option<PathBuf>,
}

impl FastImportPackState {
    fn new(force_pack: bool, unpack_limit: Option<usize>, export_edges: Option<PathBuf>) -> Self {
        Self {
            force_pack,
            unpack_limit,
            pending: Vec::new(),
            pending_set: HashSet::new(),
            pending_loose: HashSet::new(),
            pending_edges: BTreeMap::new(),
            export_edges,
        }
    }

    fn record_object(&mut self, oid: ObjectId, wrote_loose: bool) {
        if !self.tracks_pending_objects() {
            return;
        }
        if self.pending_set.insert(oid) {
            self.pending.push(oid);
        }
        if wrote_loose {
            self.pending_loose.insert(oid);
        }
    }

    fn record_edge(&mut self, ref_name: &str, oid: ObjectId) {
        if self.export_edges.is_some() {
            self.pending_edges.insert(ref_name.to_string(), oid);
        }
    }

    fn tracks_pending_objects(&self) -> bool {
        self.force_pack || self.unpack_limit.is_some() || self.export_edges.is_some()
    }

    fn should_pack_pending(&self) -> bool {
        if self.force_pack || self.export_edges.is_some() {
            return true;
        }
        self.unpack_limit
            .is_some_and(|limit| self.pending.len() > limit)
    }

    fn clear_pending(&mut self) {
        self.pending.clear();
        self.pending_set.clear();
        self.pending_loose.clear();
        self.pending_edges.clear();
    }
}

/// Read-through object overlay for one import stream.
///
/// Git's fast-import writes into an open pack and only decides at the final
/// checkpoint whether a small pack should be unpacked. Writing every object
/// loose first is observably equivalent, but turns a large import into one
/// temp-file/rename pair per object. Keep new objects in memory so commands in
/// the same stream can resolve marks and parents, then persist them once at a
/// checkpoint as either one pack or (for a small import) loose objects.
struct FastImportDatabase {
    persistent: FileObjectDatabase,
    persistent_presence: RefCell<sley_odb::ObjectPresenceChecker>,
    staged: HashMap<ObjectId, Arc<EncodedObject>>,
    staged_body_bytes: usize,
    recent_commit_trees: HashMap<ObjectId, FastImportCommitTree>,
    recent_commit_tree_order: VecDeque<ObjectId>,
}

impl FastImportDatabase {
    // Bound the closed-world overlay independently of stream length. Pack
    // writing adds transient buffers, so keep the retained body budget modest;
    // one individual object may still exceed it because parsing that object is
    // inherently atomic.
    const MAX_STAGED_BODY_BYTES: usize = 64 * 1024 * 1024;
    const MAX_STAGED_OBJECTS: usize = 100_000;
    const RECENT_COMMIT_TREE_LIMIT: usize = 8;
    const MAX_CACHED_COMMIT_TREE_ENTRIES: usize = 4096;
    const MAX_CACHED_COMMIT_TREE_BYTES: usize = 1024 * 1024;

    fn new(persistent: FileObjectDatabase) -> Self {
        let persistent_presence = RefCell::new(persistent.presence_checker());
        Self {
            persistent,
            persistent_presence,
            staged: HashMap::new(),
            staged_body_bytes: 0,
            recent_commit_trees: HashMap::new(),
            recent_commit_tree_order: VecDeque::new(),
        }
    }

    fn object_format(&self) -> ObjectFormat {
        self.persistent.object_format()
    }

    fn contains(&self, oid: &ObjectId) -> Result<bool> {
        Ok(self.staged.contains_key(oid)
            || self
                .persistent_presence
                .borrow_mut()
                .contains_without_refresh(oid)?)
    }

    fn stage(&mut self, oid: ObjectId, object: EncodedObject) {
        self.staged_body_bytes = self.staged_body_bytes.saturating_add(object.body.len());
        self.staged.insert(oid, Arc::new(object));
    }

    fn should_flush_staged(&self) -> bool {
        Self::staging_budget_exceeded(self.staged.len(), self.staged_body_bytes)
    }

    fn staging_budget_exceeded(object_count: usize, body_bytes: usize) -> bool {
        object_count >= Self::MAX_STAGED_OBJECTS || body_bytes >= Self::MAX_STAGED_BODY_BYTES
    }

    fn take_staged(&mut self, oid: &ObjectId) -> Option<Arc<EncodedObject>> {
        let object = self.staged.remove(oid)?;
        self.staged_body_bytes = self.staged_body_bytes.saturating_sub(object.body.len());
        Some(object)
    }

    fn persist_staged_loose(&mut self, oids: &[ObjectId]) -> Result<()> {
        for oid in oids {
            if let Some(object) = self.take_staged(oid) {
                self.persistent.write_object(object.as_ref().clone())?;
            }
        }
        Ok(())
    }

    fn discard_staged(&mut self, oids: &[ObjectId]) {
        for oid in oids {
            self.take_staged(oid);
        }
    }

    fn refresh_persistent_presence(&mut self) {
        self.persistent_presence = RefCell::new(self.persistent.presence_checker());
    }

    fn remember_commit_tree(
        &mut self,
        commit_oid: ObjectId,
        tree_oid: ObjectId,
        entries: Arc<BTreeMap<Vec<u8>, TreeEntry>>,
    ) {
        let estimated_bytes = entries.iter().fold(0usize, |total, (path, entry)| {
            total
                .saturating_add(path.len())
                .saturating_add(entry.name.as_bytes().len())
                .saturating_add(mem::size_of::<TreeEntry>())
        });
        if entries.len() > Self::MAX_CACHED_COMMIT_TREE_ENTRIES
            || estimated_bytes > Self::MAX_CACHED_COMMIT_TREE_BYTES
        {
            return;
        }
        if self.recent_commit_trees.contains_key(&commit_oid) {
            return;
        }
        while self.recent_commit_tree_order.len() >= Self::RECENT_COMMIT_TREE_LIMIT {
            if let Some(expired) = self.recent_commit_tree_order.pop_front() {
                self.recent_commit_trees.remove(&expired);
            }
        }
        self.recent_commit_tree_order.push_back(commit_oid);
        self.recent_commit_trees.insert(
            commit_oid,
            FastImportCommitTree {
                oid: tree_oid,
                entries,
            },
        );
    }

    fn recent_commit_tree(&self, commit_oid: &ObjectId) -> Option<&FastImportCommitTree> {
        self.recent_commit_trees.get(commit_oid)
    }
}

impl sley_odb::ObjectReader for FastImportDatabase {
    fn read_object(&self, oid: &ObjectId) -> Result<Arc<EncodedObject>> {
        self.staged
            .get(oid)
            .cloned()
            .map_or_else(|| self.persistent.read_object(oid), Ok)
    }

    fn is_shallow_graft(&self, oid: &ObjectId) -> bool {
        self.persistent.is_shallow_graft(oid)
    }

    fn has_shallow_grafts(&self) -> bool {
        self.persistent.has_shallow_grafts()
    }

    fn is_promised_object(&self, oid: &ObjectId) -> bool {
        self.persistent.is_promised_object(oid)
    }
}

pub(crate) fn cmd_fast_import(args: &[String]) -> Result<()> {
    // Accept and ignore the options test_commit_bulk pairs us with; reject
    // anything we don't model so a caller never gets a silently-wrong import.
    let mut require_done = false;
    let mut options = FastImportOptions::default();
    for arg in args {
        match arg.as_str() {
            // No-op flags that don't change the minimal-subset behavior.
            "--quiet" => {}
            "--force" => {
                options.force = true;
            }
            "--done" => require_done = true,
            "--allow-unsafe-features" => {
                options.allow_unsafe_features = true;
            }
            value if value.starts_with("--date-format=") => {
                options.date_format =
                    parse_fast_import_date_format(option_value(value, "--date-format="))?;
            }
            value if value.starts_with("--max-pack-size=") => {}
            value if value.starts_with("--big-file-threshold=") => {
                validate_fast_import_non_negative_option(
                    "--big-file-threshold",
                    option_value(value, "--big-file-threshold="),
                )?
            }
            value if value.starts_with("--depth=") => {
                validate_fast_import_non_negative_option("--depth", &value["--depth=".len()..])?
            }
            value if value.starts_with("--cat-blob-fd=") => {
                options.cat_blob_fd = Some(validate_fast_import_fd(
                    "--cat-blob-fd",
                    option_value(value, "--cat-blob-fd="),
                )?);
            }
            value if value.starts_with("--export-marks=") => {
                options.export_marks = Some(PathBuf::from(option_value(value, "--export-marks=")));
            }
            value if value.starts_with("--export-pack-edges=") => {
                options.export_pack_edges =
                    Some(PathBuf::from(option_value(value, "--export-pack-edges=")));
            }
            value if value.starts_with("--import-marks=") => {
                options.import_marks.push(MarksImport {
                    path: PathBuf::from(option_value(value, "--import-marks=")),
                    missing_ok: false,
                });
            }
            value if value.starts_with("--import-marks-if-exists=") => {
                options.import_marks.push(MarksImport {
                    path: PathBuf::from(option_value(value, "--import-marks-if-exists=")),
                    missing_ok: true,
                });
            }
            value if value.starts_with("--rewrite-submodules-from=") => {
                options.rewrite_specs.push(parse_submodule_rewrite_option(
                    option_value(value, "--rewrite-submodules-from="),
                    SubmoduleRewriteDirection::From,
                )?);
            }
            value if value.starts_with("--rewrite-submodules-to=") => {
                options.rewrite_specs.push(parse_submodule_rewrite_option(
                    option_value(value, "--rewrite-submodules-to="),
                    SubmoduleRewriteDirection::To,
                )?);
            }
            value => {
                return Err(GitError::Command(format!(
                    "unsupported fast-import option {value}"
                )));
            }
        }
    }

    let git_dir = crate::session::cli_git_dir()?;
    let format = repository_object_format(&git_dir)?;
    let mut db = FastImportDatabase::new(FileObjectDatabase::from_git_dir(&git_dir, format));
    let store = FileRefStore::new(&git_dir, format);
    let pack_policy = fast_import_pack_policy(&git_dir, options.export_pack_edges.is_some())?;
    let mut pack_state = FastImportPackState::new(
        pack_policy.force_pack,
        pack_policy.unpack_limit,
        options.export_pack_edges.clone(),
    );

    let stdin = io::stdin();
    let mut parser = StreamParser::new(io::BufReader::new(stdin.lock()));
    let mut stdout = io::stdout();
    let mut cat_blob_out = fast_import_output_for_cat_blob(options.cat_blob_fd)?;
    // Marks introduced by `mark :N` on blobs/commits, mapping to the written oid.
    let mut marks: HashMap<u64, ObjectId> = HashMap::new();
    load_mark_imports(&mut marks, format, &options.import_marks)?;
    load_submodule_rewrites(format, &mut options)?;
    let mut ref_states: HashMap<String, FastImportRefState> = HashMap::new();
    let mut dirty_refs: HashSet<String> = HashSet::new();
    let mut saw_done = false;
    let mut features_allowed = true;
    let mut failed_ref_update = false;

    while let Some(line) = parser.next_command_line()? {
        if line.is_empty() {
            continue;
        }
        if line.starts_with(b"#") {
            continue;
        }
        if let Some(rest) = line_after(&line, b"feature ") {
            if !features_allowed {
                return Err(GitError::Command(
                    "fast-import: feature command after data command".into(),
                ));
            }
            handle_feature(
                rest,
                &git_dir,
                format,
                &mut marks,
                &mut options,
                &mut require_done,
            )?;
        } else if let Some(rest) = line_after(&line, b"commit ") {
            features_allowed = false;
            let ref_name = resolve_commit_ref(&store, rest)?;
            handle_commit(
                &mut parser,
                &mut db,
                &store,
                format,
                &options,
                &mut marks,
                &mut pack_state,
                &mut ref_states,
                &mut dirty_refs,
                ref_name,
                &mut cat_blob_out,
            )?;
        } else if let Some(rest) = line_after(&line, b"blob") {
            features_allowed = false;
            handle_blob(&mut parser, &mut db, &mut pack_state, &mut marks, rest)?;
        } else if let Some(rest) = line_after(&line, b"tag ") {
            features_allowed = false;
            handle_tag(
                &mut parser,
                &mut db,
                &store,
                format,
                options.date_format,
                &mut marks,
                &mut pack_state,
                &mut ref_states,
                &mut dirty_refs,
                rest,
            )?;
        } else if let Some(rest) = line_after(&line, b"alias") {
            features_allowed = false;
            handle_alias(
                &mut parser,
                &db,
                &store,
                format,
                &mut marks,
                &ref_states,
                rest,
            )?;
        } else if let Some(rest) = line_after(&line, b"reset ") {
            features_allowed = false;
            handle_reset(
                &mut parser,
                &db,
                &store,
                format,
                &marks,
                &mut ref_states,
                &mut dirty_refs,
                rest,
            )?;
        } else if let Some(rest) = line_after(&line, b"cat-blob ") {
            features_allowed = false;
            handle_cat_blob_line(
                &mut db,
                &store,
                format,
                &marks,
                &ref_states,
                rest,
                &mut cat_blob_out,
            )?;
        } else if let Some(rest) = line_after(&line, b"get-mark ") {
            features_allowed = false;
            handle_get_mark_line(format, &marks, rest, &mut cat_blob_out)?;
        } else if let Some(rest) = line_after(&line, b"ls ") {
            features_allowed = false;
            handle_ls_line(
                &mut db,
                &store,
                format,
                &marks,
                &ref_states,
                None,
                None,
                &mut pack_state,
                rest,
                &mut cat_blob_out,
            )?;
        } else if line.as_slice() == b"done" || line.as_slice() == b"checkpoint" {
            // `checkpoint` asks fast-import to flush durable state, including a
            // currently-open pack when pack mode is active.
            flush_fast_import_pack(&git_dir, &mut db, format, &mut pack_state)?;
            failed_ref_update |= flush_fast_import_refs(
                &store,
                &git_dir,
                &db,
                format,
                options.force,
                &ref_states,
                &mut dirty_refs,
            )?;
            if line.as_slice() == b"done" {
                saw_done = true;
                break;
            }
            export_marks_if_requested(&marks, &options)?;
        } else if let Some(rest) = line_after(&line, b"option ") {
            features_allowed = false;
            handle_option(rest)?;
        } else if line_after(&line, b"progress ").is_some() {
            write_progress(&mut stdout, &line)?;
        } else if line.as_slice() == b"progress" {
            write_progress(&mut stdout, &line)?;
        } else {
            return Err(GitError::Command(format!(
                "unsupported command {}",
                String::from_utf8_lossy(&line).trim_end()
            )));
        }
        if db.should_flush_staged() {
            // Object durability is independent of the ref checkpoint: publish
            // no branch here, but close the current object batch before an
            // unbounded stream can grow the in-memory overlay without limit.
            flush_fast_import_pack(&git_dir, &mut db, format, &mut pack_state)?;
        }
    }

    if require_done && !saw_done {
        return Err(GitError::Command(
            "fast-import: stream ended without done".into(),
        ));
    }
    flush_fast_import_pack(&git_dir, &mut db, format, &mut pack_state)?;
    failed_ref_update |= flush_fast_import_refs(
        &store,
        &git_dir,
        &db,
        format,
        options.force,
        &ref_states,
        &mut dirty_refs,
    )?;
    export_marks_if_requested(&marks, &options)?;
    if failed_ref_update {
        return Err(GitError::Command(
            "fast-import: one or more branch updates were rejected".into(),
        ));
    }
    Ok(())
}

fn validate_fast_import_non_negative_option(name: &str, value: &str) -> Result<()> {
    if value.is_empty() || value.parse::<u64>().is_err() {
        return Err(GitError::Command(format!(
            "invalid value for {name}: {value}"
        )));
    }
    Ok(())
}

fn validate_fast_import_fd(name: &str, value: &str) -> Result<i32> {
    let fd = value
        .parse::<i32>()
        .map_err(|_| GitError::Command(format!("invalid value for {name}: {value}")))?;
    if fd < 0 {
        return Err(GitError::Command(format!(
            "invalid value for {name}: {value}"
        )));
    }
    Ok(fd)
}

fn fast_import_output_for_cat_blob(fd: Option<i32>) -> Result<Box<dyn Write>> {
    let Some(fd) = fd else {
        return Ok(Box::new(io::stdout()));
    };
    #[cfg(unix)]
    {
        return Ok(Box::new(sley_procinfo::duplicate_fd(fd)?));
    }
    #[cfg(not(unix))]
    {
        let path = PathBuf::from(format!("/dev/fd/{fd}"));
        Ok(Box::new(fs::OpenOptions::new().append(true).open(path)?))
    }
}

fn option_value<'a>(arg: &'a str, prefix: &str) -> &'a str {
    &arg[prefix.len()..]
}

fn parse_fast_import_date_format(value: &str) -> Result<FastImportDateFormat> {
    match value {
        "raw" => Ok(FastImportDateFormat::Raw),
        "raw-permissive" => Ok(FastImportDateFormat::RawPermissive),
        "rfc2822" => Ok(FastImportDateFormat::Rfc2822),
        "now" => Ok(FastImportDateFormat::Now),
        _ => Err(GitError::Command(format!(
            "unknown --date-format argument {value}"
        ))),
    }
}

fn parse_submodule_rewrite_option(
    value: &str,
    direction: SubmoduleRewriteDirection,
) -> Result<SubmoduleRewriteOption> {
    let (name, path) = value.split_once(':').ok_or_else(|| {
        GitError::Command(format!(
            "fast-import: invalid rewrite-submodules option {value}"
        ))
    })?;
    if name.is_empty() || path.is_empty() {
        return Err(GitError::Command(format!(
            "fast-import: invalid rewrite-submodules option {value}"
        )));
    }
    Ok(SubmoduleRewriteOption {
        name: name.to_string(),
        path: PathBuf::from(path),
        direction,
    })
}

fn handle_feature(
    rest: &[u8],
    git_dir: &Path,
    format: ObjectFormat,
    marks: &mut HashMap<u64, ObjectId>,
    options: &mut FastImportOptions,
    require_done: &mut bool,
) -> Result<()> {
    let feature = trim_ascii(rest);
    match feature {
        value if value.starts_with(b"date-format=") => {
            let value = std::str::from_utf8(&value[b"date-format=".len()..]).map_err(|_| {
                GitError::InvalidFormat("fast-import: date format is not utf8".into())
            })?;
            options.date_format = parse_fast_import_date_format(value)?;
            Ok(())
        }
        b"done" => {
            *require_done = true;
            Ok(())
        }
        b"ls" | b"cat-blob" => Ok(()),
        b"relative-marks" => {
            options.relative_marks = true;
            Ok(())
        }
        b"no-relative-marks" => {
            options.relative_marks = false;
            Ok(())
        }
        value if value.starts_with(b"import-marks=") => {
            ensure_unsafe_marks_feature(options, feature)?;
            if options.import_marks.is_empty() {
                if options.feature_import_seen {
                    return Err(GitError::Command(
                        "fast-import: only one import-marks feature allowed".into(),
                    ));
                }
                let path = marks_feature_path(git_dir, options.relative_marks, &value[13..])?;
                load_mark_file(marks, format, &path, false)?;
                options.feature_import_seen = true;
            }
            Ok(())
        }
        value if value.starts_with(b"import-marks-if-exists=") => {
            ensure_unsafe_marks_feature(options, feature)?;
            if options.import_marks.is_empty() {
                if options.feature_import_seen {
                    return Err(GitError::Command(
                        "fast-import: only one import-marks feature allowed".into(),
                    ));
                }
                let path = marks_feature_path(git_dir, options.relative_marks, &value[23..])?;
                load_mark_file(marks, format, &path, true)?;
                options.feature_import_seen = true;
            }
            Ok(())
        }
        value if value.starts_with(b"export-marks=") => {
            ensure_unsafe_marks_feature(options, feature)?;
            if options.export_marks.is_none() {
                options.export_marks = Some(marks_feature_path(
                    git_dir,
                    options.relative_marks,
                    &value[13..],
                )?);
            }
            Ok(())
        }
        _ => Err(GitError::Command(format!(
            "unsupported fast-import feature {}",
            String::from_utf8_lossy(feature)
        ))),
    }
}

fn handle_option(rest: &[u8]) -> Result<()> {
    let rest = trim_ascii(rest);
    if let Some(option) = rest.strip_prefix(b"git ") {
        match trim_ascii(option) {
            b"quiet" => Ok(()),
            _ => Err(GitError::Command(format!(
                "unsupported fast-import option {}",
                String::from_utf8_lossy(rest)
            ))),
        }
    } else {
        Ok(())
    }
}

fn ensure_unsafe_marks_feature(options: &FastImportOptions, feature: &[u8]) -> Result<()> {
    if options.allow_unsafe_features {
        Ok(())
    } else {
        Err(GitError::Command(format!(
            "unsafe fast-import feature {} requires --allow-unsafe-features",
            String::from_utf8_lossy(feature)
        )))
    }
}

fn marks_feature_path(git_dir: &Path, relative: bool, value: &[u8]) -> Result<PathBuf> {
    let text = std::str::from_utf8(trim_ascii(value))
        .map_err(|_| GitError::InvalidFormat("fast-import: marks path is not utf8".into()))?;
    if relative {
        Ok(git_dir.join("info").join("fast-import").join(text))
    } else {
        Ok(PathBuf::from(text))
    }
}

fn load_mark_imports(
    marks: &mut HashMap<u64, ObjectId>,
    format: ObjectFormat,
    imports: &[MarksImport],
) -> Result<()> {
    for import in imports {
        load_mark_file(marks, format, &import.path, import.missing_ok)?;
    }
    Ok(())
}

fn load_mark_file(
    marks: &mut HashMap<u64, ObjectId>,
    format: ObjectFormat,
    path: &Path,
    missing_ok: bool,
) -> Result<()> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if missing_ok && err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    for (idx, line) in bytes.split(|b| *b == b'\n').enumerate() {
        let line = trim_ascii(line);
        if line.is_empty() {
            continue;
        }
        let (mark, oid) = split_field(line);
        let Some(mark) = mark.strip_prefix(b":") else {
            return Err(GitError::Command(format!(
                "fast-import: malformed marks line {}",
                idx + 1
            )));
        };
        let mark_text = std::str::from_utf8(mark)
            .map_err(|_| GitError::InvalidFormat("fast-import: mark not utf8".into()))?;
        let n: u64 = mark_text
            .parse()
            .map_err(|_| GitError::Command(format!("fast-import: bad mark ':{mark_text}'")))?;
        let oid_text = std::str::from_utf8(trim_ascii(oid)).map_err(|_| {
            GitError::InvalidFormat("fast-import: mark object id is not utf8".into())
        })?;
        let oid = ObjectId::from_hex(format, oid_text)?;
        if oid == zero_oid(format)? {
            return Err(GitError::Command(format!(
                "fast-import: corrupt mark line {}",
                idx + 1
            )));
        }
        marks.insert(n, oid);
    }
    Ok(())
}

fn load_submodule_rewrites(format: ObjectFormat, options: &mut FastImportOptions) -> Result<()> {
    if options.rewrite_specs.is_empty() {
        return Ok(());
    }
    struct Pair {
        from: Option<PathBuf>,
        to: Option<PathBuf>,
    }
    let mut pairs: HashMap<String, Pair> = HashMap::new();
    for spec in &options.rewrite_specs {
        let pair = pairs.entry(spec.name.clone()).or_insert(Pair {
            from: None,
            to: None,
        });
        match spec.direction {
            SubmoduleRewriteDirection::From => pair.from = Some(spec.path.clone()),
            SubmoduleRewriteDirection::To => pair.to = Some(spec.path.clone()),
        }
    }
    for (name, pair) in pairs {
        let from = pair.from.ok_or_else(|| {
            GitError::Command(format!(
                "fast-import: missing rewrite-submodules-from for {name}"
            ))
        })?;
        let to = pair.to.ok_or_else(|| {
            GitError::Command(format!(
                "fast-import: missing rewrite-submodules-to for {name}"
            ))
        })?;
        let from_marks = read_mark_file_map(format, &from)?;
        let to_marks = read_mark_file_map(format, &to)?;
        for (mark, old_oid) in from_marks {
            if let Some(new_oid) = to_marks.get(&mark) {
                options.submodule_rewrites.insert(old_oid, *new_oid);
            }
        }
    }
    Ok(())
}

fn read_mark_file_map(format: ObjectFormat, path: &Path) -> Result<HashMap<u64, ObjectId>> {
    let bytes = fs::read(path)?;
    let mut out = HashMap::new();
    for (idx, line) in bytes.split(|b| *b == b'\n').enumerate() {
        let line = trim_ascii(line);
        if line.is_empty() {
            continue;
        }
        let (mark, oid) = split_field(line);
        let Some(mark) = mark.strip_prefix(b":") else {
            return Err(GitError::Command(format!(
                "fast-import: malformed marks line {}",
                idx + 1
            )));
        };
        let mark_text = std::str::from_utf8(mark)
            .map_err(|_| GitError::InvalidFormat("fast-import: mark not utf8".into()))?;
        let n: u64 = mark_text
            .parse()
            .map_err(|_| GitError::Command(format!("fast-import: bad mark ':{mark_text}'")))?;
        let oid_text = std::str::from_utf8(trim_ascii(oid)).map_err(|_| {
            GitError::InvalidFormat("fast-import: mark object id is not utf8".into())
        })?;
        out.insert(n, ObjectId::from_hex(format, oid_text)?);
    }
    Ok(out)
}

fn export_marks_if_requested(
    marks: &HashMap<u64, ObjectId>,
    options: &FastImportOptions,
) -> Result<()> {
    let Some(path) = &options.export_marks else {
        return Ok(());
    };
    export_marks(marks, path)
}

fn export_marks(marks: &HashMap<u64, ObjectId>, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    let mut keys = marks.keys().copied().collect::<Vec<_>>();
    keys.sort_unstable();
    let mut out = Vec::new();
    for mark in keys {
        let oid = marks
            .get(&mark)
            .expect("mark key was collected from the marks map");
        out.extend_from_slice(format!(":{mark} {oid}\n").as_bytes());
    }
    fs::write(path, out)?;
    Ok(())
}

#[derive(Clone, Copy)]
struct FastImportPackPolicy {
    force_pack: bool,
    unpack_limit: Option<usize>,
}

fn fast_import_pack_policy(
    git_dir: &Path,
    export_pack_edges: bool,
) -> Result<FastImportPackPolicy> {
    if export_pack_edges {
        return Ok(FastImportPackPolicy {
            force_pack: true,
            unpack_limit: None,
        });
    }
    if let Some(value) = global_config_value("fastimport.unpackLimit")?
        && let Some(policy) = fast_import_unpack_limit_policy(&value)
    {
        return Ok(policy);
    }
    let common_git_dir = common_git_dir_for_git_dir(git_dir)?;
    let Ok(config) = GitConfig::read(common_git_dir.join("config")) else {
        return Ok(FastImportPackPolicy {
            force_pack: false,
            unpack_limit: Some(100),
        });
    };
    Ok(config
        .get("fastimport", None, "unpackLimit")
        .and_then(fast_import_unpack_limit_policy)
        .unwrap_or(FastImportPackPolicy {
            force_pack: false,
            // builtin/fast-import.c defaults `unpack_limit` to 100. The
            // absence of config must therefore still use pack-backed import
            // for a large stream.
            unpack_limit: Some(100),
        }))
}

fn fast_import_unpack_limit_policy(value: &str) -> Option<FastImportPackPolicy> {
    let limit = value.trim().parse::<i64>().ok()?;
    if limit <= 0 {
        Some(FastImportPackPolicy {
            force_pack: true,
            unpack_limit: None,
        })
    } else {
        let unpack_limit = usize::try_from(limit).ok()?;
        Some(FastImportPackPolicy {
            force_pack: false,
            unpack_limit: Some(unpack_limit),
        })
    }
}

fn write_fast_import_object(
    db: &mut FastImportDatabase,
    pack_state: &mut FastImportPackState,
    object: EncodedObject,
) -> Result<ObjectId> {
    let oid = object.object_id(db.object_format())?;
    let existed = db.contains(&oid)?;
    if existed {
        // fast-import's active-pack object count includes an object emitted by
        // the stream even when an equivalent loose object already exists. That
        // count controls unpackLimit, and the existing object may consequently
        // be copied into the preserved pack.
        pack_state.record_object(oid, false);
        return Ok(oid);
    }
    if pack_state.tracks_pending_objects() {
        db.stage(oid, object);
        pack_state.record_object(oid, false);
    } else {
        let written = db.persistent.write_object(object)?;
        pack_state.record_object(written, true);
    }
    Ok(oid)
}

fn flush_fast_import_pack(
    git_dir: &Path,
    db: &mut FastImportDatabase,
    format: ObjectFormat,
    pack_state: &mut FastImportPackState,
) -> Result<()> {
    if pack_state.pending.is_empty() {
        pack_state.clear_pending();
        return Ok(());
    }

    if !pack_state.should_pack_pending() {
        db.persist_staged_loose(&pack_state.pending)?;
        pack_state.clear_pending();
        return Ok(());
    }

    let pending = mem::take(&mut pack_state.pending);
    pack_state.pending_set.clear();
    let pending_loose = mem::take(&mut pack_state.pending_loose);
    let edges = mem::take(&mut pack_state.pending_edges);
    let mut objects = Vec::with_capacity(pending.len());
    for oid in pending {
        objects.push((oid, db.read_object(&oid)?));
    }
    let inputs = objects
        .iter()
        .map(|(oid, object)| sley_pack::PackInput {
            oid,
            object: object.as_ref(),
        })
        .collect::<Vec<_>>();
    // Git fast-import writes its active pack in input order without a later
    // repack-style delta search (`verify-pack` reports the imported commits as
    // non-delta). Running the general packer's sliding window here costs more
    // CPU and produces a different storage profile for no command-level gain.
    let pack_options = sley_pack::PackWriteOptions::new()
        .with_depth(0)
        .with_reorder(false);
    let written =
        PackFile::write_packed_with_known_ids_and_options(&inputs, format, &pack_options)?;
    let install = db.persistent.install_written_pack(&written)?;
    prune_fast_import_packed_loose(&db.persistent, &pending_loose, &install.object_ids)?;
    db.discard_staged(&install.object_ids);
    db.persistent.refresh_read_cache();
    db.refresh_persistent_presence();

    if let Some(path) = &pack_state.export_edges
        && !edges.is_empty()
    {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        let edge_pack_path = fast_import_edge_pack_path(git_dir, &install.pack_path);
        for (_, edge) in edges {
            writeln!(file, "{edge_pack_path}: {edge}")?;
        }
    }
    Ok(())
}

fn prune_fast_import_packed_loose(
    db: &FileObjectDatabase,
    pending_loose: &HashSet<ObjectId>,
    packed_oids: &[ObjectId],
) -> Result<()> {
    let packed: HashSet<ObjectId> = packed_oids.iter().copied().collect();
    for oid in pending_loose {
        if !packed.contains(oid) {
            continue;
        }
        match fs::remove_file(db.loose().object_path(oid)?) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
    }
    Ok(())
}

fn fast_import_edge_pack_path(git_dir: &Path, pack_path: &Path) -> String {
    let Some(file_name) = pack_path.file_name() else {
        return pack_path.display().to_string();
    };
    let suffix = Path::new("objects").join("pack").join(file_name);
    if git_dir.is_relative() {
        return git_dir.join(suffix).display().to_string();
    }
    if let Ok(cwd) = env::current_dir()
        && let Ok(relative_git_dir) = git_dir.strip_prefix(&cwd)
    {
        return relative_git_dir.join(suffix).display().to_string();
    }
    pack_path.display().to_string()
}

fn write_progress(out: &mut impl Write, line: &[u8]) -> io::Result<()> {
    out.write_all(line)?;
    out.write_all(b"\n")?;
    out.flush()
}

/// Apply git fast-import's implicit-parent rule: a `commit` with no `from`
/// inherits the branch's *current* in-stream tip (including a tip created by an
/// earlier commit that has not reached a checkpoint yet) as both its parent and
/// its starting tree. Idempotent: once the base is fixed, this is a no-op.
fn default_base_from_branch(
    db: &FastImportDatabase,
    store: &FileRefStore,
    format: ObjectFormat,
    ref_states: &HashMap<String, FastImportRefState>,
    ref_name: &str,
    base_fixed: &mut bool,
    parents: &mut Vec<ObjectId>,
    tree: &mut BTreeMap<Vec<u8>, TreeEntry>,
    unchanged_tree_oid: &mut Option<ObjectId>,
) -> Result<()> {
    if *base_fixed {
        return Ok(());
    }
    *base_fixed = true;
    if let Some(state) = ref_states.get(ref_name) {
        match state {
            FastImportRefState::Empty => {}
            FastImportRefState::Tip(tip) => {
                *unchanged_tree_oid = Some(seed_tree_from_commit(db, format, tip, tree)?);
                parents.clear();
                parents.push(*tip);
            }
        }
        return Ok(());
    }
    if let Some(tip) = resolve_ref_peeled(store, ref_name)? {
        *unchanged_tree_oid = Some(seed_tree_from_commit(db, format, &tip, tree)?);
        parents.clear();
        parents.push(tip);
    }
    Ok(())
}

/// Resolve the `commit <ref>` operand to the full ref name to write. `HEAD`
/// follows the symbolic ref to the underlying branch (git updates the branch,
/// not HEAD itself); a fully-qualified `refs/...` name is used verbatim.
fn resolve_commit_ref(store: &FileRefStore, operand: &[u8]) -> Result<String> {
    let name = std::str::from_utf8(operand)
        .map_err(|_| GitError::InvalidFormat("fast-import: ref name is not utf8".into()))?
        .trim()
        .to_string();
    if name == "HEAD" {
        if let Some(branch) = store.current_branch_ref()? {
            return Ok(branch);
        }
        // Detached or unborn HEAD with a non-symbolic target: write HEAD itself.
        return Ok("HEAD".to_string());
    }
    Ok(name)
}

fn handle_commit(
    parser: &mut StreamParser<impl BufRead>,
    db: &mut FastImportDatabase,
    store: &FileRefStore,
    format: ObjectFormat,
    options: &FastImportOptions,
    marks: &mut HashMap<u64, ObjectId>,
    pack_state: &mut FastImportPackState,
    ref_states: &mut HashMap<String, FastImportRefState>,
    dirty_refs: &mut HashSet<String>,
    ref_name: String,
    query_out: &mut dyn Write,
) -> Result<()> {
    let mut author: Option<Vec<u8>> = None;
    let mut committer: Option<Vec<u8>> = None;
    let mut encoding: Option<Vec<u8>> = None;
    let mut message: Option<Vec<u8>> = None;
    let mut parents: Vec<ObjectId> = Vec::new();
    let mut tree: BTreeMap<Vec<u8>, TreeEntry> = BTreeMap::new();
    let mut unchanged_tree_oid = None;
    let mut tree_changed = false;
    let mut commit_mark: Option<u64> = None;
    let mut deferred_get_mark: Option<Vec<u8>> = None;
    // Whether this commit's parent/tree base has been fixed yet. `from`,
    // `merge`, and `deleteall` all fix it explicitly; otherwise the first
    // filemodify triggers the implicit-parent default below.
    let mut base_fixed = false;

    while let Some(line) = parser.peek_command_line()? {
        let line = line.to_vec();
        if line.starts_with(b"#") {
            parser.next_command_line()?;
        } else if let Some(rest) = line_after(&line, b"mark :") {
            commit_mark = Some(parse_mark(rest)?);
            parser.next_command_line()?;
        } else if line_after(&line, b"original-oid ").is_some() {
            parser.next_command_line()?;
        } else if let Some(rest) = line_after(&line, b"author ") {
            author = Some(parse_fast_import_ident(rest, options.date_format)?);
            parser.next_command_line()?;
        } else if let Some(rest) = line_after(&line, b"committer ") {
            committer = Some(parse_fast_import_ident(rest, options.date_format)?);
            parser.next_command_line()?;
        } else if let Some(rest) = line_after(&line, b"encoding ") {
            encoding = Some(trim_ascii(rest).to_vec());
            parser.next_command_line()?;
        } else if line_after(&line, b"data").is_some() {
            parser.next_command_line()?;
            message = Some(parser.read_data(&line)?);
        } else if let Some(rest) = line_after(&line, b"from ") {
            parser.next_command_line()?;
            // An explicit `from` (even to the zero oid, meaning "no parent")
            // fixes the base, so the implicit default below is suppressed.
            let oid = resolve_committish(db, store, format, marks, ref_states, rest)?;
            if oid != zero_oid(format)? {
                unchanged_tree_oid = Some(seed_tree_from_commit(db, format, &oid, &mut tree)?);
                parents.clear();
                parents.push(oid);
            }
            base_fixed = true;
        } else if let Some(rest) = line_after(&line, b"merge ") {
            parser.next_command_line()?;
            let oid = resolve_committish(db, store, format, marks, ref_states, rest)?;
            if oid != zero_oid(format)? {
                parents.push(oid);
            }
        } else if let Some(rest) = line_after(&line, b"M ") {
            parser.next_command_line()?;
            default_base_from_branch(
                db,
                store,
                format,
                ref_states,
                &ref_name,
                &mut base_fixed,
                &mut parents,
                &mut tree,
                &mut unchanged_tree_oid,
            )?;
            apply_filemodify(
                parser,
                db,
                marks,
                pack_state,
                format,
                &options.submodule_rewrites,
                rest,
                &mut tree,
            )?;
            tree_changed = true;
        } else if let Some(rest) = line_after(&line, b"D ") {
            parser.next_command_line()?;
            default_base_from_branch(
                db,
                store,
                format,
                ref_states,
                &ref_name,
                &mut base_fixed,
                &mut parents,
                &mut tree,
                &mut unchanged_tree_oid,
            )?;
            apply_filedelete(rest, &mut tree)?;
            tree_changed = true;
        } else if let Some(rest) = line_after(&line, b"C ") {
            parser.next_command_line()?;
            default_base_from_branch(
                db,
                store,
                format,
                ref_states,
                &ref_name,
                &mut base_fixed,
                &mut parents,
                &mut tree,
                &mut unchanged_tree_oid,
            )?;
            apply_filecopy(rest, &mut tree)?;
            tree_changed = true;
        } else if let Some(rest) = line_after(&line, b"R ") {
            parser.next_command_line()?;
            default_base_from_branch(
                db,
                store,
                format,
                ref_states,
                &ref_name,
                &mut base_fixed,
                &mut parents,
                &mut tree,
                &mut unchanged_tree_oid,
            )?;
            apply_filerename(rest, &mut tree)?;
            tree_changed = true;
        } else if let Some(rest) = line_after(&line, b"N ") {
            parser.next_command_line()?;
            default_base_from_branch(
                db,
                store,
                format,
                ref_states,
                &ref_name,
                &mut base_fixed,
                &mut parents,
                &mut tree,
                &mut unchanged_tree_oid,
            )?;
            apply_notemodify(
                parser, db, store, marks, pack_state, ref_states, format, rest, &mut tree,
            )?;
            tree_changed = true;
        } else if let Some(rest) = line_after(&line, b"ls ") {
            parser.next_command_line()?;
            default_base_from_branch(
                db,
                store,
                format,
                ref_states,
                &ref_name,
                &mut base_fixed,
                &mut parents,
                &mut tree,
                &mut unchanged_tree_oid,
            )?;
            handle_ls_line(
                db,
                store,
                format,
                marks,
                ref_states,
                Some(&tree),
                commit_mark,
                pack_state,
                rest,
                query_out,
            )?;
        } else if let Some(rest) = line_after(&line, b"cat-blob ") {
            parser.next_command_line()?;
            handle_cat_blob_line(db, store, format, marks, ref_states, rest, query_out)?;
        } else if let Some(rest) = line_after(&line, b"get-mark ") {
            if parser.skipped_blank_lines > 1 {
                return Err(GitError::Command(
                    "fast-import: too many blank lines before get-mark".into(),
                ));
            }
            parser.next_command_line()?;
            deferred_get_mark = Some(rest.to_vec());
            break;
        } else if line.as_slice() == b"done" {
            // `done` is a top-level stream terminator, not part of this commit
            // body. Leave it queued so the outer loop can satisfy --done and do
            // the final flush.
            break;
        } else if line.as_slice() == b"deleteall" {
            parser.next_command_line()?;
            // `deleteall` keeps the commit's implicit parent, but starts edits
            // from an empty tree rather than from that parent's tree.
            default_base_from_branch(
                db,
                store,
                format,
                ref_states,
                &ref_name,
                &mut base_fixed,
                &mut parents,
                &mut tree,
                &mut unchanged_tree_oid,
            )?;
            tree.clear();
            base_fixed = true;
            tree_changed = true;
        } else {
            // First non-commit-body line ends this commit.
            break;
        }
    }

    // An empty commit (no filemodify lines) still inherits the branch tip.
    default_base_from_branch(
        db,
        store,
        format,
        ref_states,
        &ref_name,
        &mut base_fixed,
        &mut parents,
        &mut tree,
        &mut unchanged_tree_oid,
    )?;

    let committer = committer
        .ok_or_else(|| GitError::Command("fast-import: commit missing committer".into()))?;
    let author = author.unwrap_or_else(|| committer.clone());
    let message =
        message.ok_or_else(|| GitError::Command("fast-import: commit missing data".into()))?;

    let build = CommitBuild {
        parents,
        tree,
        unchanged_tree_oid: if tree_changed {
            None
        } else {
            unchanged_tree_oid
        },
        author,
        committer,
        encoding,
        message,
    };
    let oid = write_commit(db, pack_state, build, marks, commit_mark)?;
    pack_state.record_edge(&ref_name, oid);
    dirty_refs.insert(ref_name.clone());
    ref_states.insert(ref_name, FastImportRefState::Tip(oid));
    if let Some(rest) = deferred_get_mark {
        handle_get_mark_line(format, marks, &rest, query_out)?;
    }
    Ok(())
}

fn parse_fast_import_ident(ident: &[u8], date_format: FastImportDateFormat) -> Result<Vec<u8>> {
    let (prefix, date) = split_fast_import_ident(ident)?;
    let mut out = prefix;
    match date_format {
        FastImportDateFormat::Raw => {
            validate_fast_import_raw_date(&date, true)?;
            out.extend_from_slice(&date);
        }
        FastImportDateFormat::RawPermissive => {
            validate_fast_import_raw_date(&date, false)?;
            out.extend_from_slice(&date);
        }
        FastImportDateFormat::Rfc2822 => {
            let date_text = std::str::from_utf8(&date)
                .map_err(|_| GitError::InvalidFormat("fast-import: date is not utf8".into()))?;
            let (seconds, timezone) = crate::commands::approxidate::parse_commit_date(date_text)
                .ok_or_else(|| {
                    GitError::Command(format!(
                        "fast-import: invalid rfc2822 date \"{}\" in ident: {}",
                        String::from_utf8_lossy(&date),
                        String::from_utf8_lossy(ident)
                    ))
                })?;
            out.extend_from_slice(format!("{seconds} {timezone}").as_bytes());
        }
        FastImportDateFormat::Now => {
            if date.as_slice() != b"now" {
                return Err(GitError::Command(format!(
                    "fast-import: date in ident must be 'now': {}",
                    String::from_utf8_lossy(ident)
                )));
            }
            out.extend_from_slice(format!("{} +0000", current_unix_seconds()).as_bytes());
        }
    }
    Ok(out)
}

fn split_fast_import_ident(ident: &[u8]) -> Result<(Vec<u8>, Vec<u8>)> {
    let bytes = if ident.first() == Some(&b'<') {
        let mut fixed = Vec::with_capacity(ident.len() + 1);
        fixed.push(b' ');
        fixed.extend_from_slice(ident);
        fixed
    } else {
        ident.to_vec()
    };
    let ltgt = bytes
        .iter()
        .position(|byte| matches!(*byte, b'<' | b'>'))
        .ok_or_else(|| {
            GitError::Command(format!(
                "fast-import: missing < in ident string: {}",
                String::from_utf8_lossy(&bytes)
            ))
        })?;
    if bytes[ltgt] != b'<' {
        return Err(GitError::Command(format!(
            "fast-import: missing < in ident string: {}",
            String::from_utf8_lossy(&bytes)
        )));
    }
    if ltgt > 0 && bytes[ltgt - 1] != b' ' {
        return Err(GitError::Command(format!(
            "fast-import: missing space before < in ident string: {}",
            String::from_utf8_lossy(&bytes)
        )));
    }
    let after_lt = ltgt + 1;
    let rel = bytes[after_lt..]
        .iter()
        .position(|byte| matches!(*byte, b'<' | b'>'))
        .ok_or_else(|| {
            GitError::Command(format!(
                "fast-import: missing > in ident string: {}",
                String::from_utf8_lossy(&bytes)
            ))
        })?;
    let gt = after_lt + rel;
    if bytes[gt] != b'>' {
        return Err(GitError::Command(format!(
            "fast-import: missing > in ident string: {}",
            String::from_utf8_lossy(&bytes)
        )));
    }
    if bytes.get(gt + 1) != Some(&b' ') {
        return Err(GitError::Command(format!(
            "fast-import: missing space after > in ident string: {}",
            String::from_utf8_lossy(&bytes)
        )));
    }
    Ok((bytes[..gt + 2].to_vec(), bytes[gt + 2..].to_vec()))
}

fn validate_fast_import_raw_date(date: &[u8], strict: bool) -> Result<()> {
    let text = std::str::from_utf8(date)
        .map_err(|_| GitError::InvalidFormat("fast-import: raw date is not utf8".into()))?;
    let (seconds, timezone) = text.split_once(' ').ok_or_else(|| {
        GitError::Command(format!(
            "fast-import: invalid raw date \"{}\"",
            String::from_utf8_lossy(date)
        ))
    })?;
    if seconds.is_empty()
        || !seconds.bytes().all(|byte| byte.is_ascii_digit())
        || timezone.len() < 2
        || !matches!(timezone.as_bytes()[0], b'+' | b'-')
        || !timezone.as_bytes()[1..].iter().all(u8::is_ascii_digit)
    {
        return Err(GitError::Command(format!(
            "fast-import: invalid raw date \"{}\"",
            String::from_utf8_lossy(date)
        )));
    }
    if strict {
        let offset = timezone[1..].parse::<u64>().map_err(|_| {
            GitError::Command(format!(
                "fast-import: invalid raw date \"{}\"",
                String::from_utf8_lossy(date)
            ))
        })?;
        if offset > 1400 {
            return Err(GitError::Command(format!(
                "fast-import: invalid raw date \"{}\"",
                String::from_utf8_lossy(date)
            )));
        }
    }
    Ok(())
}

fn write_commit(
    db: &mut FastImportDatabase,
    pack_state: &mut FastImportPackState,
    build: CommitBuild,
    marks: &mut HashMap<u64, ObjectId>,
    commit_mark: Option<u64>,
) -> Result<ObjectId> {
    let CommitBuild {
        parents,
        tree,
        unchanged_tree_oid,
        author,
        committer,
        encoding,
        message,
    } = build;
    let tree = Arc::new(tree);
    // Build the tree object from the accumulated entries (git tree ordering is
    // handled by Tree::write, which sorts on the canonical key).
    let tree_oid = match unchanged_tree_oid {
        Some(tree_oid) => tree_oid,
        None => write_tree_from_map(db, pack_state, tree.as_ref())?,
    };
    let commit = Commit {
        tree: tree_oid,
        parents,
        author,
        committer,
        encoding,
        message,
    };
    let oid = write_fast_import_object(
        db,
        pack_state,
        EncodedObject::new(ObjectType::Commit, commit.write()),
    )?;
    if let Some(mark) = commit_mark {
        marks.insert(mark, oid);
    }
    db.remember_commit_tree(oid, tree_oid, tree);
    Ok(oid)
}

fn handle_blob(
    parser: &mut StreamParser<impl BufRead>,
    db: &mut FastImportDatabase,
    pack_state: &mut FastImportPackState,
    marks: &mut HashMap<u64, ObjectId>,
    _rest: &[u8],
) -> Result<()> {
    let mut blob_mark: Option<u64> = None;
    let data;
    loop {
        let Some(line) = parser.peek_command_line()? else {
            return Err(GitError::Command("fast-import: blob missing data".into()));
        };
        let line = line.to_vec();
        if line.starts_with(b"#") {
            parser.next_command_line()?;
        } else if let Some(rest) = line_after(&line, b"mark :") {
            blob_mark = Some(parse_mark(rest)?);
            parser.next_command_line()?;
        } else if line_after(&line, b"original-oid ").is_some() {
            parser.next_command_line()?;
        } else if line_after(&line, b"data").is_some() {
            parser.next_command_line()?;
            data = parser.read_data(&line)?;
            break;
        } else {
            return Err(GitError::Command(format!(
                "fast-import: unexpected line in blob: {}",
                String::from_utf8_lossy(&line).trim_end()
            )));
        }
    }
    let oid = write_fast_import_object(db, pack_state, EncodedObject::new(ObjectType::Blob, data))?;
    if let Some(mark) = blob_mark {
        marks.insert(mark, oid);
    }
    Ok(())
}

fn handle_tag(
    parser: &mut StreamParser<impl BufRead>,
    db: &mut FastImportDatabase,
    store: &FileRefStore,
    format: ObjectFormat,
    date_format: FastImportDateFormat,
    marks: &mut HashMap<u64, ObjectId>,
    pack_state: &mut FastImportPackState,
    ref_states: &mut HashMap<String, FastImportRefState>,
    dirty_refs: &mut HashSet<String>,
    operand: &[u8],
) -> Result<()> {
    let tag_operand = trim_ascii(operand);
    if tag_operand.is_empty() {
        return Err(GitError::Command("fast-import: tag missing name".into()));
    }
    let tag_ref = fast_import_tag_ref_name(tag_operand)?;
    let tag_name = fast_import_tag_object_name(tag_operand).to_vec();
    let mut tag_mark: Option<u64> = None;
    let mut target: Option<ObjectId> = None;
    let mut tagger: Option<Vec<u8>> = None;
    let mut message: Option<Vec<u8>> = None;

    while let Some(line) = parser.peek_command_line()? {
        let line = line.to_vec();
        if line.starts_with(b"#") {
            parser.next_command_line()?;
        } else if let Some(rest) = line_after(&line, b"mark :") {
            tag_mark = Some(parse_mark(rest)?);
            parser.next_command_line()?;
        } else if let Some(rest) = line_after(&line, b"from ") {
            parser.next_command_line()?;
            let oid = resolve_objectish(db, store, format, marks, ref_states, rest)?;
            if oid == zero_oid(format)? {
                return Err(GitError::Command(
                    "fast-import: tag cannot target zero oid".into(),
                ));
            }
            target = Some(oid);
        } else if line_after(&line, b"original-oid ").is_some() {
            parser.next_command_line()?;
        } else if let Some(rest) = line_after(&line, b"tagger ") {
            tagger = Some(parse_fast_import_ident(rest, date_format)?);
            parser.next_command_line()?;
        } else if line_after(&line, b"data").is_some() {
            parser.next_command_line()?;
            message = Some(parser.read_data(&line)?);
        } else {
            break;
        }
    }

    let target = target.ok_or_else(|| GitError::Command("fast-import: tag missing from".into()))?;
    let object = db.read_object(&target)?;
    let tag = Tag {
        object: target,
        object_type: object.object_type,
        name: tag_name,
        tagger,
        message: message
            .ok_or_else(|| GitError::Command("fast-import: tag missing data".into()))?,
        raw_body: None,
    };
    let oid = write_fast_import_object(
        db,
        pack_state,
        EncodedObject::new(ObjectType::Tag, tag.write()),
    )?;
    if let Some(mark) = tag_mark {
        marks.insert(mark, oid);
    }
    dirty_refs.insert(tag_ref.clone());
    ref_states.insert(tag_ref, FastImportRefState::Tip(oid));
    Ok(())
}

fn handle_alias(
    parser: &mut StreamParser<impl BufRead>,
    db: &FastImportDatabase,
    store: &FileRefStore,
    format: ObjectFormat,
    marks: &mut HashMap<u64, ObjectId>,
    ref_states: &HashMap<String, FastImportRefState>,
    rest: &[u8],
) -> Result<()> {
    if !trim_ascii(rest).is_empty() {
        return Err(GitError::Command("fast-import: malformed alias".into()));
    }
    let mut alias_mark: Option<u64> = None;
    let mut target: Option<ObjectId> = None;
    while let Some(line) = parser.peek_command_line()? {
        let line = line.to_vec();
        if line.starts_with(b"#") {
            parser.next_command_line()?;
        } else if let Some(rest) = line_after(&line, b"mark :") {
            alias_mark = Some(parse_mark(rest)?);
            parser.next_command_line()?;
        } else if let Some(rest) = line_after(&line, b"to ") {
            target = Some(resolve_committish(
                db, store, format, marks, ref_states, rest,
            )?);
            parser.next_command_line()?;
        } else {
            break;
        }
    }
    let mark =
        alias_mark.ok_or_else(|| GitError::Command("fast-import: alias missing mark".into()))?;
    let oid = target.ok_or_else(|| GitError::Command("fast-import: alias missing to".into()))?;
    marks.insert(mark, oid);
    Ok(())
}

fn handle_reset(
    parser: &mut StreamParser<impl BufRead>,
    db: &FastImportDatabase,
    store: &FileRefStore,
    format: ObjectFormat,
    marks: &HashMap<u64, ObjectId>,
    ref_states: &mut HashMap<String, FastImportRefState>,
    dirty_refs: &mut HashSet<String>,
    operand: &[u8],
) -> Result<()> {
    let ref_name = parse_fast_import_ref_name(operand)?;
    let from = loop {
        let Some(peek) = parser.peek_command_line()? else {
            break None;
        };
        if peek.starts_with(b"#") {
            parser.next_command_line()?;
            continue;
        } else if let Some(rest) = line_after(peek, b"from ") {
            let rest = rest.to_vec();
            parser.next_command_line()?;
            break Some(rest);
        } else {
            break None;
        }
    };

    let Some(from) = from else {
        // A bare reset only clears fast-import's in-memory branch state; unlike
        // `from <zero-oid>` it does not request deletion of an existing ref.
        ref_states.insert(ref_name, FastImportRefState::Empty);
        return Ok(());
    };
    let oid = resolve_committish(db, store, format, marks, ref_states, &from)?;
    if oid == zero_oid(format)? {
        dirty_refs.insert(ref_name.clone());
        ref_states.insert(ref_name, FastImportRefState::Empty);
    } else {
        dirty_refs.insert(ref_name.clone());
        ref_states.insert(ref_name, FastImportRefState::Tip(oid));
    }
    Ok(())
}

fn parse_fast_import_ref_name(operand: &[u8]) -> Result<String> {
    let text = std::str::from_utf8(trim_ascii(operand))
        .map_err(|_| GitError::InvalidFormat("fast-import: ref name is not utf8".into()))?;
    if text.is_empty() {
        return Err(GitError::Command("fast-import: missing ref name".into()));
    }
    Ok(text.to_string())
}

fn fast_import_tag_ref_name(operand: &[u8]) -> Result<String> {
    let text = std::str::from_utf8(trim_ascii(operand))
        .map_err(|_| GitError::InvalidFormat("fast-import: tag name is not utf8".into()))?;
    if text.starts_with("refs/tags/") {
        Ok(text.to_string())
    } else {
        Ok(format!("refs/tags/{text}"))
    }
}

fn fast_import_tag_object_name(operand: &[u8]) -> &[u8] {
    let operand = trim_ascii(operand);
    operand.strip_prefix(b"refs/tags/").unwrap_or(operand)
}

fn delete_ref_if_exists(store: &FileRefStore, name: &str) -> Result<()> {
    if store.read_ref(name)?.is_some() {
        let mut tx = store.transaction();
        tx.delete_with_precondition(name, sley_refs::RefDeletePrecondition::Any, None);
        tx.commit()?;
    }
    Ok(())
}

/// Apply an `M <mode> <dataref> <path>` filemodify to the working tree map.
/// `<dataref>` is `inline` (an inline `data` block follows) or an oid/mark.
fn apply_filemodify(
    parser: &mut StreamParser<impl BufRead>,
    db: &mut FastImportDatabase,
    marks: &HashMap<u64, ObjectId>,
    pack_state: &mut FastImportPackState,
    format: ObjectFormat,
    submodule_rewrites: &HashMap<ObjectId, ObjectId>,
    rest: &[u8],
    tree: &mut BTreeMap<Vec<u8>, TreeEntry>,
) -> Result<()> {
    // rest = "<mode> <dataref> <path>"
    let (mode_bytes, after_mode) = split_field(rest);
    let mode_text = std::str::from_utf8(mode_bytes)
        .map_err(|_| GitError::InvalidFormat("fast-import: mode not utf8".into()))?;
    let mode = parse_filemode(mode_text)?;
    let (dataref, path) = split_field_preserve_rest(after_mode);
    let path = parse_path_at_eol(path, mode == 0o040000, "path")?;
    validate_fast_import_path(&path, FastImportPathKind::FileModify(mode))?;

    if mode == 0o040000 {
        if dataref == b"inline" {
            return Err(GitError::Command(
                "fast-import: tree mode cannot use inline data".into(),
            ));
        }
        let tree_oid = resolve_typed_dataref(db, format, marks, dataref, ObjectType::Tree)?;
        replace_tree_prefix(db, format, tree, &path, &tree_oid)?;
        return Ok(());
    }

    let oid = if mode == 0o160000 {
        if dataref == b"inline" {
            return Err(GitError::Command(
                "fast-import: gitlink cannot use inline data".into(),
            ));
        }
        let oid = resolve_gitlink_dataref(db, format, marks, dataref)?;
        submodule_rewrites.get(&oid).copied().unwrap_or(oid)
    } else if dataref == b"inline" {
        // An inline `data` block is the next command line.
        let Some(data_line) = parser.next_non_comment_command_line()? else {
            return Err(GitError::Command(
                "fast-import: M inline missing data block".into(),
            ));
        };
        if line_after(&data_line, b"data").is_none() {
            return Err(GitError::Command(
                "fast-import: M inline must be followed by data".into(),
            ));
        }
        let content = parser.read_data(&data_line)?;
        write_fast_import_object(
            db,
            pack_state,
            EncodedObject::new(ObjectType::Blob, content),
        )?
    } else {
        resolve_typed_dataref(db, format, marks, dataref, ObjectType::Blob)?
    };

    remove_conflicts_for_path(tree, &path);
    tree.insert(
        path.clone(),
        TreeEntry {
            mode,
            name: BString::from(path),
            oid,
        },
    );
    Ok(())
}

fn apply_filedelete(rest: &[u8], tree: &mut BTreeMap<Vec<u8>, TreeEntry>) -> Result<()> {
    let path = parse_path_at_eol(rest, true, "path")?;
    validate_fast_import_path(&path, FastImportPathKind::Any)?;
    delete_tree_prefix(tree, &path);
    Ok(())
}

fn apply_filecopy(rest: &[u8], tree: &mut BTreeMap<Vec<u8>, TreeEntry>) -> Result<()> {
    let (src, after_src) = parse_path_before_space(rest, "source")?;
    let dst = parse_path_at_eol(after_src, true, "dest")?;
    validate_fast_import_path(&src, FastImportPathKind::Any)?;
    validate_fast_import_path(&dst, FastImportPathKind::Any)?;
    copy_tree_prefix(tree, &src, &dst)
}

fn apply_filerename(rest: &[u8], tree: &mut BTreeMap<Vec<u8>, TreeEntry>) -> Result<()> {
    let (src, after_src) = parse_path_before_space(rest, "source")?;
    let dst = parse_path_at_eol(after_src, true, "dest")?;
    validate_fast_import_path(&src, FastImportPathKind::Any)?;
    validate_fast_import_path(&dst, FastImportPathKind::Any)?;
    rename_tree_prefix(tree, &src, &dst)
}

fn copy_tree_prefix(tree: &mut BTreeMap<Vec<u8>, TreeEntry>, src: &[u8], dst: &[u8]) -> Result<()> {
    let copies = entries_under_prefix(tree, src);
    if copies.is_empty() {
        return Err(GitError::Command(format!(
            "fast-import: source path '{}' not found",
            String::from_utf8_lossy(src)
        )));
    }
    delete_tree_prefix(tree, dst);
    for (src_path, mut entry) in copies {
        let new_path = rewrite_prefix(&src_path, src, dst);
        entry.name = BString::from(new_path.clone());
        tree.insert(new_path, entry);
    }
    Ok(())
}

fn rename_tree_prefix(
    tree: &mut BTreeMap<Vec<u8>, TreeEntry>,
    src: &[u8],
    dst: &[u8],
) -> Result<()> {
    let copies = entries_under_prefix(tree, src);
    if copies.is_empty() {
        return Err(GitError::Command(format!(
            "fast-import: source path '{}' not found",
            String::from_utf8_lossy(src)
        )));
    }
    delete_tree_prefix(tree, src);
    delete_tree_prefix(tree, dst);
    for (src_path, mut entry) in copies {
        let new_path = rewrite_prefix(&src_path, src, dst);
        entry.name = BString::from(new_path.clone());
        tree.insert(new_path, entry);
    }
    Ok(())
}

fn entries_under_prefix(
    tree: &BTreeMap<Vec<u8>, TreeEntry>,
    prefix: &[u8],
) -> Vec<(Vec<u8>, TreeEntry)> {
    if prefix.is_empty() {
        return tree
            .iter()
            .map(|(path, entry)| (path.clone(), entry.clone()))
            .collect();
    }
    tree.iter()
        .filter(|(path, _)| path_matches_prefix(path, prefix))
        .map(|(path, entry)| (path.clone(), entry.clone()))
        .collect()
}

fn delete_tree_prefix(tree: &mut BTreeMap<Vec<u8>, TreeEntry>, prefix: &[u8]) {
    if prefix.is_empty() {
        tree.clear();
        return;
    }
    let keys = tree
        .keys()
        .filter(|path| path_matches_prefix(path, prefix))
        .cloned()
        .collect::<Vec<_>>();
    for key in keys {
        tree.remove(&key);
    }
}

fn replace_tree_prefix(
    db: &FastImportDatabase,
    format: ObjectFormat,
    tree: &mut BTreeMap<Vec<u8>, TreeEntry>,
    prefix: &[u8],
    tree_oid: &ObjectId,
) -> Result<()> {
    delete_tree_prefix(tree, prefix);
    seed_tree_entries(db, format, tree_oid, prefix, tree)
}

fn remove_conflicts_for_path(tree: &mut BTreeMap<Vec<u8>, TreeEntry>, path: &[u8]) {
    delete_tree_prefix(tree, path);
    let mut ancestors = Vec::new();
    for idx in path
        .iter()
        .enumerate()
        .filter_map(|(idx, byte)| (*byte == b'/').then_some(idx))
    {
        ancestors.push(path[..idx].to_vec());
    }
    for ancestor in ancestors {
        tree.remove(&ancestor);
    }
}

fn path_matches_prefix(path: &[u8], prefix: &[u8]) -> bool {
    path == prefix || path.starts_with(prefix) && path.get(prefix.len()) == Some(&b'/')
}

fn rewrite_prefix(path: &[u8], src: &[u8], dst: &[u8]) -> Vec<u8> {
    if src.is_empty() {
        return join_path(dst, path);
    }
    let rest = if path == src {
        &[][..]
    } else {
        &path[src.len() + 1..]
    };
    join_path(dst, rest)
}

fn join_path(prefix: &[u8], rest: &[u8]) -> Vec<u8> {
    if prefix.is_empty() {
        return rest.to_vec();
    }
    if rest.is_empty() {
        return prefix.to_vec();
    }
    let mut out = prefix.to_vec();
    out.push(b'/');
    out.extend_from_slice(rest);
    out
}

/// Apply an `N <dataref> <commit-ish>` notemodify: attach a note blob to the
/// annotated object inside a notes tree. `<dataref>` is `inline` (an inline
/// `data` block follows), an oid, or a mark; `<commit-ish>` is the annotated
/// object. The note is stored flat, keyed by the annotated object's full hex —
/// a valid (if un-fanned) notes layout that the notes reader handles, and which
/// notes-writing commands later re-fan as needed.
fn apply_notemodify(
    parser: &mut StreamParser<impl BufRead>,
    db: &mut FastImportDatabase,
    store: &FileRefStore,
    marks: &HashMap<u64, ObjectId>,
    pack_state: &mut FastImportPackState,
    ref_states: &HashMap<String, FastImportRefState>,
    format: ObjectFormat,
    rest: &[u8],
    tree: &mut BTreeMap<Vec<u8>, TreeEntry>,
) -> Result<()> {
    // rest = "<dataref> <commit-ish>"
    let (dataref, committish) = split_field(rest);

    let blob_oid = if dataref == b"inline" {
        let Some(data_line) = parser.next_non_comment_command_line()? else {
            return Err(GitError::Command(
                "fast-import: N inline missing data block".into(),
            ));
        };
        if line_after(&data_line, b"data").is_none() {
            return Err(GitError::Command(
                "fast-import: N inline must be followed by data".into(),
            ));
        }
        let content = parser.read_data(&data_line)?;
        write_fast_import_object(
            db,
            pack_state,
            EncodedObject::new(ObjectType::Blob, content),
        )?
    } else {
        resolve_dataref(format, marks, dataref)?
    };

    let target = resolve_committish(db, store, format, marks, ref_states, committish)?;
    if target == zero_oid(format)? {
        return Err(GitError::Command(
            "fast-import: cannot add note for empty branch".into(),
        ));
    }
    let path = target.to_hex().into_bytes();
    tree.insert(
        path.clone(),
        TreeEntry {
            mode: 0o100644,
            name: BString::from(path),
            oid: blob_oid,
        },
    );
    Ok(())
}

/// Resolve a `from`/`merge` committish: a mark (`:N`), a full ref, or a hex oid.
fn resolve_committish(
    db: &FastImportDatabase,
    store: &FileRefStore,
    format: ObjectFormat,
    marks: &HashMap<u64, ObjectId>,
    ref_states: &HashMap<String, FastImportRefState>,
    operand: &[u8],
) -> Result<ObjectId> {
    let operand = trim_ascii(operand);
    let text = std::str::from_utf8(operand)
        .map_err(|_| GitError::InvalidFormat("fast-import: committish not utf8".into()))?;
    let (base, suffix) = split_fast_import_revision_suffix(text)?;
    let mut oid = resolve_committish_base(db, store, format, marks, ref_states, base)?;
    if oid == zero_oid(format)? {
        return Ok(oid);
    }
    apply_fast_import_revision_suffix(db, format, &mut oid, suffix)?;
    Ok(oid)
}

fn resolve_committish_base(
    db: &FastImportDatabase,
    store: &FileRefStore,
    format: ObjectFormat,
    marks: &HashMap<u64, ObjectId>,
    ref_states: &HashMap<String, FastImportRefState>,
    text: &str,
) -> Result<ObjectId> {
    if let Some(mark) = text.strip_prefix(':') {
        let n: u64 = mark
            .parse()
            .map_err(|_| GitError::Command("fast-import: missing space after mark".into()))?;
        return marks
            .get(&n)
            .copied()
            .ok_or_else(|| GitError::Command(format!("fast-import: unknown mark :{n}")));
    }
    if let Some(state) = ref_states.get(text) {
        return match state {
            FastImportRefState::Empty => Ok(zero_oid(format)?),
            FastImportRefState::Tip(oid) => Ok(*oid),
        };
    }
    // Try a full ref name (e.g. HEAD, refs/heads/main).
    if let Some(oid) = resolve_ref_peeled(store, text)? {
        return Ok(oid);
    }
    if text == "HEAD"
        && let Some(branch) = store.current_branch_ref()?
        && let Some(oid) = resolve_ref_peeled(store, &branch)?
    {
        return Ok(oid);
    }
    // Fall back to a literal hex oid.
    let oid = ObjectId::from_hex(format, text)
        .map_err(|_| GitError::Command(format!("fast-import: cannot resolve '{text}'")))?;
    if oid == zero_oid(format)? {
        return Ok(oid);
    }
    if db.contains(&oid)? {
        Ok(oid)
    } else {
        Err(GitError::Command(format!(
            "fast-import: object {text} not found"
        )))
    }
}

#[derive(Clone, Copy)]
enum FastImportRevSuffix {
    Peel,
    Parent(usize),
    FirstParent(usize),
}

fn split_fast_import_revision_suffix(text: &str) -> Result<(&str, Option<FastImportRevSuffix>)> {
    if let Some(base) = text.strip_suffix("^0") {
        return Ok((base, Some(FastImportRevSuffix::Peel)));
    }
    if let Some((base, count)) = text.rsplit_once('~')
        && !base.is_empty()
        && !count.is_empty()
        && count.bytes().all(|byte| byte.is_ascii_digit())
    {
        let count = count
            .parse::<usize>()
            .map_err(|_| GitError::Command(format!("fast-import: bad revision '{text}'")))?;
        return Ok((base, Some(FastImportRevSuffix::FirstParent(count))));
    }
    if let Some((base, parent)) = text.rsplit_once('^')
        && !base.is_empty()
        && (parent.is_empty() || parent.bytes().all(|byte| byte.is_ascii_digit()))
    {
        let parent = if parent.is_empty() {
            1
        } else {
            parent
                .parse::<usize>()
                .map_err(|_| GitError::Command(format!("fast-import: bad revision '{text}'")))?
        };
        return Ok((base, Some(FastImportRevSuffix::Parent(parent))));
    }
    Ok((text, None))
}

fn apply_fast_import_revision_suffix(
    db: &FastImportDatabase,
    format: ObjectFormat,
    oid: &mut ObjectId,
    suffix: Option<FastImportRevSuffix>,
) -> Result<()> {
    match suffix {
        None | Some(FastImportRevSuffix::Peel) => {
            ensure_fast_import_commitish(db, format, oid)?;
        }
        Some(FastImportRevSuffix::Parent(parent)) => {
            if parent == 0 {
                ensure_fast_import_commitish(db, format, oid)?;
                return Ok(());
            }
            let commit = read_fast_import_commit(db, format, oid)?;
            let selected = commit.parents.get(parent - 1).copied().ok_or_else(|| {
                GitError::Command(format!(
                    "fast-import: revision {oid}^{parent} has no parent"
                ))
            })?;
            *oid = selected;
        }
        Some(FastImportRevSuffix::FirstParent(count)) => {
            ensure_fast_import_commitish(db, format, oid)?;
            for _ in 0..count {
                let commit = read_fast_import_commit(db, format, oid)?;
                let selected = commit.parents.first().copied().ok_or_else(|| {
                    GitError::Command(format!("fast-import: revision {oid} has no parent"))
                })?;
                *oid = selected;
            }
        }
    }
    Ok(())
}

fn ensure_fast_import_commitish(
    db: &FastImportDatabase,
    format: ObjectFormat,
    oid: &ObjectId,
) -> Result<()> {
    let _ = read_fast_import_commit(db, format, oid)?;
    Ok(())
}

fn read_fast_import_commit(
    db: &FastImportDatabase,
    format: ObjectFormat,
    oid: &ObjectId,
) -> Result<Commit> {
    let object = db.read_object(oid)?;
    if object.object_type != ObjectType::Commit {
        return Err(GitError::Command(format!(
            "fast-import: {oid} is not a commit"
        )));
    }
    Commit::parse(format, &object.body)
}

/// Resolve an `M`-line dataref that is a mark (`:N`) or a hex oid.
fn resolve_dataref(
    format: ObjectFormat,
    marks: &HashMap<u64, ObjectId>,
    dataref: &[u8],
) -> Result<ObjectId> {
    let dataref = trim_ascii(dataref);
    if let Some(mark) = dataref.strip_prefix(b":") {
        let text = std::str::from_utf8(mark)
            .map_err(|_| GitError::InvalidFormat("fast-import: mark not utf8".into()))?;
        let n: u64 = text
            .parse()
            .map_err(|_| GitError::Command("fast-import: missing space after mark".into()))?;
        return marks
            .get(&n)
            .copied()
            .ok_or_else(|| GitError::Command(format!("fast-import: unknown mark :{n}")));
    }
    let text = std::str::from_utf8(dataref)
        .map_err(|_| GitError::InvalidFormat("fast-import: dataref not utf8".into()))?;
    ObjectId::from_hex(format, text).map_err(|_| {
        if looks_like_hex_with_trailing_garbage(text, format.hex_len()) {
            GitError::Command("fast-import: missing space after SHA1".into())
        } else {
            GitError::Command(format!("fast-import: invalid dataref '{text}'"))
        }
    })
}

fn resolve_objectish(
    db: &FastImportDatabase,
    store: &FileRefStore,
    format: ObjectFormat,
    marks: &HashMap<u64, ObjectId>,
    ref_states: &HashMap<String, FastImportRefState>,
    operand: &[u8],
) -> Result<ObjectId> {
    let operand = trim_ascii(operand);
    let text = std::str::from_utf8(operand)
        .map_err(|_| GitError::InvalidFormat("fast-import: objectish not utf8".into()))?;
    let oid = resolve_committish_base(db, store, format, marks, ref_states, text)?;
    db.read_object(&oid)?;
    Ok(oid)
}

fn resolve_typed_dataref(
    db: &FastImportDatabase,
    format: ObjectFormat,
    marks: &HashMap<u64, ObjectId>,
    dataref: &[u8],
    want: ObjectType,
) -> Result<ObjectId> {
    let oid = resolve_dataref(format, marks, dataref)?;
    let object = db.read_object(&oid)?;
    if object.object_type != want {
        return Err(GitError::Command(format!(
            "fast-import: expected {}, got {} for {oid}",
            want.as_str(),
            object.object_type.as_str()
        )));
    }
    Ok(oid)
}

fn resolve_gitlink_dataref(
    db: &FastImportDatabase,
    format: ObjectFormat,
    marks: &HashMap<u64, ObjectId>,
    dataref: &[u8],
) -> Result<ObjectId> {
    let dataref = trim_ascii(dataref);
    if dataref.starts_with(b":") {
        return resolve_typed_dataref(db, format, marks, dataref, ObjectType::Commit);
    }
    let text = std::str::from_utf8(dataref)
        .map_err(|_| GitError::InvalidFormat("fast-import: gitlink oid not utf8".into()))?;
    ObjectId::from_hex(format, text)
        .map_err(|_| GitError::Command(format!("fast-import: bad gitlink object id '{text}'")))
}

fn handle_cat_blob_line(
    db: &mut FastImportDatabase,
    store: &FileRefStore,
    format: ObjectFormat,
    marks: &HashMap<u64, ObjectId>,
    ref_states: &HashMap<String, FastImportRefState>,
    rest: &[u8],
    out: &mut dyn Write,
) -> Result<()> {
    let oid = resolve_cat_blob_dataref(db, store, format, marks, ref_states, rest)?;
    let object = db.read_object(&oid)?;
    write!(
        out,
        "{} {} {}\n",
        oid,
        object.object_type.as_str(),
        object.body.len()
    )?;
    out.write_all(&object.body)?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}

fn handle_get_mark_line(
    format: ObjectFormat,
    marks: &HashMap<u64, ObjectId>,
    rest: &[u8],
    out: &mut dyn Write,
) -> Result<()> {
    let oid = resolve_dataref(format, marks, rest)?;
    writeln!(out, "{oid}")?;
    out.flush()?;
    Ok(())
}

fn handle_ls_line(
    db: &mut FastImportDatabase,
    store: &FileRefStore,
    format: ObjectFormat,
    marks: &HashMap<u64, ObjectId>,
    ref_states: &HashMap<String, FastImportRefState>,
    staged_tree: Option<&BTreeMap<Vec<u8>, TreeEntry>>,
    current_mark: Option<u64>,
    pack_state: &mut FastImportPackState,
    rest: &[u8],
    out: &mut dyn Write,
) -> Result<()> {
    let (tree, path) = if let Some(tree) = staged_tree
        && rest.first() == Some(&b'"')
    {
        (tree.clone(), parse_path_at_eol(rest, true, "path")?)
    } else {
        let (dataref, after_dataref) = split_field_preserve_rest(rest);
        let path = parse_path_at_eol(after_dataref, true, "path")?;
        if staged_tree.is_some()
            && current_mark.is_some()
            && mark_ref_matches(dataref, current_mark.expect("checked is_some"))
        {
            (staged_tree.expect("checked is_some").clone(), path)
        } else {
            let root = resolve_ls_root_tree(db, store, format, marks, ref_states, dataref)?;
            let mut map = BTreeMap::new();
            seed_tree_entries(db, format, &root, &[], &mut map)?;
            (map, path)
        }
    };
    validate_fast_import_path(&path, FastImportPathKind::Any)?;
    write_ls_result(db, pack_state, &tree, &path, out)
}

fn resolve_cat_blob_dataref(
    db: &FastImportDatabase,
    store: &FileRefStore,
    format: ObjectFormat,
    marks: &HashMap<u64, ObjectId>,
    ref_states: &HashMap<String, FastImportRefState>,
    rest: &[u8],
) -> Result<ObjectId> {
    let operand = trim_ascii(rest);
    if operand.starts_with(b":") {
        return resolve_dataref(format, marks, operand);
    }
    if let Ok(text) = std::str::from_utf8(operand)
        && let Ok(oid) = ObjectId::from_hex(format, text)
    {
        return Ok(oid);
    }
    resolve_objectish(db, store, format, marks, ref_states, operand)
}

fn resolve_ls_root_tree(
    db: &FastImportDatabase,
    store: &FileRefStore,
    format: ObjectFormat,
    marks: &HashMap<u64, ObjectId>,
    ref_states: &HashMap<String, FastImportRefState>,
    dataref: &[u8],
) -> Result<ObjectId> {
    if let Ok(text) = std::str::from_utf8(trim_ascii(dataref))
        && looks_like_hex_with_trailing_garbage(text, format.hex_len())
    {
        return Err(GitError::Command(
            "fast-import: missing space after tree-ish".into(),
        ));
    }
    let oid = resolve_objectish(db, store, format, marks, ref_states, dataref)?;
    object_to_tree_oid(db, format, oid)
}

fn object_to_tree_oid(
    db: &FastImportDatabase,
    format: ObjectFormat,
    mut oid: ObjectId,
) -> Result<ObjectId> {
    loop {
        let object = db.read_object(&oid)?;
        match object.object_type {
            ObjectType::Tree => return Ok(oid),
            ObjectType::Commit => {
                let commit = Commit::parse(format, &object.body)?;
                return Ok(commit.tree);
            }
            ObjectType::Tag => {
                let tag = Tag::parse(format, &object.body)?;
                oid = tag.object;
            }
            ObjectType::Blob => {
                return Err(GitError::Command(format!(
                    "fast-import: {oid} is not a tree-ish"
                )));
            }
        }
    }
}

fn write_ls_result(
    db: &mut FastImportDatabase,
    pack_state: &mut FastImportPackState,
    tree: &BTreeMap<Vec<u8>, TreeEntry>,
    path: &[u8],
    out: &mut dyn Write,
) -> Result<()> {
    if path.is_empty() {
        let oid = write_tree_from_map(db, pack_state, tree)?;
        writeln!(out, "040000 tree {oid}\t")?;
        out.flush()?;
        return Ok(());
    }
    if let Some(entry) = tree.get(path) {
        writeln!(
            out,
            "{:06o} {} {}\t{}",
            entry.mode,
            object_type_for_tree_entry(entry.mode).as_str(),
            entry.oid,
            String::from_utf8_lossy(path)
        )?;
        out.flush()?;
        return Ok(());
    }
    let subtree = entries_under_prefix(tree, path);
    if subtree.is_empty() {
        writeln!(out, "missing {}", String::from_utf8_lossy(path))?;
        out.flush()?;
        return Ok(());
    }
    let mut submap = BTreeMap::new();
    for (src, mut entry) in subtree {
        let rewritten = rewrite_prefix(&src, path, b"");
        entry.name = BString::from(rewritten.clone());
        submap.insert(rewritten, entry);
    }
    let oid = write_tree_from_map(db, pack_state, &submap)?;
    writeln!(out, "040000 tree {oid}\t{}", String::from_utf8_lossy(path))?;
    out.flush()?;
    Ok(())
}

fn object_type_for_tree_entry(mode: u32) -> ObjectType {
    match mode {
        0o040000 => ObjectType::Tree,
        0o160000 => ObjectType::Commit,
        _ => ObjectType::Blob,
    }
}

fn looks_like_hex_with_trailing_garbage(text: &str, hex_len: usize) -> bool {
    text.len() > hex_len
        && text
            .as_bytes()
            .get(..hex_len)
            .is_some_and(|prefix| prefix.iter().all(|byte| byte.is_ascii_hexdigit()))
}

fn mark_ref_matches(dataref: &[u8], mark: u64) -> bool {
    let Some(raw) = dataref.strip_prefix(b":") else {
        return false;
    };
    std::str::from_utf8(raw)
        .ok()
        .and_then(|text| text.parse::<u64>().ok())
        == Some(mark)
}

/// Seed the tree map from an existing commit's tree (recursively flattened to
/// full paths). Only blobs and nested trees are represented; the minimal subset
/// only ever modifies top-level blobs, but recursion keeps `from` correct when a
/// parent tree has subdirectories.
fn seed_tree_from_commit(
    db: &FastImportDatabase,
    format: ObjectFormat,
    commit_oid: &ObjectId,
    tree: &mut BTreeMap<Vec<u8>, TreeEntry>,
) -> Result<ObjectId> {
    if let Some(recent) = db.recent_commit_tree(commit_oid) {
        tree.clone_from(recent.entries.as_ref());
        return Ok(recent.oid);
    }
    let object = db.read_object(commit_oid)?;
    if object.object_type != ObjectType::Commit {
        return Err(GitError::Command(format!(
            "fast-import: {commit_oid} is not a commit"
        )));
    }
    let commit = Commit::parse(format, &object.body)?;
    seed_tree_entries(db, format, &commit.tree, &[], tree)?;
    Ok(commit.tree)
}

fn seed_tree_entries(
    db: &FastImportDatabase,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    prefix: &[u8],
    tree: &mut BTreeMap<Vec<u8>, TreeEntry>,
) -> Result<()> {
    let object = db.read_object(tree_oid)?;
    if object.object_type != ObjectType::Tree {
        return Err(GitError::Command(format!(
            "fast-import: {tree_oid} is not a tree"
        )));
    }
    let parsed = Tree::parse(format, &object.body)?;
    for entry in parsed.entries {
        let mut full = prefix.to_vec();
        if !full.is_empty() {
            full.push(b'/');
        }
        full.extend_from_slice(entry.name.as_bytes());
        if entry.mode == 0o040000 {
            let sub_oid = entry.oid;
            seed_tree_entries(db, format, &sub_oid, &full, tree)?;
        } else {
            tree.insert(
                full.clone(),
                TreeEntry {
                    mode: entry.mode,
                    name: BString::from(full),
                    oid: entry.oid,
                },
            );
        }
    }
    Ok(())
}

/// Build (and write) the nested tree objects from a flat path→entry map, then
/// return the root tree oid.
fn write_tree_from_map(
    db: &mut FastImportDatabase,
    pack_state: &mut FastImportPackState,
    entries: &BTreeMap<Vec<u8>, TreeEntry>,
) -> Result<ObjectId> {
    // Group by first path component to construct subtrees recursively.
    write_tree_level(db, pack_state, entries, &[])
}

fn write_tree_level(
    db: &mut FastImportDatabase,
    pack_state: &mut FastImportPackState,
    entries: &BTreeMap<Vec<u8>, TreeEntry>,
    prefix: &[u8],
) -> Result<ObjectId> {
    // Collect direct children (blobs) and subdirectory names under `prefix`.
    let mut tree_entries: Vec<TreeEntry> = Vec::new();
    let mut subdirs: BTreeSet<Vec<u8>> = BTreeSet::new();

    let prefix_len = if prefix.is_empty() {
        0
    } else {
        prefix.len() + 1
    };

    for (path, entry) in entries {
        if !prefix.is_empty()
            && (!path.starts_with(prefix) || path.get(prefix.len()) != Some(&b'/'))
        {
            continue;
        }
        let rel = &path[prefix_len..];
        if let Some(slash) = rel.iter().position(|b| *b == b'/') {
            subdirs.insert(rel[..slash].to_vec());
        } else {
            tree_entries.push(TreeEntry {
                mode: entry.mode,
                name: BString::from(rel.to_vec()),
                oid: entry.oid,
            });
        }
    }

    for dir in subdirs {
        let mut sub_prefix = prefix.to_vec();
        if !sub_prefix.is_empty() {
            sub_prefix.push(b'/');
        }
        sub_prefix.extend_from_slice(&dir);
        let sub_oid = write_tree_level(db, pack_state, entries, &sub_prefix)?;
        tree_entries.push(TreeEntry {
            mode: 0o040000,
            name: BString::from(dir),
            oid: sub_oid,
        });
    }

    // Git tree entries are sorted by name, with a subtree collating as though its
    // name ended in `/` (so `foo` < `foo/` < `foo0`). `Tree::write` emits entries
    // verbatim, so we must impose that ordering here.
    tree_entries.sort_by_key(tree_sort_key);

    let tree = Tree {
        entries: tree_entries,
    };
    write_fast_import_object(
        db,
        pack_state,
        EncodedObject::new(ObjectType::Tree, tree.write()),
    )
}

/// The git tree-ordering collation key: a directory's name sorts as if it had a
/// trailing `/`.
fn tree_sort_key(entry: &TreeEntry) -> Vec<u8> {
    let mut key = entry.name.as_bytes().to_vec();
    if entry.mode == 0o040000 {
        key.push(b'/');
    }
    key
}

/// Persist the final value of every branch changed since the last checkpoint.
///
/// fast-import keeps branch tips in memory and writes each ref once at a
/// checkpoint/end-of-stream. Besides avoiding thousands of lock/rename cycles,
/// this is the durability boundary Git exposes: a malformed stream before its
/// first checkpoint may leave unreferenced objects, but must not publish a
/// partially imported branch.
fn flush_fast_import_refs(
    store: &FileRefStore,
    git_dir: &Path,
    db: &FastImportDatabase,
    format: ObjectFormat,
    force: bool,
    ref_states: &HashMap<String, FastImportRefState>,
    dirty_refs: &mut HashSet<String>,
) -> Result<bool> {
    let mut names = dirty_refs.drain().collect::<Vec<_>>();
    names.sort();
    let mut rejected = false;
    for ref_name in names {
        let Some(state) = ref_states.get(&ref_name).copied() else {
            continue;
        };
        match state {
            FastImportRefState::Empty => delete_ref_if_exists(store, &ref_name)?,
            FastImportRefState::Tip(new_oid) => {
                let old_oid = match store.read_ref(&ref_name)? {
                    Some(RefTarget::Direct(oid)) => oid,
                    _ => zero_oid(format)?,
                };
                if !force
                    && ref_name.starts_with("refs/heads/")
                    && old_oid != zero_oid(format)?
                    && !sley_rev::is_ancestor(git_dir, format, db, &old_oid, &new_oid)?
                {
                    eprintln!(
                        "warning: not updating {ref_name} (new tip {new_oid} does not contain {old_oid})"
                    );
                    rejected = true;
                    continue;
                }
                update_ref_direct(store, git_dir, format, &ref_name, new_oid)?;
            }
        }
    }
    Ok(rejected)
}

/// Update a ref to the supplied oid. A reflog entry is written (git's
/// fast-import appends one), so HEAD~N navigation and reflog reads both work
/// after a bulk import.
fn update_ref_direct(
    store: &FileRefStore,
    git_dir: &Path,
    format: ObjectFormat,
    ref_name: &str,
    new_oid: ObjectId,
) -> Result<()> {
    if let Some(replaced) = ref_name.strip_prefix("refs/replace/")
        && replaced == new_oid.to_hex()
    {
        eprintln!(
            "warning: dropping {ref_name} since it would point to itself (i.e. to {new_oid})"
        );
        delete_ref_if_exists(store, ref_name)?;
        return Ok(());
    }
    let old_oid = match store.read_ref(ref_name)? {
        Some(RefTarget::Direct(oid)) => oid,
        _ => zero_oid(format)?,
    };
    let reflog = fast_import_should_write_reflog(git_dir, ref_name)?.then(|| ReflogEntry {
        old_oid,
        new_oid,
        committer: default_committer(),
        message: b"fast-import".to_vec(),
    });
    let mut tx = store.transaction();
    tx.update(RefUpdate {
        name: ref_name.to_string(),
        expected: None,
        new: RefTarget::Direct(new_oid),
        reflog,
    });
    tx.commit()
}

fn fast_import_should_write_reflog(git_dir: &Path, name: &str) -> Result<bool> {
    if reflog_path_for_ref_name(git_dir, name)?.exists() {
        return Ok(true);
    }
    if let Some(value) = global_config_value("core.logAllRefUpdates")? {
        return Ok(fast_import_log_all_ref_updates_matches(name, &value));
    }
    let common_git_dir = common_git_dir_for_git_dir(git_dir)?;
    let Ok(config) = GitConfig::read(common_git_dir.join("config")) else {
        return Ok(false);
    };
    if let Some(value) = config.get("core", None, "logAllRefUpdates") {
        return Ok(fast_import_log_all_ref_updates_matches(name, value));
    }
    if config.get_bool("core", None, "bare").unwrap_or(false) {
        return Ok(false);
    }
    Ok(fast_import_log_all_ref_updates_matches(name, "true"))
}

fn reflog_path_for_ref_name(git_dir: &Path, name: &str) -> Result<PathBuf> {
    let common_git_dir = common_git_dir_for_git_dir(git_dir)?;
    let base = if name == "HEAD" || name.starts_with("refs/bisect/") {
        git_dir
    } else {
        &common_git_dir
    };
    Ok(base.join("logs").join(name))
}

fn fast_import_log_all_ref_updates_matches(name: &str, value: &str) -> bool {
    if value.eq_ignore_ascii_case("always") {
        return true;
    }
    if !sley_config::parse_config_bool(value).unwrap_or(false) {
        return false;
    }
    name == "HEAD"
        || name.starts_with("refs/heads/")
        || name.starts_with("refs/remotes/")
        || name.starts_with("refs/notes/")
}

// ---------------------------------------------------------------------------
// Stream tokenizer
// ---------------------------------------------------------------------------

/// A line-oriented cursor over the fast-import stream that also knows how to read
/// `data` payloads (both `data <<DELIM` and `data <n>` forms).
struct StreamParser<R: BufRead> {
    reader: R,
    peeked: Option<Vec<u8>>,
    skipped_blank_lines: usize,
}

impl<R: BufRead> StreamParser<R> {
    fn new(reader: R) -> Self {
        Self {
            reader,
            peeked: None,
            skipped_blank_lines: 0,
        }
    }

    /// Read the next newline-terminated line (the trailing `\n` is stripped),
    /// advancing the cursor. Returns `None` at end of input. A blank line is
    /// returned as an empty slice (callers use it as a record separator).
    fn raw_line(&mut self) -> Result<Option<Vec<u8>>> {
        let mut line = Vec::new();
        let bytes = self.reader.read_until(b'\n', &mut line)?;
        if bytes == 0 {
            return Ok(None);
        }
        if line.last() == Some(&b'\n') {
            line.pop();
        }
        Ok(Some(line))
    }

    /// The next command line, skipping leading blank separator lines. Returns the
    /// line bytes (without trailing newline) or `None` at end of stream.
    fn next_command_line(&mut self) -> Result<Option<Vec<u8>>> {
        if let Some(line) = self.peeked.take() {
            return Ok(Some(line));
        }
        let mut skipped = 0usize;
        loop {
            let line = self.raw_line()?;
            match line {
                Some(line) if line.is_empty() => {
                    skipped += 1;
                    continue;
                }
                Some(line) => {
                    self.skipped_blank_lines = skipped;
                    return Ok(Some(line));
                }
                None => {
                    self.skipped_blank_lines = skipped;
                    return Ok(None);
                }
            }
        }
    }

    fn next_non_comment_command_line(&mut self) -> Result<Option<Vec<u8>>> {
        loop {
            let Some(line) = self.next_command_line()? else {
                return Ok(None);
            };
            if line.starts_with(b"#") {
                continue;
            }
            return Ok(Some(line));
        }
    }

    /// Peek the next command line without consuming it.
    fn peek_command_line(&mut self) -> Result<Option<&[u8]>> {
        if self.peeked.is_none() {
            let mut skipped = 0usize;
            loop {
                let Some(line) = self.raw_line()? else {
                    self.skipped_blank_lines = skipped;
                    return Ok(None);
                };
                if line.is_empty() {
                    skipped += 1;
                    continue;
                }
                self.skipped_blank_lines = skipped;
                self.peeked = Some(line);
                break;
            }
        }
        Ok(self.peeked.as_deref())
    }

    /// Read a `data` payload given its header line (`data <<DELIM` or `data N`).
    fn read_data(&mut self, header: &[u8]) -> Result<Vec<u8>> {
        let arg = line_after(header, b"data")
            .map(trim_ascii)
            .ok_or_else(|| GitError::Command("fast-import: malformed data header".into()))?;
        if let Some(delim) = arg.strip_prefix(b"<<") {
            let delim = trim_ascii(delim).to_vec();
            self.read_delimited_data(&delim)
        } else {
            let count_text = std::str::from_utf8(arg)
                .map_err(|_| GitError::InvalidFormat("fast-import: data count not utf8".into()))?;
            let count: usize = count_text.parse().map_err(|_| {
                GitError::Command(format!("fast-import: bad data count '{count_text}'"))
            })?;
            self.read_counted_data(count)
        }
    }

    /// `data <<DELIM`: accumulate raw lines until one equals DELIM exactly. The
    /// terminator line and the trailing newline of the final payload line are
    /// consumed; git's heredoc form drops the newline before DELIM.
    fn read_delimited_data(&mut self, delim: &[u8]) -> Result<Vec<u8>> {
        let mut out: Vec<u8> = Vec::new();
        loop {
            let Some(line) = self.raw_line()? else {
                return Err(GitError::Command(
                    "fast-import: data terminator not found".into(),
                ));
            };
            if line == delim {
                break;
            }
            out.extend_from_slice(&line);
            out.push(b'\n');
        }
        Ok(out)
    }

    /// `data N`: read exactly N bytes, then skip an optional trailing newline.
    fn read_counted_data(&mut self, count: usize) -> Result<Vec<u8>> {
        let mut data = vec![0; count];
        self.reader.read_exact(&mut data).map_err(|_| {
            GitError::Command("fast-import: data count exceeds stream length".into())
        })?;
        // git allows an optional LF directly after a counted data block.
        let buffered = self.reader.fill_buf()?;
        if buffered.first() == Some(&b'\n') {
            self.reader.consume(1);
        }
        Ok(data)
    }
}

// ---------------------------------------------------------------------------
// Small byte helpers
// ---------------------------------------------------------------------------

/// If `line` starts with `prefix`, return the remainder, else `None`. For a
/// bare-keyword prefix like `b"data"`, the remainder may begin with a space.
fn line_after<'a>(line: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    line.strip_prefix(prefix)
}

/// Split off the first whitespace-delimited field, returning `(field, rest)`
/// where `rest` has leading spaces removed.
fn split_field(bytes: &[u8]) -> (&[u8], &[u8]) {
    let bytes = trim_leading_space(bytes);
    match bytes.iter().position(|b| *b == b' ') {
        Some(idx) => (&bytes[..idx], trim_leading_space(&bytes[idx + 1..])),
        None => (bytes, &[]),
    }
}

fn split_field_preserve_rest(bytes: &[u8]) -> (&[u8], &[u8]) {
    let bytes = trim_leading_space(bytes);
    match bytes.iter().position(|b| *b == b' ') {
        Some(idx) => (&bytes[..idx], &bytes[idx + 1..]),
        None => (bytes, &[]),
    }
}

fn trim_leading_space(bytes: &[u8]) -> &[u8] {
    let mut start = 0;
    while start < bytes.len() && bytes[start] == b' ' {
        start += 1;
    }
    &bytes[start..]
}

fn trim_ascii(bytes: &[u8]) -> &[u8] {
    let mut start = 0;
    let mut end = bytes.len();
    while start < end && bytes[start].is_ascii_whitespace() {
        start += 1;
    }
    while end > start && bytes[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    &bytes[start..end]
}

/// Parse and normalize an `M`-line file mode, mirroring git fast-import's
/// `parse_mode` (builtin/fast-import.c): the short forms `644`/`755` expand to
/// the canonical `100644`/`100755`; `120000` (symlink), `160000` (gitlink), and
/// `040000` (subtree) are accepted as-is. Anything else is rejected.
fn parse_filemode(text: &str) -> Result<u32> {
    let raw = u32::from_str_radix(text, 8)
        .map_err(|_| GitError::Command(format!("fast-import: bad mode '{text}'")))?;
    let mode = match raw {
        0o644 => 0o100644,
        0o755 => 0o100755,
        0o100644 | 0o100755 | 0o120000 | 0o160000 | 0o040000 => raw,
        _ => {
            return Err(GitError::Command(format!(
                "fast-import: unsupported mode '{text}'"
            )));
        }
    };
    Ok(mode)
}

/// Parse a `mark :N` numeric id from the bytes after the `:`.
fn parse_mark(rest: &[u8]) -> Result<u64> {
    let text = std::str::from_utf8(trim_ascii(rest))
        .map_err(|_| GitError::InvalidFormat("fast-import: mark not utf8".into()))?;
    text.parse()
        .map_err(|_| GitError::Command(format!("fast-import: bad mark ':{text}'")))
}

fn parse_path_at_eol(bytes: &[u8], allow_empty: bool, field: &str) -> Result<Vec<u8>> {
    if bytes.first() == Some(&b'"') {
        let mut out = Vec::new();
        let consumed = crate::commands::ref_command_stream::unquote_c_style(bytes, &mut out)
            .ok_or_else(|| GitError::Command(format!("fast-import: invalid {field}")))?;
        if out.contains(&0) {
            return Err(GitError::Command(format!("fast-import: NUL in {field}")));
        }
        if consumed != bytes.len() {
            return Err(GitError::Command(format!(
                "fast-import: garbage after {field}"
            )));
        }
        if out.is_empty() && !allow_empty {
            return Err(GitError::Command(format!("fast-import: empty {field}")));
        }
        return Ok(out);
    }
    let path = bytes.to_vec();
    if path.is_empty() && !allow_empty {
        return Err(GitError::Command(format!("fast-import: empty {field}")));
    }
    Ok(path)
}

fn parse_path_before_space<'a>(bytes: &'a [u8], field: &str) -> Result<(Vec<u8>, &'a [u8])> {
    if bytes.first() == Some(&b'"') {
        let mut out = Vec::new();
        let consumed = crate::commands::ref_command_stream::unquote_c_style(bytes, &mut out)
            .ok_or_else(|| GitError::Command(format!("fast-import: invalid {field}")))?;
        if out.contains(&0) {
            return Err(GitError::Command(format!("fast-import: NUL in {field}")));
        }
        if bytes.get(consumed) != Some(&b' ') {
            return Err(GitError::Command(format!(
                "fast-import: missing space after {field}"
            )));
        }
        return Ok((out, &bytes[consumed + 1..]));
    }
    let Some(space) = bytes.iter().position(|byte| *byte == b' ') else {
        return Err(GitError::Command(format!(
            "fast-import: missing space after {field}"
        )));
    };
    Ok((bytes[..space].to_vec(), &bytes[space + 1..]))
}

#[derive(Clone, Copy)]
enum FastImportPathKind {
    Any,
    FileModify(u32),
}

fn validate_fast_import_path(path: &[u8], kind: FastImportPathKind) -> Result<()> {
    if path == b"." || path == b".." || path.starts_with(b"../") || path.ends_with(b"/..") {
        return Err(GitError::Command(
            "fast-import: invalid path component".into(),
        ));
    }
    if path.ends_with(b"/") {
        return Err(GitError::Command(
            "fast-import: path may not end with '/'".into(),
        ));
    }
    for component in path.split(|byte| *byte == b'/') {
        if component == b"." || component == b".." {
            return Err(GitError::Command(
                "fast-import: invalid path component".into(),
            ));
        }
        if component.eq_ignore_ascii_case(b".git") {
            return Err(GitError::Command("fast-import: invalid .git path".into()));
        }
    }
    if let FastImportPathKind::FileModify(mode) = kind
        && mode == 0o120000
        && path == b".gitmodules"
    {
        return Err(GitError::Command(
            "fast-import: invalid .gitmodules symlink".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn staged_overlay_has_object_and_body_size_flush_bounds() {
        assert!(!FastImportDatabase::staging_budget_exceeded(
            99_999,
            64 * 1024 * 1024 - 1
        ));
        assert!(FastImportDatabase::staging_budget_exceeded(100_000, 0));
        assert!(FastImportDatabase::staging_budget_exceeded(
            0,
            64 * 1024 * 1024
        ));
    }

    #[test]
    fn parse_filemode_normalizes_short_forms() {
        assert_eq!(parse_filemode("644").expect("644"), 0o100644);
        assert_eq!(parse_filemode("755").expect("755"), 0o100755);
        assert_eq!(parse_filemode("100644").expect("100644"), 0o100644);
        assert_eq!(parse_filemode("120000").expect("120000"), 0o120000);
        assert_eq!(parse_filemode("160000").expect("160000"), 0o160000);
        assert_eq!(parse_filemode("040000").expect("040000"), 0o040000);
        assert!(parse_filemode("600").is_err());
        assert!(parse_filemode("zz").is_err());
    }

    #[test]
    fn read_delimited_data_drops_terminator_and_keeps_trailing_newline() {
        // `data <<EOF` heredoc: every payload line, including the last, is
        // newline-terminated; the EOF marker line is consumed but not stored.
        let input = b"data <<EOF\ncommit 1\nEOF\nM 644 inline 1.t\n";
        let mut p = StreamParser::new(io::BufReader::new(&input[..]));
        let header = p.next_command_line().expect("read header").expect("header");
        let body = p.read_data(&header).expect("data");
        assert_eq!(body, b"commit 1\n");
        // The cursor is positioned at the line after the terminator.
        assert_eq!(
            p.next_command_line().expect("read next").expect("next"),
            b"M 644 inline 1.t"
        );
    }

    #[test]
    fn read_counted_data_reads_exact_bytes_and_optional_lf() {
        let input = b"data 5\nhelloM 644 inline x\n";
        let mut p = StreamParser::new(io::BufReader::new(&input[..]));
        let header = p.next_command_line().expect("read header").expect("header");
        let body = p.read_data(&header).expect("data");
        assert_eq!(body, b"hello");
        // No newline immediately followed "hello", so the next token starts at M.
        assert_eq!(
            p.next_command_line().expect("read next").expect("next"),
            b"M 644 inline x"
        );
    }

    #[test]
    fn next_command_line_skips_blank_separators() {
        let input = b"\n\ncommit HEAD\n\nfrom HEAD^0\n";
        let mut p = StreamParser::new(io::BufReader::new(&input[..]));
        assert_eq!(
            p.next_command_line().expect("read commit").expect("commit"),
            b"commit HEAD"
        );
        assert_eq!(
            p.next_command_line().expect("read from").expect("from"),
            b"from HEAD^0"
        );
        assert!(p.next_command_line().expect("read eof").is_none());
    }

    #[test]
    fn write_progress_echoes_the_full_progress_line() {
        let mut out = Vec::new();
        write_progress(&mut out, b"progress checkpoint").expect("write progress");
        assert_eq!(out, b"progress checkpoint\n");
    }

    #[test]
    fn split_field_partitions_on_first_space() {
        let (head, rest) = split_field(b"644 inline 1.t");
        assert_eq!(head, b"644");
        assert_eq!(rest, b"inline 1.t");
        let (head, rest) = split_field(b"inline 1.t");
        assert_eq!(head, b"inline");
        assert_eq!(rest, b"1.t");
    }

    #[test]
    fn parse_path_at_eol_preserves_unquoted_and_decodes_quoted() {
        assert_eq!(
            parse_path_at_eol(b"  1.t  ", false, "path").expect("path"),
            b"  1.t  "
        );
        assert_eq!(
            parse_path_at_eol(br#""qu\157ted""#, false, "path").expect("quoted"),
            b"quoted"
        );
        assert!(parse_path_at_eol(b"", false, "path").is_err());
        assert_eq!(parse_path_at_eol(b"", true, "path").expect("root"), b"");
    }
}
