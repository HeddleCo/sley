use sley_config::GitConfig;
use sley_core::{ObjectFormat, ObjectId};
use sley_object::{Commit, EncodedObject, ObjectType, Tag, TreeEntries};
use sley_odb::ObjectReader;
use std::collections::{HashMap, HashSet, VecDeque};

mod connectivity;
pub mod content;

pub use connectivity::{
    ConnectivityOptions, FsckFinding, FsckFindings, FsckRef, FsckRefTarget, FsckSeverity,
    check_connectivity, check_refs,
};
pub use content::SeverityConfig;

// Re-exported below: IssueSeverity, IssueStream, FsckIssue (declared here).

/// Whether an issue is a hard error (fails fsck, exit 1) or a warning (printed
/// but does not by itself fail the check).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueSeverity {
    Error,
    Warning,
}

/// Which stream an issue prints on, matching builtin/fsck.c. Connectivity
/// complaints (`broken link`, `missing`, type-mismatch) print on stdout
/// alongside `dangling`/`unreachable` notices; object-content findings
/// (`error in`/`warning in`) print on stderr.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueStream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsckIssue {
    pub message: String,
    pub severity: IssueSeverity,
    pub stream: IssueStream,
}

impl FsckIssue {
    /// A hard-error connectivity issue (broken link, missing object, type
    /// mismatch) — printed on stdout.
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            severity: IssueSeverity::Error,
            stream: IssueStream::Stdout,
        }
    }

    /// A hard-error content finding — printed on stderr.
    pub fn content_error(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            severity: IssueSeverity::Error,
            stream: IssueStream::Stderr,
        }
    }

    /// A warning content finding (does not fail fsck) — printed on stderr.
    pub fn content_warning(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            severity: IssueSeverity::Warning,
            stream: IssueStream::Stderr,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsckNotice {
    pub message: String,
}

/// git's fsck exit-code bits (builtin/fsck.c). The process exit status is the
/// OR of these.
pub const ERROR_OBJECT: i32 = 0o1; // a content/object problem
pub const ERROR_REACHABLE: i32 = 0o2; // a missing/broken reachability link
pub const ERROR_REFS: i32 = 0o10; // a ref points at an incomplete object

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FsckReport {
    pub notices: Vec<FsckNotice>,
    pub issues: Vec<FsckIssue>,
    /// Accumulated git exit-code bits (see `ERROR_*`).
    pub error_bits: i32,
}

impl FsckReport {
    /// True if no *error*-severity issue was found. Warning-severity issues do
    /// not fail fsck (git exits 0 when only warnings are present).
    pub fn is_ok(&self) -> bool {
        self.error_bits == 0
            && !self
                .issues
                .iter()
                .any(|i| i.severity == IssueSeverity::Error)
    }

    /// git's process exit status: the OR of all accumulated error bits, plus
    /// `ERROR_OBJECT` for any content-level error issue.
    pub fn exit_code(&self) -> i32 {
        let mut bits = self.error_bits;
        if self
            .issues
            .iter()
            .any(|i| i.severity == IssueSeverity::Error && i.stream == IssueStream::Stderr)
        {
            bits |= ERROR_OBJECT;
        }
        bits
    }
}

#[derive(Debug, Clone, Default)]
pub struct FsckOptions {
    pub report_dangling: bool,
    pub report_unreachable: bool,
    pub connectivity_only: bool,
    pub object_names: HashMap<ObjectId, String>,
    /// `fsck.<msgid>` severity overrides plus `--strict`, applied to
    /// object-content findings.
    pub severity: SeverityConfig,
}

#[derive(Debug, Clone)]
struct ObjectLink {
    object_type: ObjectType,
    oid: ObjectId,
}

pub fn fsck_objects<R, I, J>(
    reader: &R,
    format: ObjectFormat,
    roots: I,
    object_ids: J,
) -> FsckReport
where
    R: ObjectReader,
    I: IntoIterator<Item = ObjectId>,
    J: IntoIterator<Item = ObjectId>,
{
    fsck_objects_with_options(reader, format, roots, object_ids, FsckOptions::default())
}

pub fn fsck_objects_with_options<R, I, J>(
    reader: &R,
    format: ObjectFormat,
    roots: I,
    object_ids: J,
    options: FsckOptions,
) -> FsckReport
where
    R: ObjectReader,
    I: IntoIterator<Item = ObjectId>,
    J: IntoIterator<Item = ObjectId>,
{
    let mut checker = FsckChecker {
        reader,
        format,
        checked: HashSet::new(),
        issues: Vec::new(),
        severity: options.severity.clone(),
        connectivity_only: options.connectivity_only,
        object_names: options.object_names.clone(),
        error_bits: 0,
        gitmodules_found: HashSet::new(),
        gitattributes_found: HashSet::new(),
    };
    let roots = roots.into_iter().collect::<Vec<_>>();
    let object_ids = object_ids.into_iter().collect::<Vec<_>>();
    for oid in roots.iter().cloned() {
        checker.check_object_root(oid);
    }
    for oid in object_ids.iter().cloned() {
        if !checker.checked.contains(&oid) {
            checker.check_object_content_only(oid);
        }
    }
    // git's deferred `fsck_blobs` pass: every non-symlink entry named
    // `.gitmodules` or `.gitattributes` is type/content checked with the
    // path-specific security rules.
    checker.check_gitmodules_blobs();
    checker.check_gitattributes_blobs();
    let notices = if options.report_unreachable {
        unreachable_notices(reader, format, &roots, &object_ids)
    } else if options.report_dangling {
        dangling_notices(reader, format, &roots, &object_ids)
    } else {
        Vec::new()
    };
    FsckReport {
        notices,
        issues: checker.issues,
        error_bits: checker.error_bits,
    }
}

struct FsckChecker<'a, R> {
    reader: &'a R,
    format: ObjectFormat,
    checked: HashSet<ObjectId>,
    issues: Vec<FsckIssue>,
    severity: SeverityConfig,
    connectivity_only: bool,
    object_names: HashMap<ObjectId, String>,
    /// Accumulated git exit-code bits (`ERROR_REACHABLE`).
    error_bits: i32,
    /// Blob oids that some tree entry named `.gitmodules`.
    gitmodules_found: HashSet<ObjectId>,
    /// Blob oids that some tree entry named `.gitattributes`, mirroring git's
    /// `gitattributes_found` oidset. Content-checked in a deferred final pass
    /// (`fsck_blobs`) so the blob is validated as a gitattributes file even
    /// though it parses fine as a plain blob.
    gitattributes_found: HashSet<ObjectId>,
}

