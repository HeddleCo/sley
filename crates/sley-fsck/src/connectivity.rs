//! Structured connectivity, reachability, and reference-integrity checks.
//!
//! This module layers a *typed* finding model on top of the string-message
//! oriented [`crate::fsck_objects`] family. Instead of pre-formatted
//! [`crate::FsckIssue`] / [`crate::FsckNotice`] strings, callers receive
//! [`FsckFinding`] values they can match on programmatically (for exit codes,
//! filtering, machine-readable output, etc.) while still being able to render
//! each finding to the exact message `git fsck` prints via
//! [`FsckFinding::message`].
//!
//! The checks provided here are additive and do not change any existing public
//! API in this crate:
//!
//! * [`check_connectivity`] walks the object graph from a set of roots,
//!   reporting missing objects, broken links, type mismatches and corrupt
//!   objects, and (optionally) dangling / unreachable objects.
//! * [`check_refs`] validates that every reference resolves to an object that
//!   actually exists in the object database, reporting `git fsck`-style
//!   `invalid <hash> pointer` errors for direct refs and broken symbolic
//!   references.

use sley_core::{ObjectFormat, ObjectId};
use sley_object::{Commit, EncodedObject, ObjectType, Tag, TreeEntries};
use sley_odb::ObjectReader;
use std::collections::{HashMap, HashSet, VecDeque};

/// Severity of a [`FsckFinding`].
///
/// Mirrors `git fsck`'s split between hard errors (printed to stderr, cause a
/// non-zero exit) and informational notices such as `dangling`/`unreachable`
/// (printed to stdout, do not by themselves fail the check).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FsckSeverity {
    /// A genuine integrity problem (missing object, broken link, bad ref, ...).
    Error,
    /// An informational notice (`dangling`/`unreachable`).
    Notice,
}

/// A single structured finding produced by the connectivity/ref checks.
///
/// Each variant renders to the exact textual form emitted by `git fsck` via
/// [`FsckFinding::message`], and is classified as an error or notice via
/// [`FsckFinding::severity`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsckFinding {
    /// An object that was expected to exist could not be read from the database.
    ///
    /// Renders as `missing <type> <oid>`.
    MissingObject {
        /// The type the referrer expected this object to have.
        object_type: ObjectType,
        /// The object id that is absent.
        oid: ObjectId,
    },
    /// A reachable object references another object that is missing.
    ///
    /// Renders as the two-line `git fsck` form:
    ///
    /// ```text
    /// broken link from  <stype> <source>
    ///               to    <ttype> <target>
    /// ```
    BrokenLink {
        /// Type of the object holding the dangling reference.
        source_type: ObjectType,
        /// Object id holding the dangling reference.
        source: ObjectId,
        /// Type the missing target was expected to have.
        target_type: ObjectType,
        /// Object id of the missing target.
        target: ObjectId,
    },
    /// An object exists but has a different type than a referrer expected.
    ///
    /// Renders as `object <oid> is <actual>, expected <expected>`.
    TypeMismatch {
        /// The object id whose type does not match.
        oid: ObjectId,
        /// The actual type stored in the database.
        actual: ObjectType,
        /// The type that was expected by the referrer.
        expected: ObjectType,
    },
    /// An object could not be parsed or its content hashes to a different id.
    ///
    /// Renders as `<reason>` (the reason is pre-formatted to match git, e.g.
    /// `invalid commit <oid>: <err>` or
    /// `object id mismatch: expected <oid>, got <actual>`).
    CorruptObject {
        /// The object id that is corrupt (best-effort; the claimed id).
        oid: ObjectId,
        /// A git-style description of the corruption.
        reason: String,
    },
    /// An unreachable object that is not referenced by any other unreachable
    /// object (a tip of the unreachable graph).
    ///
    /// Renders as `dangling <type> <oid>`.
    Dangling {
        /// The object's type.
        object_type: ObjectType,
        /// The object id.
        oid: ObjectId,
    },
    /// An object not reachable from any root.
    ///
    /// Renders as `unreachable <type> <oid>`.
    Unreachable {
        /// The object's type.
        object_type: ObjectType,
        /// The object id.
        oid: ObjectId,
    },
    /// A direct reference points at an object that does not exist in the object
    /// database.
    ///
    /// Renders as `error: <refname>: invalid <hash> pointer <oid>`, matching
    /// `git fsck`'s diagnostic for refs with missing targets.
    BadRefTarget {
        /// The fully-qualified reference name (e.g. `refs/heads/main`).
        refname: String,
        /// The object id the reference points at.
        oid: ObjectId,
        /// The hash algorithm name (`sha1` / `sha256`) used in the message.
        hash: &'static str,
    },
    /// A symbolic reference points at a target that cannot be resolved (the
    /// pointed-to ref is absent from the provided set, or the symref chain
    /// loops/exceeds the resolution depth).
    ///
    /// `git fsck` treats an unresolved symbolic ref as pointing at the null
    /// object id, so this renders identically to a [`FsckFinding::BadRefTarget`]
    /// with an all-zero oid: `error: <refname>: invalid <hash> pointer
    /// 0000000000000000000000000000000000000000`. The unresolved `target` ref
    /// name is retained for callers that want the structured detail.
    BrokenSymref {
        /// The symbolic reference name.
        refname: String,
        /// The unresolved target ref name.
        target: String,
        /// The hash algorithm name (`sha1` / `sha256`) used in the message.
        hash: &'static str,
    },
}

impl FsckFinding {
    /// The severity of this finding.
    pub fn severity(&self) -> FsckSeverity {
        match self {
            FsckFinding::Dangling { .. } | FsckFinding::Unreachable { .. } => FsckSeverity::Notice,
            _ => FsckSeverity::Error,
        }
    }

