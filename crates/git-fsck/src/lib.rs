use git_core::{ObjectFormat, ObjectId};
use git_object::{Commit, EncodedObject, ObjectType, Tag, Tree};
use git_odb::ObjectReader;
use std::collections::{HashSet, VecDeque};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsckIssue {
    pub message: String,
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
    pub fn is_ok(&self) -> bool {
        self.issues.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FsckOptions {
    pub report_dangling: bool,
    pub report_unreachable: bool,
}

impl Default for FsckOptions {
    fn default() -> Self {
        Self {
            report_dangling: false,
            report_unreachable: false,
        }
    }
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
            self.issues.push(FsckIssue {
                message: format!(
                    "object {} is {}, expected {}",
                    link.oid,
                    object.object_type.as_str(),
                    link.object_type.as_str()
                ),
            });
        }
        self.check_loaded_object(link.oid, object);
    }

    fn check_object(&mut self, oid: ObjectId) {
        let object = match self.reader.read_object(&oid) {
            Ok(object) => object,
            Err(err) => {
                self.issues.push(FsckIssue {
                    message: format!("missing object {oid}: {err}"),
                });
                return;
            }
        };
        self.check_loaded_object(oid, object);
    }

    fn check_loaded_object(&mut self, oid: ObjectId, object: EncodedObject) {
        if !self.checked.insert(oid.clone()) {
            return;
        }
        match object.object_id(self.format) {
            Ok(actual) if actual == oid => {}
            Ok(actual) => {
                self.issues.push(FsckIssue {
                    message: format!("object id mismatch: expected {oid}, got {actual}"),
                });
                return;
            }
            Err(err) => {
                self.issues.push(FsckIssue {
                    message: format!("invalid object {oid}: {err}"),
                });
                return;
            }
        }
        match object.object_type {
            ObjectType::Commit => self.check_commit(oid, &object.body),
            ObjectType::Tree => self.check_tree(oid, &object.body),
            ObjectType::Tag => self.check_tag(oid, &object.body),
            ObjectType::Blob => {}
        }
    }

    fn check_commit(&mut self, oid: ObjectId, body: &[u8]) {
        let commit = match Commit::parse(self.format, body) {
            Ok(commit) => commit,
            Err(err) => {
                self.issues.push(FsckIssue {
                    message: format!("invalid commit {oid}: {err}"),
                });
                return;
            }
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
        for parent in commit.parents {
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
        let tree = match Tree::parse(self.format, body) {
            Ok(tree) => tree,
            Err(err) => {
                self.issues.push(FsckIssue {
                    message: format!("invalid tree {oid}: {err}"),
                });
                return;
            }
        };
        let source = ObjectLink {
            object_type: ObjectType::Tree,
            oid,
        };
        for entry in tree.entries {
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
        let tag = match Tag::parse(self.format, body) {
            Ok(tag) => tag,
            Err(err) => {
                self.issues.push(FsckIssue {
                    message: format!("invalid tag {oid}: {err}"),
                });
                return;
            }
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
            self.issues.push(FsckIssue {
                message: format!(
                    "broken link from  {} {}\n              to    {} {}",
                    source.object_type.as_str(),
                    source.oid,
                    link.object_type.as_str(),
                    link.oid
                ),
            });
        }
        self.issues.push(FsckIssue {
            message: format!("missing {} {}", link.object_type.as_str(), link.oid),
        });
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
        if !reachable.insert(oid.clone()) {
            continue;
        }
        let Ok(object) = reader.read_object(&oid) else {
            continue;
        };
        for link in object_links(format, &object) {
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
            oid.clone(),
            object.object_type,
            object_links(format, &object),
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
        .map(|(oid, _, _)| oid.clone())
        .collect::<HashSet<_>>();
    let referenced_by_unreachable = unreachable
        .iter()
        .flat_map(|(_, _, links)| links.iter())
        .filter(|link| unreachable_ids.contains(&link.oid))
        .map(|link| link.oid.clone())
        .collect::<HashSet<_>>();
    unreachable
        .into_iter()
        .filter(|(oid, _, _)| !referenced_by_unreachable.contains(oid))
        .map(|(oid, object_type, _)| FsckNotice {
            message: format!("dangling {} {}", object_type.as_str(), oid),
        })
        .collect()
}

fn object_links(format: ObjectFormat, object: &EncodedObject) -> Vec<ObjectLink> {
    match object.object_type {
        ObjectType::Commit => Commit::parse(format, &object.body)
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
        ObjectType::Tree => Tree::parse(format, &object.body)
            .map(|tree| {
                tree.entries
                    .into_iter()
                    .map(|entry| ObjectLink {
                        object_type: fsck_tree_entry_object_type(entry.mode),
                        oid: entry.oid,
                    })
                    .collect()
            })
            .unwrap_or_default(),
        ObjectType::Tag => Tag::parse(format, &object.body)
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
    use git_object::{Commit, EncodedObject, ObjectType, Tree, TreeEntry};
    use git_odb::{ObjectDatabase, ObjectWriter};

    #[test]
    fn fsck_accepts_connected_commit_graph() {
        let format = ObjectFormat::Sha1;
        let mut db = ObjectDatabase::new(format);
        let blob = db
            .write_object(EncodedObject::new(ObjectType::Blob, b"payload\n".to_vec()))
            .unwrap();
        let tree = db
            .write_object(EncodedObject::new(
                ObjectType::Tree,
                Tree {
                    entries: vec![TreeEntry {
                        mode: 0o100644,
                        name: b"payload.txt".to_vec(),
                        oid: blob,
                    }],
                }
                .write(),
            ))
            .unwrap();
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
            .unwrap();

        let report = fsck_objects(&db, format, [commit.clone()], [commit]);
        assert!(report.is_ok(), "{report:?}");
    }

    #[test]
    fn fsck_reports_missing_tree_link() {
        let format = ObjectFormat::Sha1;
        let mut db = ObjectDatabase::new(format);
        let missing_tree =
            ObjectId::from_hex(format, "1111111111111111111111111111111111111111").unwrap();
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
            .unwrap();

        let report = fsck_objects(&db, format, [commit.clone()], [commit]);
        assert_eq!(report.issues.len(), 2);
        assert!(report.issues[0]
            .message
            .contains("broken link from  commit"));
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
            .unwrap();

        let report = fsck_objects_with_options(
            &db,
            format,
            [],
            [blob.clone()],
            FsckOptions {
                report_dangling: true,
                report_unreachable: false,
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
            .unwrap();
        let tree = db
            .write_object(EncodedObject::new(
                ObjectType::Tree,
                Tree {
                    entries: vec![TreeEntry {
                        mode: 0o100644,
                        name: b"lost.txt".to_vec(),
                        oid: blob.clone(),
                    }],
                }
                .write(),
            ))
            .unwrap();
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
            .unwrap();

        let dangling = fsck_objects_with_options(
            &db,
            format,
            [],
            [blob.clone(), tree.clone(), commit.clone()],
            FsckOptions {
                report_dangling: true,
                report_unreachable: false,
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