impl<R> FsckChecker<'_, R>
where
    R: ObjectReader,
{
    fn check_object_link(&mut self, source: Option<ObjectLink>, link: ObjectLink) {
        let object = match self.reader.read_object(&link.oid) {
            Ok(object) => object,
            Err(_) => {
                if self.reader.is_promised_object(&link.oid) {
                    return;
                }
                self.report_missing_link(source, link);
                return;
            }
        };
        if object.object_type != link.object_type {
            // git: "<oid>: object is a <actual>, not a <expected>" — an object
            // error (ERROR_OBJECT).
            self.error_bits |= ERROR_OBJECT;
            self.issues.push(FsckIssue::error(format!(
                "{} is a {}, not a {}",
                link.oid,
                object.object_type.as_str(),
                link.object_type.as_str()
            )));
        }
        self.check_loaded_object(link.oid, &object);
    }

    fn check_object(&mut self, oid: ObjectId) {
        let object = match self.reader.read_object(&oid) {
            Ok(object) => object,
            Err(err) => {
                self.issues
                    .push(FsckIssue::error(format!("missing object {oid}: {err}")));
                self.error_bits |= ERROR_REACHABLE;
                return;
            }
        };
        self.check_loaded_object(oid, &object);
    }

    fn check_object_content_only(&mut self, oid: ObjectId) {
        if !self.checked.insert(oid) {
            return;
        }
        let object = match self.reader.read_object(&oid) {
            Ok(object) => object,
            Err(err) => {
                self.issues
                    .push(FsckIssue::error(format!("missing object {oid}: {err}")));
                self.error_bits |= ERROR_REACHABLE;
                return;
            }
        };
        if self.check_loaded_object_content(oid, &object, false) {
            return;
        }
        // git's object-enumeration pass (`fsck_obj`) link-walks every commit and
        // tree it examines — even one reachable only through the enumeration —
        // and reports their missing tree/parent/entry targets (verified against
        // git 2.54: a blob referenced solely by a dangling commit's tree is
        // reported `missing blob`; a dangling commit with a missing tree/parent
        // is reported `broken link`/`missing`). The lone exception is a *tag*:
        // an unreachable tag reports only `dangling tag`, never a broken link to
        // its referent (see `unreachable_tag_referent_is_not_checked_as_a_broken_link`).
        // So walk commits/trees here but leave tags content-only.
        match object.object_type {
            ObjectType::Commit => self.check_commit(oid, &object.body),
            ObjectType::Tree => self.check_tree(oid, &object.body),
            ObjectType::Tag | ObjectType::Blob => {}
        }
    }

    /// Check a ref-reachable root. The driver validates the ref tip itself
    /// (`invalid sha1 pointer` / `not a commit`, and the ERROR_REACHABLE/REFS
    /// attribution) via git's `snapshot_ref` rules before handing us only
    /// readable tip oids, so the root walk is just an ordinary object check.
    fn check_object_root(&mut self, oid: ObjectId) {
        self.check_object(oid);
    }

    fn check_loaded_object(&mut self, oid: ObjectId, object: &EncodedObject) {
        if !self.checked.insert(oid) {
            return;
        }
        if self.check_loaded_object_content(oid, object, true) {
            return;
        }

        match object.object_type {
            ObjectType::Commit => self.check_commit(oid, &object.body),
            ObjectType::Tree => self.check_tree(oid, &object.body),
            ObjectType::Tag => self.check_tag(oid, &object.body),
            ObjectType::Blob => {}
        }
    }

    fn check_loaded_object_content(
        &mut self,
        oid: ObjectId,
        object: &EncodedObject,
        fail_nonfatal_errors: bool,
    ) -> bool {
        match object.object_id(self.format) {
            Ok(actual) if actual == oid => {}
            Ok(actual) => {
                if !self.connectivity_only {
                    self.error_bits |= ERROR_OBJECT;
                    self.issues.push(FsckIssue::error(format!(
                        "object id mismatch: expected {oid}, got {actual}"
                    )));
                    return true;
                }
            }
            Err(err) => {
                if !self.connectivity_only {
                    self.error_bits |= ERROR_OBJECT;
                    self.issues
                        .push(FsckIssue::error(format!("invalid object {oid}: {err}")));
                    return true;
                }
            }
        }

        // Run git's content checker (commit/tree/tag buffer validation). It
        // emits the exact `error in <type> <oid>: <msgid>: <detail>` /
        // `warning in ...` lines on stderr, with `fsck.<id>` severity applied.
        let content_findings = if self.connectivity_only {
            Vec::new()
        } else {
            content::check_object_content(object.object_type, &object.body, &self.severity)
        };
        let had_fatal = content_findings.iter().any(|f| f.fatal);
        for f in &content_findings {
            let prefix = match f.severity {
                content::Severity::Error => "error in",
                content::Severity::Warn => "warning in",
                content::Severity::Ignore => continue,
            };
            // git emits some raw `error: <msg>` stderr lines (e.g. tree-walk's
            // "empty filename in tree entry") *before* the formatted finding.
            if let Some(raw) = &f.raw_stderr {
                self.issues
                    .push(FsckIssue::content_error(format!("error: {raw}")));
            }
            let msg = format!(
                "{prefix} {} {oid}: {}: {}",
                object.object_type.as_str(),
                f.msg_id.camel(),
                f.detail,
            );
            let masked_tag_ident =
                object.object_type == ObjectType::Tag && is_tag_ident_msg(f.msg_id) && !f.fatal;
            let issue = if f.severity == content::Severity::Error
                && (fail_nonfatal_errors || !masked_tag_ident)
            {
                FsckIssue::content_error(msg)
            } else {
                FsckIssue::content_warning(msg)
            };
            self.issues.push(issue);
        }

        // If a structural (fatal) content problem stopped parsing, do not also
        // run the link walk — git aborts the object too.
        if had_fatal {
            return true;
        }
        false
    }

    fn check_commit(&mut self, oid: ObjectId, body: &[u8]) {
        // Content checks already ran; for the link walk we tolerate a strict
        // parse failure (the content checker reported the specifics).
        let Ok(commit) = Commit::parse_ref(self.format, body) else {
            if self.connectivity_only {
                self.error_bits |= ERROR_OBJECT;
                self.issues.push(FsckIssue::error(format!(
                    "{oid}: object corrupt or missing"
                )));
            }
            return;
        };
        let source_name = self.object_names.get(&oid).cloned();
        let source = ObjectLink {
            object_type: ObjectType::Commit,
            oid,
        };
        if let Some(name) = &source_name {
            self.object_names
                .entry(commit.tree)
                .or_insert_with(|| format!("{name}:"));
        }
        self.check_object_link(
            Some(source.clone()),
            ObjectLink {
                object_type: ObjectType::Tree,
                oid: commit.tree,
            },
        );
        for (idx, parent) in sley_odb::grafted_parents(self.reader, &oid, commit.parents)
            .into_iter()
            .enumerate()
        {
            if let Some(name) = &source_name {
                let suffix = if idx == 0 {
                    "^".to_string()
                } else {
                    format!("^{}", idx + 1)
                };
                self.object_names
                    .entry(parent)
                    .or_insert_with(|| format!("{name}{suffix}"));
            }
            self.check_object_link(
                Some(source.clone()),
                ObjectLink {
                    object_type: ObjectType::Commit,
                    oid: parent,
                },
            );
        }
    }

    fn check_tree(&mut self, oid: ObjectId, body: &[u8]) {
        let Ok(entries) =
            TreeEntries::new(self.format, body).collect::<std::result::Result<Vec<_>, _>>()
        else {
            // The content checker already reported `badTree`/`nullSha1`/etc.
            if self.connectivity_only {
                self.error_bits |= ERROR_OBJECT;
                self.issues.push(FsckIssue::error(format!(
                    "{oid}: object corrupt or missing"
                )));
            }
            return;
        };
        let source_name = self.object_names.get(&oid).cloned();
        let source = ObjectLink {
            object_type: ObjectType::Tree,
            oid,
        };
        for entry in entries {
            let entry_object_type = fsck_tree_entry_object_type(entry.mode);
            let is_symlink = entry.mode == 0o120000;
            self.check_tree_dotfile_entry(oid, entry.name, entry.oid, is_symlink);
            // A null-sha entry is reported by the content checker as a warning;
            // do not also walk it as a broken link (git skips null entries).
            if entry.oid.is_null() {
                continue;
            }
            // git's `fsck_walk_tree` skips gitlink (mode 160000) entries: a
            // submodule commit lives in the submodule's own object store, not the
            // superproject's, so it is never a broken link / missing object here.
            if entry.mode == 0o160000 {
                continue;
            }
            if let Some(name) = &source_name {
                let entry_name = String::from_utf8_lossy(entry.name);
                self.object_names
                    .entry(entry.oid)
                    .or_insert_with(|| format!("{name}{entry_name}"));
            }
            self.check_object_link(
                Some(source.clone()),
                ObjectLink {
                    object_type: entry_object_type,
                    oid: entry.oid,
                },
            );
        }
    }

    /// git's `fsck_tree` security check for the magic dotfiles, shared by the
    /// both the reachable and unreachable (`check_tree`)
    /// walks so the two can never drift. A symlinked `.gitmodules` /
    /// `.gitattributes` / `.gitignore` / `.mailmap` is an attack vector and must
    /// be rejected on *every* tree object regardless of reachability — git runs
    /// `fsck_tree` over the whole object database, not just ref-reachable trees.
    /// Centralising it here closes the regression where the dangling-object walk
    /// (added in "Preserve fsck dangling tag diagnostics") recorded only the
    /// non-symlink dotfile blobs and silently dropped the symlink rejection.
    fn check_tree_dotfile_entry(
        &mut self,
        oid: ObjectId,
        name: &[u8],
        entry_oid: ObjectId,
        is_symlink: bool,
    ) {
        if content::is_dotgitmodules_name(name) {
            if is_symlink {
                self.report_content(
                    ObjectType::Tree,
                    oid,
                    content::MsgId::GitmodulesSymlink,
                    ".gitmodules is a symbolic link",
                );
            } else {
                self.gitmodules_found.insert(entry_oid);
            }
        }
        if content::is_dotgitattributes_name(name) {
            if is_symlink {
                self.report_content(
                    ObjectType::Tree,
                    oid,
                    content::MsgId::GitattributesSymlink,
                    ".gitattributes is a symlink",
                );
            } else {
                self.gitattributes_found.insert(entry_oid);
            }
        }
        if is_symlink && content::is_dotgitignore_name(name) {
            self.report_content(
                ObjectType::Tree,
                oid,
                content::MsgId::GitignoreSymlink,
                ".gitignore is a symlink",
            );
        }
        if is_symlink && content::is_dotmailmap_name(name) {
            self.report_content(
                ObjectType::Tree,
                oid,
                content::MsgId::MailmapSymlink,
                ".mailmap is a symlink",
            );
        }
    }

    fn check_tag(&mut self, oid: ObjectId, body: &[u8]) {
        // Content checks already ran; tolerate a strict parse failure here.
        let Ok(tag) = Tag::parse_ref(self.format, body) else {
            if self.connectivity_only {
                self.error_bits |= ERROR_OBJECT;
                self.issues.push(FsckIssue::error(format!(
                    "{oid}: object corrupt or missing"
                )));
            }
            return;
        };
        if let Some(name) = self.object_names.get(&oid).cloned() {
            self.object_names.entry(tag.object).or_insert(name);
        }
        self.check_object_link(
            Some(ObjectLink {
                object_type: ObjectType::Tag,
                oid,
            }),
            ObjectLink {
                object_type: tag.object_type,
                oid: tag.object,
            },
        );
    }

    fn report_content(
        &mut self,
        object_type: ObjectType,
        oid: ObjectId,
        msg_id: content::MsgId,
        detail: impl Into<String>,
    ) {
        let severity = self.severity.resolve(msg_id);
        if severity == content::Severity::Ignore {
            return;
        }
        let prefix = match severity {
            content::Severity::Error => "error in",
            content::Severity::Warn => "warning in",
            content::Severity::Ignore => return,
        };
        let msg = format!(
            "{prefix} {} {oid}: {}: {}",
            object_type.as_str(),
            msg_id.camel(),
            detail.into()
        );
        let issue = match severity {
            content::Severity::Error => FsckIssue::content_error(msg),
            _ => FsckIssue::content_warning(msg),
        };
        self.issues.push(issue);
    }

    /// git's deferred `fsck_blobs` pass for `.gitmodules`.
    fn check_gitmodules_blobs(&mut self) {
        let mut oids: Vec<ObjectId> = self.gitmodules_found.iter().cloned().collect();
        oids.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
        for oid in oids {
            let Ok(object) = self.reader.read_object(&oid) else {
                self.report_content(
                    ObjectType::Blob,
                    oid,
                    content::MsgId::GitmodulesMissing,
                    "unable to read .gitmodules blob",
                );
                continue;
            };
            if object.object_type != ObjectType::Blob {
                self.report_content(
                    object.object_type,
                    oid,
                    content::MsgId::GitmodulesBlob,
                    "non-blob found at .gitmodules",
                );
                continue;
            }
            let Ok(config) = GitConfig::parse(&object.body) else {
                self.report_content(
                    ObjectType::Blob,
                    oid,
                    content::MsgId::GitmodulesParse,
                    "could not parse gitmodules blob",
                );
                continue;
            };
            for section in &config.sections {
                if section.name != "submodule" {
                    continue;
                }
                let Some(name) = section.subsection.as_deref() else {
                    continue;
                };
                if !sley_submodule::check_submodule_name(name) {
                    self.report_content(
                        ObjectType::Blob,
                        oid,
                        content::MsgId::GitmodulesName,
                        format!("disallowed submodule name: {name}"),
                    );
                }
                for entry in &section.entries {
                    let key = entry.key.to_ascii_lowercase();
                    let Some(value) = entry.value.as_deref() else {
                        continue;
                    };
                    match key.as_str() {
                        "url" if !sley_submodule::check_submodule_url(value) => {
                            self.report_content(
                                ObjectType::Blob,
                                oid,
                                content::MsgId::GitmodulesUrl,
                                format!("disallowed submodule url: {value}"),
                            );
                        }
                        "path" if sley_submodule::looks_like_command_line_option(value) => {
                            self.report_content(
                                ObjectType::Blob,
                                oid,
                                content::MsgId::GitmodulesPath,
                                format!("disallowed submodule path: {value}"),
                            );
                        }
                        "update"
                            if sley_submodule::parse_update_type(value)
                                == sley_submodule::UpdateType::Command =>
                        {
                            self.report_content(
                                ObjectType::Blob,
                                oid,
                                content::MsgId::GitmodulesUpdate,
                                format!("disallowed submodule update setting: {value}"),
                            );
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    /// git's deferred `fsck_blobs` pass for `.gitattributes`: each enrolled blob
    /// is loaded and content-checked for the line-length / size limits it would
    /// not get as a plain blob. Findings render as `error in blob <oid>: ...` on
    /// stderr; a content error sets ERROR_OBJECT (via the issue's stream).
    fn check_gitattributes_blobs(&mut self) {
        // Iterate in a stable order so output is deterministic.
        let mut oids: Vec<ObjectId> = self.gitattributes_found.iter().cloned().collect();
        oids.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
        for oid in oids {
            let Ok(object) = self.reader.read_object(&oid) else {
                self.report_content(
                    ObjectType::Blob,
                    oid,
                    content::MsgId::GitattributesMissing,
                    "unable to read .gitattributes blob",
                );
                continue;
            };
            if object.object_type != ObjectType::Blob {
                self.report_content(
                    object.object_type,
                    oid,
                    content::MsgId::GitattributesBlob,
                    "non-blob found at .gitattributes",
                );
                continue;
            }
            let findings = content::check_gitattributes_blob(&object.body, &self.severity);
            for f in &findings {
                if f.severity == content::Severity::Ignore {
                    continue;
                }
                let prefix = match f.severity {
                    content::Severity::Error => "error in",
                    _ => "warning in",
                };
                let msg = format!("{prefix} blob {oid}: {}: {}", f.msg_id.camel(), f.detail);
                let issue = match f.severity {
                    content::Severity::Error => FsckIssue::content_error(msg),
                    _ => FsckIssue::content_warning(msg),
                };
                self.issues.push(issue);
            }
        }
    }

    fn report_missing_link(&mut self, source: Option<ObjectLink>, link: ObjectLink) {
        // A broken reachability link sets only ERROR_REACHABLE. git's
        // ERROR_REFS is reserved for `snapshot_ref`'s branch→non-commit check
        // (handled in the driver), NOT for broken links reached through a ref
        // tip's closure — so a tag pointing at a missing blob, or a ref tip
        // whose subtree is missing, exits 2 (REACHABLE), not 10.
        self.error_bits |= ERROR_REACHABLE;
        if let Some(source) = source {
            // git: `printf_ln("broken link from %7s %s\n              to %7s %s")`
            // — the object type is right-aligned in a 7-char field.
            self.issues.push(FsckIssue::error(format!(
                "broken link from {:>7} {}\n              to {:>7} {}",
                source.object_type.as_str(),
                self.describe_oid(&source.oid),
                link.object_type.as_str(),
                self.describe_oid(&link.oid)
            )));
        }
        self.issues.push(FsckIssue::error(format!(
            "missing {} {}",
            link.object_type.as_str(),
            self.describe_oid(&link.oid)
        )));
    }

    fn describe_oid(&self, oid: &ObjectId) -> String {
        match self.object_names.get(oid) {
            Some(name) => format!("{oid} ({name})"),
            None => oid.to_string(),
        }
    }
}

fn is_tag_ident_msg(msg_id: content::MsgId) -> bool {
    matches!(
        msg_id,
        content::MsgId::MissingNameBeforeEmail
            | content::MsgId::MissingEmail
            | content::MsgId::BadName
            | content::MsgId::MissingSpaceBeforeEmail
            | content::MsgId::BadEmail
            | content::MsgId::MissingSpaceBeforeDate
            | content::MsgId::BadDate
            | content::MsgId::ZeroPaddedDate
            | content::MsgId::BadDateOverflow
            | content::MsgId::BadTimezone
    )
}

fn reachable_objects<R>(reader: &R, format: ObjectFormat, roots: &[ObjectId]) -> HashSet<ObjectId>
where
    R: ObjectReader,
{
    let mut reachable = HashSet::new();
    let mut pending = VecDeque::new();
    pending.extend(roots.iter().cloned());
    while let Some(oid) = pending.pop_front() {
        if !reachable.insert(oid) {
            continue;
        }
        let Ok(object) = reader.read_object(&oid) else {
            continue;
        };
        for link in object_links_grafted(reader, format, &oid, &object) {
            pending.push_back(link.oid);
        }
    }
    reachable
}

fn unreachable_objects<R>(
    reader: &R,
    format: ObjectFormat,
    roots: &[ObjectId],
    object_ids: &[ObjectId],
) -> Vec<(ObjectId, ObjectType, Vec<ObjectLink>)>
where
    R: ObjectReader,
{
    let reachable = reachable_objects(reader, format, roots);
    let mut unreachable = Vec::new();
    for oid in object_ids {
        if reachable.contains(oid) {
            continue;
        }
        let Ok(object) = reader.read_object(oid) else {
            continue;
        };
        unreachable.push((
            *oid,
            object.object_type,
            object_links_grafted(reader, format, oid, &object),
        ));
    }
    unreachable
}

fn unreachable_notices<R>(
    reader: &R,
    format: ObjectFormat,
    roots: &[ObjectId],
    object_ids: &[ObjectId],
) -> Vec<FsckNotice>
where
    R: ObjectReader,
{
    unreachable_objects(reader, format, roots, object_ids)
        .into_iter()
        .map(|(oid, object_type, _)| FsckNotice {
            message: format!("unreachable {} {}", object_type.as_str(), oid),
        })
        .collect()
}

fn dangling_notices<R>(
    reader: &R,
    format: ObjectFormat,
    roots: &[ObjectId],
    object_ids: &[ObjectId],
) -> Vec<FsckNotice>
where
    R: ObjectReader,
{
    let unreachable = unreachable_objects(reader, format, roots, object_ids);
    let unreachable_ids = unreachable
        .iter()
        .map(|(oid, _, _)| oid)
        .collect::<HashSet<_>>();
    let referenced_by_unreachable = unreachable
        .iter()
        .flat_map(|(_, _, links)| links.iter())
        .filter(|link| unreachable_ids.contains(&link.oid))
        .map(|link| link.oid)
        .collect::<HashSet<_>>();
    unreachable
        .into_iter()
        .filter(|(oid, _, _)| !referenced_by_unreachable.contains(oid))
        .map(|(oid, object_type, _)| FsckNotice {
            message: format!("dangling {} {}", object_type.as_str(), oid),
        })
        .collect()
}

/// [`object_links`] with the graft seam applied: parent links of a shallow
/// boundary commit are dropped, matching git's graft-aware `parse_commit`.
fn object_links_grafted<R: ObjectReader>(
    reader: &R,
    format: ObjectFormat,
    oid: &ObjectId,
    object: &EncodedObject,
) -> Vec<ObjectLink> {
    let mut links = object_links(format, object);
    if object.object_type == ObjectType::Commit && reader.is_shallow_graft(oid) {
        links.retain(|link| link.object_type != ObjectType::Commit);
    }
    links
}

fn object_links(format: ObjectFormat, object: &EncodedObject) -> Vec<ObjectLink> {
    match object.object_type {
        ObjectType::Commit => Commit::parse_ref(format, &object.body)
            .map(|commit| {
                let mut links = Vec::with_capacity(commit.parents.len() + 1);
                links.push(ObjectLink {
                    object_type: ObjectType::Tree,
                    oid: commit.tree,
                });
                links.extend(commit.parents.into_iter().map(|parent| ObjectLink {
                    object_type: ObjectType::Commit,
                    oid: parent,
                }));
                links
            })
            .unwrap_or_default(),
        ObjectType::Tree => TreeEntries::new(format, &object.body)
            .map(|entry| {
                entry.map(|entry| ObjectLink {
                    object_type: fsck_tree_entry_object_type(entry.mode),
                    oid: entry.oid,
                })
            })
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap_or_default(),
        ObjectType::Tag => Tag::parse_ref(format, &object.body)
            .map(|tag| {
                vec![ObjectLink {
                    object_type: tag.object_type,
                    oid: tag.object,
                }]
            })
            .unwrap_or_default(),
        ObjectType::Blob => Vec::new(),
    }
}

fn fsck_tree_entry_object_type(mode: u32) -> ObjectType {
    match mode {
        0o040000 => ObjectType::Tree,
        0o160000 => ObjectType::Commit,
        _ => ObjectType::Blob,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sley_core::BString;
    use sley_object::{Commit, EncodedObject, ObjectType, Tree, TreeEntry};
    use sley_odb::{ObjectDatabase, ObjectWriter};

    #[test]
    fn fsck_accepts_connected_commit_graph() {
        let format = ObjectFormat::Sha1;
        let mut db = ObjectDatabase::new(format);
        let blob = db
            .write_object(EncodedObject::new(ObjectType::Blob, b"payload\n".to_vec()))
            .expect("test operation should succeed");
        let tree = db
            .write_object(EncodedObject::new(
                ObjectType::Tree,
                Tree {
                    entries: vec![TreeEntry {
                        mode: 0o100644,
                        name: BString::from(b"payload.txt"),
                        oid: blob,
                    }],
                }
                .write(),
            ))
            .expect("test operation should succeed");
        let commit = db
            .write_object(EncodedObject::new(
                ObjectType::Commit,
                Commit {
                    tree,
                    parents: Vec::new(),
                    author: b"A <a@example.invalid> 0 +0000".to_vec(),
                    committer: b"A <a@example.invalid> 0 +0000".to_vec(),
                    encoding: None,
                    message: b"ok\n".to_vec(),
                }
                .write(),
            ))
            .expect("test operation should succeed");

        let report = fsck_objects(&db, format, [commit.clone()], [commit]);
        assert!(report.is_ok(), "{report:?}");
    }

    #[test]
    fn fsck_reports_missing_tree_link() {
        let format = ObjectFormat::Sha1;
        let mut db = ObjectDatabase::new(format);
        let missing_tree = ObjectId::from_hex(format, "1111111111111111111111111111111111111111")
            .expect("test operation should succeed");
        let commit = db
            .write_object(EncodedObject::new(
                ObjectType::Commit,
                Commit {
                    tree: missing_tree.clone(),
                    parents: Vec::new(),
                    author: b"A <a@example.invalid> 0 +0000".to_vec(),
                    committer: b"A <a@example.invalid> 0 +0000".to_vec(),
                    encoding: None,
                    message: b"bad\n".to_vec(),
                }
                .write(),
            ))
            .expect("test operation should succeed");

        let report = fsck_objects(&db, format, [commit.clone()], [commit]);
        assert_eq!(report.issues.len(), 2);
        assert!(
            report.issues[0]
                .message
                .contains("broken link from  commit")
        );
        assert_eq!(
            report.issues[1].message,
            format!("missing tree {missing_tree}")
        );
    }

    #[test]
    fn fsck_reports_dangling_tips_without_failing() {
        let format = ObjectFormat::Sha1;
        let mut db = ObjectDatabase::new(format);
        let blob = db
            .write_object(EncodedObject::new(ObjectType::Blob, b"lost\n".to_vec()))
            .expect("test operation should succeed");

        let report = fsck_objects_with_options(
            &db,
            format,
            [],
            [blob.clone()],
            FsckOptions {
                report_dangling: true,
                report_unreachable: false,
                ..Default::default()
            },
        );

        assert!(report.is_ok(), "{report:?}");
        assert_eq!(
            report.notices,
            vec![FsckNotice {
                message: format!("dangling blob {blob}")
            }]
        );
    }

    #[test]
    fn unreachable_tag_referent_is_not_checked_as_a_broken_link() {
        let format = ObjectFormat::Sha1;
        let mut db = ObjectDatabase::new(format);
        let missing_tag = ObjectId::from_hex(format, "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef")
            .expect("test operation should succeed");
        let tag = db
            .write_object(EncodedObject::new(
                ObjectType::Tag,
                format!(
                    "object {missing_tag}\n\
type tag\n\
tag valid\n\
tagger T A Gger <tagger@example.com> 1234567890 +0000\n\n"
                )
                .into_bytes(),
            ))
            .expect("test operation should succeed");

        let unreachable = fsck_objects_with_options(
            &db,
            format,
            [],
            [tag],
            FsckOptions {
                report_dangling: true,
                report_unreachable: false,
                ..Default::default()
            },
        );
        assert!(unreachable.is_ok(), "{unreachable:?}");
        assert_eq!(
            unreachable.notices,
            vec![FsckNotice {
                message: format!("dangling tag {tag}")
            }]
        );

        let reachable = fsck_objects_with_options(
            &db,
            format,
            [tag],
            [tag],
            FsckOptions {
                report_dangling: true,
                report_unreachable: false,
                ..Default::default()
            },
        );
        assert!(!reachable.is_ok(), "{reachable:?}");
        assert!(
            reachable
                .issues
                .iter()
                .any(|issue| issue.message == format!("missing tag {missing_tag}")),
            "{reachable:?}"
        );
    }

    #[test]
    fn unreachable_nonfatal_tag_content_error_does_not_fail_fsck() {
        let format = ObjectFormat::Sha1;
        let mut db = ObjectDatabase::new(format);
        let target = db
            .write_object(EncodedObject::new(ObjectType::Blob, b"x".to_vec()))
            .expect("test operation should succeed");
        let tag = db
            .write_object(EncodedObject::new(
                ObjectType::Tag,
                format!(
                    "object {target}\n\
type blob\n\
tag valid\n\
tagger T A Gger <\n\
 > 0 +0000\n\n"
                )
                .into_bytes(),
            ))
            .expect("test operation should succeed");

        let report = fsck_objects_with_options(
            &db,
            format,
            [],
            [tag],
            FsckOptions {
                report_dangling: true,
                report_unreachable: false,
                ..Default::default()
            },
        );

        assert!(report.is_ok(), "{report:?}");
        assert_eq!(report.exit_code(), 0);
        assert!(
            report
                .issues
                .iter()
                .any(|issue| issue.message.contains("badEmail:")
                    && issue.severity == IssueSeverity::Warning),
            "{report:?}"
        );
    }

    #[test]
    fn fsck_unreachable_reports_all_unreachable_objects() {
        let format = ObjectFormat::Sha1;
        let mut db = ObjectDatabase::new(format);
        let blob = db
            .write_object(EncodedObject::new(ObjectType::Blob, b"lost\n".to_vec()))
            .expect("test operation should succeed");
        let tree = db
            .write_object(EncodedObject::new(
                ObjectType::Tree,
                Tree {
                    entries: vec![TreeEntry {
                        mode: 0o100644,
                        name: BString::from(b"lost.txt"),
                        oid: blob.clone(),
                    }],
                }
                .write(),
            ))
            .expect("test operation should succeed");
        let commit = db
            .write_object(EncodedObject::new(
                ObjectType::Commit,
                Commit {
                    tree: tree.clone(),
                    parents: Vec::new(),
                    author: b"A <a@example.invalid> 0 +0000".to_vec(),
                    committer: b"A <a@example.invalid> 0 +0000".to_vec(),
                    encoding: None,
                    message: b"lost\n".to_vec(),
                }
                .write(),
            ))
            .expect("test operation should succeed");

        let dangling = fsck_objects_with_options(
            &db,
            format,
            [],
            [blob.clone(), tree.clone(), commit.clone()],
            FsckOptions {
                report_dangling: true,
                report_unreachable: false,
                ..Default::default()
            },
        );
        assert_eq!(
            dangling.notices,
            vec![FsckNotice {
                message: format!("dangling commit {commit}")
            }]
        );

        let unreachable = fsck_objects_with_options(
            &db,
            format,
            [],
            [blob.clone(), tree.clone(), commit.clone()],
            FsckOptions {
                report_dangling: false,
                report_unreachable: true,
                ..Default::default()
            },
        );
        assert_eq!(
            unreachable.notices,
            vec![
                FsckNotice {
                    message: format!("unreachable blob {blob}")
                },
                FsckNotice {
                    message: format!("unreachable tree {tree}")
                },
                FsckNotice {
                    message: format!("unreachable commit {commit}")
                },
            ]
        );
    }
}