    /// `true` if this finding is an error (rather than an informational notice).
    pub fn is_error(&self) -> bool {
        matches!(self.severity(), FsckSeverity::Error)
    }

    /// Render the finding to the exact message `git fsck` would print.
    ///
    /// Note that [`FsckFinding::BrokenLink`] renders to a two-line string, just
    /// like git, and [`FsckFinding::BadRefTarget`] / [`FsckFinding::BrokenSymref`]
    /// include the leading `error: ` prefix as git does for ref problems.
    pub fn message(&self) -> String {
        match self {
            FsckFinding::MissingObject { object_type, oid } => {
                format!("missing {} {}", object_type.as_str(), oid)
            }
            FsckFinding::BrokenLink {
                source_type,
                source,
                target_type,
                target,
            } => format!(
                "broken link from {} {}\n              to {} {}",
                pad_type(*source_type),
                source,
                pad_type(*target_type),
                target
            ),
            FsckFinding::TypeMismatch {
                oid,
                actual,
                expected,
            } => format!(
                "object {} is {}, expected {}",
                oid,
                actual.as_str(),
                expected.as_str()
            ),
            FsckFinding::CorruptObject { reason, .. } => reason.clone(),
            FsckFinding::Dangling { object_type, oid } => {
                format!("dangling {} {}", object_type.as_str(), oid)
            }
            FsckFinding::Unreachable { object_type, oid } => {
                format!("unreachable {} {}", object_type.as_str(), oid)
            }
            FsckFinding::BadRefTarget { refname, oid, hash } => {
                format!("error: {}: invalid {} pointer {}", refname, hash, oid)
            }
            FsckFinding::BrokenSymref { refname, hash, .. } => {
                // git resolves an unresolved symref to the null oid.
                format!(
                    "error: {}: invalid {} pointer {}",
                    refname,
                    hash,
                    null_oid_hex(hash)
                )
            }
        }
    }
}

impl std::fmt::Display for FsckFinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

/// `git fsck` right-aligns the object-type name inside a width-7 field
/// (`printf "%7s"`) so the `from`/`to` columns line up. With the single
/// literal space that precedes the field in git's format string, `commit`
/// renders as `from  commit` (two spaces) and `tree` as `from    tree`
/// (four spaces). This reproduces that alignment exactly.
fn pad_type(object_type: ObjectType) -> String {
    format!("{:>7}", object_type.as_str())
}

/// The all-zero object id, rendered as hex, for the given hash algorithm name.
///
/// sha1 ids are 40 hex digits; sha256 ids are 64. `git fsck` prints this null
/// oid when a symbolic ref cannot be resolved.
fn null_oid_hex(hash: &str) -> String {
    let len = if hash == "sha256" { 64 } else { 40 };
    "0".repeat(len)
}

/// A collection of [`FsckFinding`]s with convenient accessors.
///
/// Findings preserve the order in which they were discovered, matching the
/// deterministic, input-order traversal used by the rest of this crate.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FsckFindings {
    /// All findings, in discovery order.
    pub findings: Vec<FsckFinding>,
}

impl FsckFindings {
    /// Create an empty set of findings.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a single finding.
    pub fn push(&mut self, finding: FsckFinding) {
        self.findings.push(finding);
    }

    /// Append all findings from another set.
    pub fn extend_from(&mut self, other: FsckFindings) {
        self.findings.extend(other.findings);
    }

    /// `true` if there are no *error* findings (notices are allowed).
    ///
    /// This matches `git fsck`'s notion of success: dangling/unreachable
    /// notices do not by themselves constitute a failure.
    pub fn is_ok(&self) -> bool {
        !self.findings.iter().any(FsckFinding::is_error)
    }

    /// Iterate over the error findings only.
    pub fn errors(&self) -> impl Iterator<Item = &FsckFinding> {
        self.findings.iter().filter(|f| f.is_error())
    }

    /// Iterate over the notice findings only.
    pub fn notices(&self) -> impl Iterator<Item = &FsckFinding> {
        self.findings
            .iter()
            .filter(|f| matches!(f.severity(), FsckSeverity::Notice))
    }

    /// The process exit code `git fsck` would use: `0` when there are no
    /// errors, otherwise `1`.
    ///
    /// (`git fsck` uses a non-zero exit on any error; the precise non-zero
    /// value varies, but callers driving a CLI typically only need
    /// success-vs-failure.)
    pub fn exit_code(&self) -> i32 {
        if self.is_ok() { 0 } else { 1 }
    }
}

/// Controls which optional, non-error checks [`check_connectivity`] performs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ConnectivityOptions {
    /// Emit [`FsckFinding::Dangling`] notices for unreachable graph tips.
    pub report_dangling: bool,
    /// Emit [`FsckFinding::Unreachable`] notices for every unreachable object.
    pub report_unreachable: bool,
}

