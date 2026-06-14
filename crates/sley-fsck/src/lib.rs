use sley_core::{ObjectFormat, ObjectId};
use sley_object::{Commit, EncodedObject, ObjectType, Tag, TreeEntries};
use sley_odb::ObjectReader;
use std::collections::{HashSet, VecDeque};

mod connectivity;
pub mod content;

pub use connectivity::{
    ConnectivityOptions, FsckFinding, FsckFindings, FsckRef, FsckRefTarget, FsckSeverity,
    check_connectivity, check_refs,
};
pub use content::SeverityConfig;

/// Whether an issue is a hard error (fails fsck, exit 1) or a warning (printed
/// but does not by itself fail the check). Both render to stderr.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueSeverity {
    Error,
    Warning,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsckIssue {
    pub message: String,
    pub severity: IssueSeverity,
}

impl FsckIssue {
    /// A hard error issue (broken link, missing object, parse error, ...).
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            severity: IssueSeverity::Error,
        }
    }

    /// A warning issue (does not fail fsck).
    pub fn warning(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            severity: IssueSeverity::Warning,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsckNotice {
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FsckReport {
    pub notices: Vec<FsckNotice>,
    pub issues: Vec<FsckIssue>,
}

impl FsckReport {
    /// True if no *error*-severity issue was found. Warning-severity issues do
    /// not fail fsck (git exits 0 when only warnings are present).
    pub fn is_ok(&self) -> bool {
        !self
            .issues
            .iter()
            .any(|i| i.severity == IssueSeverity::Error)
    }
}

#[derive(Debug, Clone, Default)]
pub struct FsckOptions {
    pub report_dangling: bool,
    pub report_unreachable: bool,
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
    };
    let roots = roots.into_iter().collect::<Vec<_>>();
    let object_ids = object_ids.into_iter().collect::<Vec<_>>();
    for oid in roots.iter().cloned() {
        checker.check_object(oid);
    }
    for oid in object_ids.iter().cloned() {
        checker.check_object(oid);
    }
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
    }
}

struct FsckChecker<'a, R> {
    reader: &'a R,
    format: ObjectFormat,
    checked: HashSet<ObjectId>,
    issues: Vec<FsckIssue>,
    severity: SeverityConfig,
}

impl<R> FsckChecker<'_, R>
where
    R: ObjectReader,
{
    fn check_object_link(&mut self, source: Option<ObjectLink>, link: ObjectLink) {
        let object = match self.reader.read_object(&link.oid) {
            Ok(object) => object,
            Err(_) => {
                self.report_missing_link(source, link);
                return;
            }
        };
        if object.object_type != link.object_type {
            // git: "<oid>: object is a <actual>, not a <expected>".
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
                return;
            }
        };
        self.check_loaded_object(oid, &object);
    }

    fn check_loaded_object(&mut self, oid: ObjectId, object: &EncodedObject) {
        if !self.checked.insert(oid) {
            return;
        }
        match object.object_id(self.format) {
            Ok(actual) if actual == oid => {}
            Ok(actual) => {
                self.issues.push(FsckIssue::error(format!(
                    "object id mismatch: expected {oid}, got {actual}"
                )));
                return;
            }
            Err(err) => {
                self.issues
                    .push(FsckIssue::error(format!("invalid object {oid}: {err}")));
                return;
            }
        }

        // Run git's content checker (commit/tree/tag buffer validation). It
        // emits the exact `error in <type> <oid>: <msgid>: <detail>` /
        // `warning in ...` lines on stderr, with `fsck.<id>` severity applied.
        let content_findings =
            content::check_object_content(object.object_type, &object.body, &self.severity);
        let had_fatal = content_findings.iter().any(|f| f.fatal);
        for f in &content_findings {
            let prefix = match f.severity {
                content::Severity::Error => "error in",
                content::Severity::Warn => "warning in",
                content::Severity::Ignore => continue,
            };
            let msg = format!(
                "{prefix} {} {oid}: {}: {}",
                object.object_type.as_str(),
                f.msg_id.camel(),
                f.detail,
            );
            let issue = match f.severity {
                content::Severity::Error => FsckIssue::error(msg),
                _ => FsckIssue::warning(msg),
            };
            self.issues.push(issue);
        }

        // If a structural (fatal) content problem stopped parsing, do not also
        // run the link walk — git aborts the object too.
        if had_fatal {
            return;
        }

        match object.object_type {
            ObjectType::Commit => self.check_commit(oid, &object.body),
            ObjectType::Tree => self.check_tree(oid, &object.body),
            ObjectType::Tag => self.check_tag(oid, &object.body),
            ObjectType::Blob => {}
        }
    }

    fn check_commit(&mut self, oid: ObjectId, body: &[u8]) {
        // Content checks already ran; for the link walk we tolerate a strict
        // parse failure (the content checker reported the specifics).
        let Ok(commit) = Commit::parse_ref(self.format, body) else {
            return;
        };
        let source = ObjectLink {
            object_type: ObjectType::Commit,
            oid,
        };
        self.check_object_link(
            Some(source.clone()),
            ObjectLink {
                object_type: ObjectType::Tree,
                oid: commit.tree,
            },
        );
        for parent in sley_odb::grafted_parents(self.reader, &oid, commit.parents) {
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
            return;
        };
        let source = ObjectLink {
            object_type: ObjectType::Tree,
            oid,
        };
        for entry in entries {
            // A null-sha entry is reported by the content checker as a warning;
            // do not also walk it as a broken link (git skips null entries).
            if entry.oid.is_null() {
                continue;
            }
            self.check_object_link(
                Some(source.clone()),
                ObjectLink {
                    object_type: fsck_tree_entry_object_type(entry.mode),
                    oid: entry.oid,
                },
            );
        }
    }

    fn check_tag(&mut self, oid: ObjectId, body: &[u8]) {
        // Content checks already ran; tolerate a strict parse failure here.
        let Ok(tag) = Tag::parse_ref(self.format, body) else {
            return;
        };
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

    fn report_missing_link(&mut self, source: Option<ObjectLink>, link: ObjectLink) {
        if let Some(source) = source {
            self.issues.push(FsckIssue::error(format!(
                "broken link from  {} {}\n              to    {} {}",
                source.object_type.as_str(),
                source.oid,
                link.object_type.as_str(),
                link.oid
            )));
        }
        self.issues.push(FsckIssue::error(format!(
            "missing {} {}",
            link.object_type.as_str(),
            link.oid
        )));
    }
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