/// Walk the object graph from `roots` and check connectivity/integrity.
///
/// This verifies, for every reachable object, that:
///
/// * the object exists in the database ([`FsckFinding::MissingObject`] /
///   [`FsckFinding::BrokenLink`] otherwise),
/// * its content hashes to its claimed id and parses cleanly
///   ([`FsckFinding::CorruptObject`] otherwise), and
/// * each referenced object has the expected type
///   ([`FsckFinding::TypeMismatch`] otherwise).
///
/// `objects` is the full set of object ids present in the database (e.g. from
/// [`sley_odb::FileObjectDatabase::object_ids`]); it is used to compute the
/// dangling/unreachable notices requested via [`ConnectivityOptions`]. When
/// neither notice is requested, `objects` is ignored and may be empty.
///
/// The traversal is deterministic: roots are visited in order, then their
/// links in object order, exactly like the existing [`crate::fsck_objects`]
/// implementation.
pub fn check_connectivity<R>(
    reader: &R,
    format: ObjectFormat,
    roots: &[ObjectId],
    objects: &[ObjectId],
    options: ConnectivityOptions,
) -> FsckFindings
where
    R: ObjectReader,
{
    let mut walker = ConnectivityWalker {
        reader,
        format,
        checked: HashSet::new(),
        findings: Vec::new(),
    };
    for oid in roots {
        walker.check_root(*oid);
    }
    let mut findings = FsckFindings {
        findings: walker.findings,
    };

    if options.report_unreachable {
        for finding in unreachable_findings(reader, format, roots, objects, false) {
            findings.push(finding);
        }
    } else if options.report_dangling {
        for finding in unreachable_findings(reader, format, roots, objects, true) {
            findings.push(finding);
        }
    }

    findings
}

struct ObjectLink {
    object_type: ObjectType,
    oid: ObjectId,
}

struct ConnectivityWalker<'a, R> {
    reader: &'a R,
    format: ObjectFormat,
    checked: HashSet<ObjectId>,
    findings: Vec<FsckFinding>,
}

impl<R> ConnectivityWalker<'_, R>
where
    R: ObjectReader,
{
    /// Check a root object: a missing root is reported as a bare
    /// `missing <type> <oid>` (git does not have a `broken link` source for a
    /// root). We optimistically treat an unreadable root as a missing commit,
    /// matching `git fsck <oid>` which expects a committish tip.
    fn check_root(&mut self, oid: ObjectId) {
        match self.reader.read_object(&oid) {
            Ok(object) => self.check_loaded(oid, &object),
            Err(_) => self.findings.push(FsckFinding::MissingObject {
                object_type: ObjectType::Commit,
                oid,
            }),
        }
    }

    fn check_link(&mut self, source_type: ObjectType, source: &ObjectId, link: ObjectLink) {
        let object = match self.reader.read_object(&link.oid) {
            Ok(object) => object,
            Err(_) => {
                if self.reader.is_promised_object(&link.oid) {
                    return;
                }
                self.findings.push(FsckFinding::BrokenLink {
                    source_type,
                    source: source.clone(),
                    target_type: link.object_type,
                    target: link.oid,
                });
                self.findings.push(FsckFinding::MissingObject {
                    object_type: link.object_type,
                    oid: link.oid,
                });
                return;
            }
        };
        if object.object_type != link.object_type {
            self.findings.push(FsckFinding::TypeMismatch {
                oid: link.oid,
                actual: object.object_type,
                expected: link.object_type,
            });
        }
        self.check_loaded(link.oid, &object);
    }

    fn check_loaded(&mut self, oid: ObjectId, object: &EncodedObject) {
        if !self.checked.insert(oid) {
            return;
        }
        match object.object_id(self.format) {
            Ok(actual) if actual == oid => {}
            Ok(actual) => {
                self.findings.push(FsckFinding::CorruptObject {
                    reason: format!("object id mismatch: expected {oid}, got {actual}"),
                    oid,
                });
                return;
            }
            Err(err) => {
                self.findings.push(FsckFinding::CorruptObject {
                    reason: format!("invalid object {oid}: {err}"),
                    oid,
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
        let mut links = match commit_links(self.format, body) {
            Ok(links) => links,
            Err(err) => {
                self.findings.push(FsckFinding::CorruptObject {
                    reason: format!("invalid commit {oid}: {err}"),
                    oid,
                });
                return;
            }
        };
        // Graft seam: parents of a shallow boundary commit are cut.
        if self.reader.is_shallow_graft(&oid) {
            links.retain(|link| link.object_type != ObjectType::Commit);
        }
        for link in links {
            self.check_link(ObjectType::Commit, &oid, link);
        }
    }

    fn check_tree(&mut self, oid: ObjectId, body: &[u8]) {
        let links = match tree_links(self.format, body) {
            Ok(links) => links,
            Err(err) => {
                self.findings.push(FsckFinding::CorruptObject {
                    reason: format!("invalid tree {oid}: {err}"),
                    oid,
                });
                return;
            }
        };
        for link in links {
            self.check_link(ObjectType::Tree, &oid, link);
        }
    }

    fn check_tag(&mut self, oid: ObjectId, body: &[u8]) {
        let links = match tag_links(self.format, body) {
            Ok(links) => links,
            Err(err) => {
                self.findings.push(FsckFinding::CorruptObject {
                    reason: format!("invalid tag {oid}: {err}"),
                    oid,
                });
                return;
            }
        };
        for link in links {
            self.check_link(ObjectType::Tag, &oid, link);
        }
    }
}

/// A reference target for [`check_refs`].
///
/// This mirrors `sley_refs::RefTarget` without taking a dependency on that
/// crate; callers map their refs into this small enum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsckRefTarget {
    /// A direct reference to an object id.
    Direct(ObjectId),
    /// A symbolic reference to another reference by name.
    Symbolic(String),
}

/// A named reference paired with its target, as consumed by [`check_refs`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsckRef {
    /// Fully-qualified reference name (e.g. `refs/heads/main`, `HEAD`).
    pub name: String,
    /// The reference target.
    pub target: FsckRefTarget,
}

impl FsckRef {
    /// Construct a direct reference.
    pub fn direct(name: impl Into<String>, oid: ObjectId) -> Self {
        Self {
            name: name.into(),
            target: FsckRefTarget::Direct(oid),
        }
    }

    /// Construct a symbolic reference.
    pub fn symbolic(name: impl Into<String>, target: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            target: FsckRefTarget::Symbolic(target.into()),
        }
    }
}

/// Validate that every reference resolves to an object present in the database.
///
/// For each [`FsckRef`]:
///
/// * a direct ref whose target object is missing yields
///   [`FsckFinding::BadRefTarget`] (`error: <ref>: invalid <hash> pointer <oid>`),
///   matching `git fsck`;
/// * a symbolic ref is followed (within the provided set, up to a small depth
///   limit) to its terminal direct target; an unresolved symbolic target yields
///   [`FsckFinding::BrokenSymref`].
///
/// References are validated in the order provided, producing deterministic
/// output. This does not walk the objects pointed at — combine with
/// [`check_connectivity`] (passing the resolved ref tips as roots) for a full
/// `git fsck` equivalent.
pub fn check_refs<R>(reader: &R, format: ObjectFormat, refs: &[FsckRef]) -> FsckFindings
where
    R: ObjectReader,
{
    let by_name: HashMap<&str, &FsckRefTarget> =
        refs.iter().map(|r| (r.name.as_str(), &r.target)).collect();
    let mut findings = FsckFindings::new();
    for reference in refs {
        match resolve_ref_target(&reference.name, &reference.target, &by_name) {
            RefResolution::Direct(oid) => {
                if reader.read_object(&oid).is_err() {
                    findings.push(FsckFinding::BadRefTarget {
                        refname: reference.name.clone(),
                        oid,
                        hash: format.name(),
                    });
                }
            }
            RefResolution::Broken { refname, target } => {
                findings.push(FsckFinding::BrokenSymref {
                    refname,
                    target,
                    hash: format.name(),
                });
            }
        }
    }
    findings
}

enum RefResolution {
    Direct(ObjectId),
    Broken { refname: String, target: String },
}

fn resolve_ref_target(
    refname: &str,
    target: &FsckRefTarget,
    by_name: &HashMap<&str, &FsckRefTarget>,
) -> RefResolution {
    let mut current = target.clone();
    // Guard against symref cycles / overly deep chains the way git bounds
    // symbolic-ref resolution.
    for _ in 0..5 {
        match current {
            FsckRefTarget::Direct(oid) => return RefResolution::Direct(oid),
            FsckRefTarget::Symbolic(next) => match by_name.get(next.as_str()) {
                Some(found) => current = (*found).clone(),
                None => {
                    return RefResolution::Broken {
                        refname: refname.to_string(),
                        target: next,
                    };
                }
            },
        }
    }
    RefResolution::Broken {
        refname: refname.to_string(),
        target: "<symref chain too deep>".to_string(),
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

/// Compute dangling- or unreachable-object notices.
///
/// When `only_tips` is true, only the tips of the unreachable graph (objects
/// not referenced by any other unreachable object) are reported as
/// [`FsckFinding::Dangling`]; otherwise every unreachable object is reported as
/// [`FsckFinding::Unreachable`].
fn unreachable_findings<R>(
    reader: &R,
    format: ObjectFormat,
    roots: &[ObjectId],
    objects: &[ObjectId],
    only_tips: bool,
) -> Vec<FsckFinding>
where
    R: ObjectReader,
{
    let reachable = reachable_objects(reader, format, roots);
    let mut unreachable: Vec<(ObjectId, ObjectType, Vec<ObjectLink>)> = Vec::new();
    for oid in objects {
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

    if only_tips {
        let unreachable_ids: HashSet<ObjectId> =
            unreachable.iter().map(|(oid, _, _)| *oid).collect();
        let referenced: HashSet<ObjectId> = unreachable
            .iter()
            .flat_map(|(_, _, links)| links.iter())
            .filter(|link| unreachable_ids.contains(&link.oid))
            .map(|link| link.oid)
            .collect();
        unreachable
            .into_iter()
            .filter(|(oid, _, _)| !referenced.contains(oid))
            .map(|(oid, object_type, _)| FsckFinding::Dangling { object_type, oid })
            .collect()
    } else {
        unreachable
            .into_iter()
            .map(|(oid, object_type, _)| FsckFinding::Unreachable { object_type, oid })
            .collect()
    }
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
        ObjectType::Commit => commit_links(format, &object.body).unwrap_or_default(),
        ObjectType::Tree => tree_links(format, &object.body).unwrap_or_default(),
        ObjectType::Tag => tag_links(format, &object.body).unwrap_or_default(),
        ObjectType::Blob => Vec::new(),
    }
}

fn commit_links(format: ObjectFormat, body: &[u8]) -> sley_core::Result<Vec<ObjectLink>> {
    let commit = Commit::parse_ref(format, body)?;
    let mut links = Vec::with_capacity(commit.parents.len() + 1);
    links.push(ObjectLink {
        object_type: ObjectType::Tree,
        oid: commit.tree,
    });
    links.extend(commit.parents.into_iter().map(|parent| ObjectLink {
        object_type: ObjectType::Commit,
        oid: parent,
    }));
    Ok(links)
}

fn tree_links(format: ObjectFormat, body: &[u8]) -> sley_core::Result<Vec<ObjectLink>> {
    let mut links = Vec::new();
    for entry in TreeEntries::new(format, body) {
        let entry = entry?;
        links.push(ObjectLink {
            object_type: tree_entry_object_type(entry.mode),
            oid: entry.oid,
        });
    }
    Ok(links)
}

fn tag_links(format: ObjectFormat, body: &[u8]) -> sley_core::Result<Vec<ObjectLink>> {
    let tag = Tag::parse_ref(format, body)?;
    Ok(vec![ObjectLink {
        object_type: tag.object_type,
        oid: tag.object,
    }])
}

fn tree_entry_object_type(mode: u32) -> ObjectType {
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
    use sley_object::{Commit, EncodedObject, ObjectType, Tag, Tree, TreeEntry};
    use sley_odb::{ObjectDatabase, ObjectWriter};

    fn fmt() -> ObjectFormat {
        ObjectFormat::Sha1
    }

    fn oid(hex: &str) -> ObjectId {
        ObjectId::from_hex(ObjectFormat::Sha1, hex).expect("valid oid hex")
    }

    fn write_blob(db: &mut ObjectDatabase, content: &[u8]) -> ObjectId {
        db.write_object(EncodedObject::new(ObjectType::Blob, content.to_vec()))
            .expect("write blob")
    }

    fn write_tree(db: &mut ObjectDatabase, entries: Vec<TreeEntry>) -> ObjectId {
        db.write_object(EncodedObject::new(
            ObjectType::Tree,
            Tree { entries }.write(),
        ))
        .expect("write tree")
    }

    fn write_commit(db: &mut ObjectDatabase, tree: ObjectId, parents: Vec<ObjectId>) -> ObjectId {
        db.write_object(EncodedObject::new(
            ObjectType::Commit,
            Commit {
                tree,
                parents,
                author: b"A <a@example.invalid> 0 +0000".to_vec(),
                committer: b"A <a@example.invalid> 0 +0000".to_vec(),
                encoding: None,
                message: b"msg\n".to_vec(),
            }
            .write(),
        ))
        .expect("write commit")
    }

    fn write_tag(
        db: &mut ObjectDatabase,
        object: ObjectId,
        object_type: ObjectType,
        name: &str,
    ) -> ObjectId {
        db.write_object(EncodedObject::new(
            ObjectType::Tag,
            Tag {
                object,
                object_type,
                name: name.as_bytes().to_vec(),
                tagger: Some(b"A <a@example.invalid> 0 +0000".to_vec()),
                message: b"tag msg\n".to_vec(),
                raw_body: None,
            }
            .write(),
        ))
        .expect("write tag")
    }

    /// A fully connected commit -> tree -> blob graph produces no findings.
    #[test]
    fn connectivity_accepts_connected_graph() {
        let mut db = ObjectDatabase::new(fmt());
        let blob = write_blob(&mut db, b"payload\n");
        let tree = write_tree(
            &mut db,
            vec![TreeEntry {
                mode: 0o100644,
                name: BString::from(b"payload.txt"),
                oid: blob,
            }],
        );
        let commit = write_commit(&mut db, tree, Vec::new());

        let findings =
            check_connectivity(&db, fmt(), &[commit], &[], ConnectivityOptions::default());
        assert!(findings.is_ok(), "{findings:?}");
        assert_eq!(findings.exit_code(), 0);
        assert!(findings.findings.is_empty());
    }

    /// A commit referencing a missing tree yields a broken link + missing object.
    #[test]
    fn connectivity_reports_missing_tree_link() {
        let mut db = ObjectDatabase::new(fmt());
        let missing_tree = oid("1111111111111111111111111111111111111111");
        let commit = write_commit(&mut db, missing_tree, Vec::new());

        let findings = check_connectivity(
            &db,
            fmt(),
            std::slice::from_ref(&commit),
            &[],
            ConnectivityOptions::default(),
        );

        assert!(!findings.is_ok());
        assert_eq!(findings.exit_code(), 1);
        assert_eq!(findings.findings.len(), 2);
        assert_eq!(
            findings.findings[0],
            FsckFinding::BrokenLink {
                source_type: ObjectType::Commit,
                source: commit,
                target_type: ObjectType::Tree,
                target: missing_tree,
            }
        );
        assert_eq!(
            findings.findings[1],
            FsckFinding::MissingObject {
                object_type: ObjectType::Tree,
                oid: missing_tree,
            }
        );
    }

    /// A tree referencing a missing blob yields a broken link + missing object.
    #[test]
    fn connectivity_reports_missing_blob_link() {
        let mut db = ObjectDatabase::new(fmt());
        let missing_blob = oid("2222222222222222222222222222222222222222");
        let tree = write_tree(
            &mut db,
            vec![TreeEntry {
                mode: 0o100644,
                name: BString::from(b"gone.txt"),
                oid: missing_blob.clone(),
            }],
        );
        let commit = write_commit(&mut db, tree, Vec::new());

        let findings =
            check_connectivity(&db, fmt(), &[commit], &[], ConnectivityOptions::default());

        let messages: Vec<String> = findings.findings.iter().map(|f| f.message()).collect();
        assert!(
            messages
                .iter()
                .any(|m| m == &format!("missing blob {missing_blob}")),
            "{messages:?}"
        );
        assert!(findings.findings.iter().any(|f| matches!(
            f,
            FsckFinding::BrokenLink {
                source_type: ObjectType::Tree,
                ..
            }
        )));
    }

    /// A missing root object is reported as a bare `missing commit <oid>`.
    #[test]
    fn connectivity_reports_missing_root() {
        let db = ObjectDatabase::new(fmt());
        let missing = oid("3333333333333333333333333333333333333333");

        let findings = check_connectivity(
            &db,
            fmt(),
            std::slice::from_ref(&missing),
            &[],
            ConnectivityOptions::default(),
        );

        assert_eq!(
            findings.findings,
            vec![FsckFinding::MissingObject {
                object_type: ObjectType::Commit,
                oid: missing,
            }]
        );
        assert!(!findings.is_ok());
    }

    /// An object whose actual type differs from the referrer's expectation is
    /// reported as a type mismatch (a commit pointing its "tree" link at a blob).
    #[test]
    fn connectivity_reports_type_mismatch() {
        let mut db = ObjectDatabase::new(fmt());
        // Make a blob and point a commit's tree at it.
        let blob = write_blob(&mut db, b"not a tree\n");
        let commit = write_commit(&mut db, blob.clone(), Vec::new());

        let findings =
            check_connectivity(&db, fmt(), &[commit], &[], ConnectivityOptions::default());

        assert!(findings.findings.contains(&FsckFinding::TypeMismatch {
            oid: blob.clone(),
            actual: ObjectType::Blob,
            expected: ObjectType::Tree,
        }));
        assert_eq!(
            FsckFinding::TypeMismatch {
                oid: blob.clone(),
                actual: ObjectType::Blob,
                expected: ObjectType::Tree,
            }
            .message(),
            format!("object {blob} is blob, expected tree")
        );
    }

    /// A blob whose stored bytes do not hash to its key is reported as corrupt.
    #[test]
    fn connectivity_reports_corrupt_object_hash_mismatch() {
        // Build a db where we deliberately insert an object under the wrong id.
        // ObjectDatabase hashes on write, so instead corrupt via a custom reader.
        struct LyingReader {
            real: ObjectDatabase,
            claimed: ObjectId,
            payload: EncodedObject,
        }
        impl ObjectReader for LyingReader {
            fn read_object(
                &self,
                oid: &ObjectId,
            ) -> sley_core::Result<std::sync::Arc<EncodedObject>> {
                if oid == &self.claimed {
                    Ok(std::sync::Arc::new(self.payload.clone()))
                } else {
                    self.real.read_object(oid)
                }
            }
        }

        let real = ObjectDatabase::new(fmt());
        let claimed = oid("4444444444444444444444444444444444444444");
        let payload = EncodedObject::new(ObjectType::Blob, b"mismatched\n".to_vec());
        let reader = LyingReader {
            real,
            claimed: claimed.clone(),
            payload,
        };

        let findings = check_connectivity(
            &reader,
            fmt(),
            std::slice::from_ref(&claimed),
            &[],
            ConnectivityOptions::default(),
        );

        assert_eq!(findings.findings.len(), 1);
        match &findings.findings[0] {
            FsckFinding::CorruptObject { oid, reason } => {
                assert_eq!(oid, &claimed);
                assert!(reason.starts_with("object id mismatch:"), "{reason}");
            }
            other => panic!("expected corrupt object, got {other:?}"),
        }
    }

    /// A malformed commit body is reported as corrupt with git's wording.
    #[test]
    fn connectivity_reports_corrupt_commit_parse() {
        let mut db = ObjectDatabase::new(fmt());
        let bad = db
            .write_object(EncodedObject::new(
                ObjectType::Commit,
                b"this is not a valid commit".to_vec(),
            ))
            .expect("write bad commit");

        let findings = check_connectivity(
            &db,
            fmt(),
            std::slice::from_ref(&bad),
            &[],
            ConnectivityOptions::default(),
        );

        assert_eq!(findings.findings.len(), 1);
        match &findings.findings[0] {
            FsckFinding::CorruptObject { oid, reason } => {
                assert_eq!(oid, &bad);
                assert!(
                    reason.starts_with(&format!("invalid commit {bad}:")),
                    "{reason}"
                );
            }
            other => panic!("expected corrupt commit, got {other:?}"),
        }
    }

    /// Annotated tags are followed and their targets validated.
    #[test]
    fn connectivity_follows_tag_to_missing_commit() {
        let mut db = ObjectDatabase::new(fmt());
        let missing_commit = oid("5555555555555555555555555555555555555555");
        let tag = write_tag(&mut db, missing_commit, ObjectType::Commit, "v1");

        let findings = check_connectivity(
            &db,
            fmt(),
            std::slice::from_ref(&tag),
            &[],
            ConnectivityOptions::default(),
        );

        assert!(findings.findings.contains(&FsckFinding::BrokenLink {
            source_type: ObjectType::Tag,
            source: tag,
            target_type: ObjectType::Commit,
            target: missing_commit,
        }));
        assert!(findings.findings.contains(&FsckFinding::MissingObject {
            object_type: ObjectType::Commit,
            oid: missing_commit,
        }));
    }

    /// Dangling notices report only the tip of the unreachable graph.
    #[test]
    fn dangling_reports_only_graph_tips() {
        let mut db = ObjectDatabase::new(fmt());
        let blob = write_blob(&mut db, b"lost\n");
        let tree = write_tree(
            &mut db,
            vec![TreeEntry {
                mode: 0o100644,
                name: BString::from(b"lost.txt"),
                oid: blob.clone(),
            }],
        );
        let commit = write_commit(&mut db, tree, Vec::new());

        let findings = check_connectivity(
            &db,
            fmt(),
            &[],
            &[blob.clone(), tree, commit],
            ConnectivityOptions {
                report_dangling: true,
                report_unreachable: false,
            },
        );

        // No errors, only a single dangling commit notice (the tip).
        assert!(findings.is_ok());
        assert_eq!(findings.exit_code(), 0);
        let notices: Vec<&FsckFinding> = findings.notices().collect();
        assert_eq!(
            notices,
            vec![&FsckFinding::Dangling {
                object_type: ObjectType::Commit,
                oid: commit,
            }]
        );
    }

    /// A genuinely standalone blob is dangling.
    #[test]
    fn dangling_reports_standalone_blob() {
        let mut db = ObjectDatabase::new(fmt());
        let blob = write_blob(&mut db, b"orphan\n");

        let findings = check_connectivity(
            &db,
            fmt(),
            &[],
            std::slice::from_ref(&blob),
            ConnectivityOptions {
                report_dangling: true,
                report_unreachable: false,
            },
        );
        assert_eq!(
            findings.findings,
            vec![FsckFinding::Dangling {
                object_type: ObjectType::Blob,
                oid: blob.clone(),
            }]
        );
        assert_eq!(
            findings.findings[0].message(),
            format!("dangling blob {blob}")
        );
    }

    /// Unreachable notices report every unreachable object, in input order.
    #[test]
    fn unreachable_reports_all_objects() {
        let mut db = ObjectDatabase::new(fmt());
        let blob = write_blob(&mut db, b"lost\n");
        let tree = write_tree(
            &mut db,
            vec![TreeEntry {
                mode: 0o100644,
                name: BString::from(b"lost.txt"),
                oid: blob.clone(),
            }],
        );
        let commit = write_commit(&mut db, tree, Vec::new());

        let findings = check_connectivity(
            &db,
            fmt(),
            &[],
            &[blob.clone(), tree, commit],
            ConnectivityOptions {
                report_dangling: false,
                report_unreachable: true,
            },
        );

        assert_eq!(
            findings.findings,
            vec![
                FsckFinding::Unreachable {
                    object_type: ObjectType::Blob,
                    oid: blob,
                },
                FsckFinding::Unreachable {
                    object_type: ObjectType::Tree,
                    oid: tree,
                },
                FsckFinding::Unreachable {
                    object_type: ObjectType::Commit,
                    oid: commit,
                },
            ]
        );
        assert!(findings.is_ok());
    }

    /// Objects reachable from a root are not reported as dangling/unreachable.
    #[test]
    fn reachable_objects_are_not_dangling() {
        let mut db = ObjectDatabase::new(fmt());
        let blob = write_blob(&mut db, b"kept\n");
        let tree = write_tree(
            &mut db,
            vec![TreeEntry {
                mode: 0o100644,
                name: BString::from(b"kept.txt"),
                oid: blob.clone(),
            }],
        );
        let commit = write_commit(&mut db, tree, Vec::new());

        let roots = [commit];
        let findings = check_connectivity(
            &db,
            fmt(),
            &roots,
            &[blob, tree, commit],
            ConnectivityOptions {
                report_dangling: true,
                report_unreachable: true,
            },
        );
        assert!(findings.findings.is_empty(), "{findings:?}");
    }

    /// A direct ref pointing at a present object passes; pointing at a missing
    /// object produces git's `invalid sha1 pointer` error.
    #[test]
    fn check_refs_validates_direct_targets() {
        let mut db = ObjectDatabase::new(fmt());
        let blob = write_blob(&mut db, b"x\n");
        let tree = write_tree(
            &mut db,
            vec![TreeEntry {
                mode: 0o100644,
                name: BString::from(b"x"),
                oid: blob,
            }],
        );
        let commit = write_commit(&mut db, tree, Vec::new());
        let missing = oid("6666666666666666666666666666666666666666");

        let refs = vec![
            FsckRef::direct("refs/heads/main", commit),
            FsckRef::direct("refs/heads/broken", missing.clone()),
        ];
        let findings = check_refs(&db, fmt(), &refs);

        assert_eq!(
            findings.findings,
            vec![FsckFinding::BadRefTarget {
                refname: "refs/heads/broken".to_string(),
                oid: missing.clone(),
                hash: "sha1",
            }]
        );
        assert_eq!(
            findings.findings[0].message(),
            format!("error: refs/heads/broken: invalid sha1 pointer {missing}")
        );
        assert!(!findings.is_ok());
        assert_eq!(findings.exit_code(), 1);
    }

    /// The hash name in ref errors tracks the object format.
    #[test]
    fn check_refs_uses_sha256_hash_name() {
        let db = ObjectDatabase::new(ObjectFormat::Sha256);
        let missing = ObjectId::from_hex(
            ObjectFormat::Sha256,
            "0000000000000000000000000000000000000000000000000000000000000001",
        )
        .expect("sha256 oid");
        let refs = vec![FsckRef::direct("refs/heads/x", missing.clone())];
        let findings = check_refs(&db, ObjectFormat::Sha256, &refs);
        assert_eq!(
            findings.findings[0].message(),
            format!("error: refs/heads/x: invalid sha256 pointer {missing}")
        );
    }

    /// A symbolic ref is followed to its terminal direct target.
    #[test]
    fn check_refs_follows_symbolic_refs() {
        let mut db = ObjectDatabase::new(fmt());
        let blob = write_blob(&mut db, b"x\n");
        let tree = write_tree(
            &mut db,
            vec![TreeEntry {
                mode: 0o100644,
                name: BString::from(b"x"),
                oid: blob,
            }],
        );
        let commit = write_commit(&mut db, tree, Vec::new());

        let refs = vec![
            FsckRef::symbolic("HEAD", "refs/heads/main"),
            FsckRef::direct("refs/heads/main", commit),
        ];
        let findings = check_refs(&db, fmt(), &refs);
        assert!(findings.is_ok(), "{findings:?}");
    }

    /// A symbolic ref pointing at a nonexistent ref is reported as broken, and
    /// renders exactly like `git fsck` (null-oid pointer error).
    #[test]
    fn check_refs_reports_broken_symref() {
        let db = ObjectDatabase::new(fmt());
        let refs = vec![FsckRef::symbolic("refs/heads/weird", "refs/heads/gone")];
        let findings = check_refs(&db, fmt(), &refs);
        assert_eq!(
            findings.findings,
            vec![FsckFinding::BrokenSymref {
                refname: "refs/heads/weird".to_string(),
                target: "refs/heads/gone".to_string(),
                hash: "sha1",
            }]
        );
        // Matches `git fsck`: `error: <ref>: invalid sha1 pointer 000...0`.
        assert_eq!(
            findings.findings[0].message(),
            format!(
                "error: refs/heads/weird: invalid sha1 pointer {}",
                "0".repeat(40)
            )
        );
        assert!(!findings.is_ok());
    }

    /// An unresolved sha256 symref renders a 64-zero null pointer.
    #[test]
    fn check_refs_broken_symref_sha256_null_oid() {
        let db = ObjectDatabase::new(ObjectFormat::Sha256);
        let refs = vec![FsckRef::symbolic("HEAD", "refs/heads/gone")];
        let findings = check_refs(&db, ObjectFormat::Sha256, &refs);
        assert_eq!(
            findings.findings[0].message(),
            format!("error: HEAD: invalid sha256 pointer {}", "0".repeat(64))
        );
    }

    /// Symref cycles terminate without infinite looping.
    #[test]
    fn check_refs_handles_symref_cycle() {
        let db = ObjectDatabase::new(fmt());
        let refs = vec![
            FsckRef::symbolic("refs/a", "refs/b"),
            FsckRef::symbolic("refs/b", "refs/a"),
        ];
        let findings = check_refs(&db, fmt(), &refs);
        // Both refs participate in the cycle and are reported as broken.
        assert_eq!(findings.findings.len(), 2);
        assert!(
            findings
                .findings
                .iter()
                .all(|f| matches!(f, FsckFinding::BrokenSymref { .. }))
        );
    }

    /// The broken-link message reproduces git's exact two-line, width-7-aligned
    /// layout for both a `commit -> tree` and a `tree -> blob` edge.
    #[test]
    fn broken_link_message_matches_git_layout() {
        let source = oid("79e5c3abed1d02fa130914d9d10c3c214d4ef07b");
        let target = oid("6ca2b082c4982a05d9978c0e48bfbae57de44389");
        let commit_to_tree = FsckFinding::BrokenLink {
            source_type: ObjectType::Commit,
            source: source.clone(),
            target_type: ObjectType::Tree,
            target: target.clone(),
        };
        // Exactly what `git fsck` prints: "from" + 2 spaces + "commit",
        // then a newline, 14 spaces, "to", 4 spaces, "tree".
        assert_eq!(
            commit_to_tree.message(),
            format!("broken link from  commit {source}\n              to    tree {target}")
        );

        let tree_to_blob = FsckFinding::BrokenLink {
            source_type: ObjectType::Tree,
            source: target.clone(),
            target_type: ObjectType::Blob,
            target: source.clone(),
        };
        assert_eq!(
            tree_to_blob.message(),
            format!("broken link from    tree {target}\n              to    blob {source}")
        );
    }

    /// `severity`/`is_error` classify each variant the way git treats them.
    #[test]
    fn severity_classification() {
        let id = oid("7777777777777777777777777777777777777777");
        assert!(
            FsckFinding::MissingObject {
                object_type: ObjectType::Blob,
                oid: id.clone(),
            }
            .is_error()
        );
        assert!(
            FsckFinding::CorruptObject {
                oid: id.clone(),
                reason: "x".to_string(),
            }
            .is_error()
        );
        assert!(
            FsckFinding::BadRefTarget {
                refname: "r".to_string(),
                oid: id.clone(),
                hash: "sha1",
            }
            .is_error()
        );
        assert_eq!(
            FsckFinding::Dangling {
                object_type: ObjectType::Blob,
                oid: id.clone(),
            }
            .severity(),
            FsckSeverity::Notice
        );
        assert_eq!(
            FsckFinding::Unreachable {
                object_type: ObjectType::Blob,
                oid: id,
            }
            .severity(),
            FsckSeverity::Notice
        );
    }

    /// `FsckFindings` aggregation helpers behave as documented.
    #[test]
    fn findings_aggregation_helpers() {
        let id = oid("8888888888888888888888888888888888888888");
        let mut findings = FsckFindings::new();
        assert!(findings.is_ok());
        findings.push(FsckFinding::Dangling {
            object_type: ObjectType::Blob,
            oid: id.clone(),
        });
        // A lone notice is still "ok".
        assert!(findings.is_ok());
        assert_eq!(findings.exit_code(), 0);

        let mut more = FsckFindings::new();
        more.push(FsckFinding::MissingObject {
            object_type: ObjectType::Tree,
            oid: id.clone(),
        });
        findings.extend_from(more);
        assert!(!findings.is_ok());
        assert_eq!(findings.exit_code(), 1);
        assert_eq!(findings.errors().count(), 1);
        assert_eq!(findings.notices().count(), 1);
    }

    /// A shared sub-tree referenced by two parents is only walked once.
    #[test]
    fn connectivity_visits_shared_objects_once() {
        let mut db = ObjectDatabase::new(fmt());
        let blob = write_blob(&mut db, b"shared\n");
        let tree = write_tree(
            &mut db,
            vec![TreeEntry {
                mode: 0o100644,
                name: BString::from(b"f"),
                oid: blob,
            }],
        );
        let base = write_commit(&mut db, tree, Vec::new());
        let child_a = write_commit(&mut db, tree, vec![base]);
        let child_b = write_commit(&mut db, tree, vec![base]);

        // Two heads sharing history: no errors, no panics, terminates.
        let findings = check_connectivity(
            &db,
            fmt(),
            &[child_a, child_b],
            &[],
            ConnectivityOptions::default(),
        );
        assert!(findings.is_ok(), "{findings:?}");
    }
}
