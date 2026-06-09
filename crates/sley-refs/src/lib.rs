use sley_config::GitConfig;
use sley_core::{GitError, ObjectFormat, ObjectId, Result};
use sley_formats::{Reftable, ReftableRefRecord, ReftableRefValue};
use std::borrow::Borrow;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::fs;
use std::io::Write;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefTarget {
    Direct(ObjectId),
    Symbolic(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ref {
    pub name: String,
    pub target: RefTarget,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefDelete {
    pub name: String,
    pub oid: ObjectId,
}

pub fn parse_loose_ref(format: ObjectFormat, name: impl Into<String>, bytes: &[u8]) -> Result<Ref> {
    let name = name.into();
    let value = std::str::from_utf8(bytes)
        .map_err(|err| GitError::InvalidFormat(err.to_string()))?
        .trim_end_matches('\n');
    let target = if let Some(symbolic) = value.strip_prefix("ref: ") {
        RefTarget::Symbolic(symbolic.to_string())
    } else {
        RefTarget::Direct(ObjectId::from_hex(format, value)?)
    };
    Ok(Ref { name, target })
}

pub fn write_loose_ref(reference: &Ref) -> Vec<u8> {
    match &reference.target {
        RefTarget::Direct(oid) => format!("{oid}\n").into_bytes(),
        RefTarget::Symbolic(target) => format!("ref: {target}\n").into_bytes(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackedRef {
    pub reference: Ref,
    pub peeled: Option<ObjectId>,
}

pub fn parse_packed_refs(format: ObjectFormat, bytes: &[u8]) -> Result<Vec<PackedRef>> {
    let text =
        std::str::from_utf8(bytes).map_err(|err| GitError::InvalidFormat(err.to_string()))?;
    let mut refs: Vec<PackedRef> = Vec::new();
    for raw_line in text.lines() {
        let line = raw_line.trim_end();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(peeled) = line.strip_prefix('^') {
            let oid = ObjectId::from_hex(format, peeled)?;
            let Some(last) = refs.last_mut() else {
                return Err(GitError::InvalidFormat(
                    "peeled packed ref without preceding ref".into(),
                ));
            };
            last.peeled = Some(oid);
            continue;
        }
        let (oid, name) = line
            .split_once(' ')
            .ok_or_else(|| GitError::InvalidFormat("invalid packed ref line".into()))?;
        validate_ref_name(name)?;
        refs.push(PackedRef {
            reference: Ref {
                name: name.into(),
                target: RefTarget::Direct(ObjectId::from_hex(format, oid)?),
            },
            peeled: None,
        });
    }
    Ok(refs)
}

pub fn write_packed_refs(refs: &[PackedRef]) -> Result<Vec<u8>> {
    let mut refs = refs.to_vec();
    refs.sort_by(|left, right| left.reference.name.cmp(&right.reference.name));
    let mut out = b"# pack-refs with: peeled fully-peeled sorted \n".to_vec();
    for packed in refs {
        validate_ref_name(&packed.reference.name)?;
        let RefTarget::Direct(oid) = &packed.reference.target else {
            return Err(GitError::InvalidFormat(format!(
                "packed ref {} is symbolic",
                packed.reference.name
            )));
        };
        out.extend_from_slice(oid.to_hex().as_bytes());
        out.push(b' ');
        out.extend_from_slice(packed.reference.name.as_bytes());
        out.push(b'\n');
        if let Some(peeled) = packed.peeled {
            out.push(b'^');
            out.extend_from_slice(peeled.to_hex().as_bytes());
            out.push(b'\n');
        }
    }
    Ok(out)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReflogEntry {
    pub old_oid: ObjectId,
    pub new_oid: ObjectId,
    pub committer: Vec<u8>,
    pub message: Vec<u8>,
}

impl ReflogEntry {
    pub fn to_line(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(self.old_oid.to_hex().as_bytes());
        out.push(b' ');
        out.extend_from_slice(self.new_oid.to_hex().as_bytes());
        out.push(b' ');
        out.extend_from_slice(&self.committer);
        if !self.message.is_empty() {
            out.push(b'\t');
            out.extend_from_slice(&self.message);
        }
        out.push(b'\n');
        out
    }

    pub fn timestamp_seconds(&self) -> Result<i64> {
        let committer = std::str::from_utf8(&self.committer)
            .map_err(|err| GitError::InvalidFormat(err.to_string()))?;
        let Some((before_tz, _tz)) = committer.rsplit_once(' ') else {
            return Err(GitError::InvalidFormat(
                "reflog committer is missing timezone".into(),
            ));
        };
        let Some((_identity, timestamp)) = before_tz.rsplit_once(' ') else {
            return Err(GitError::InvalidFormat(
                "reflog committer is missing timestamp".into(),
            ));
        };
        timestamp
            .parse::<i64>()
            .map_err(|err| GitError::InvalidFormat(err.to_string()))
    }
}

pub fn parse_reflog(format: ObjectFormat, bytes: &[u8]) -> Result<Vec<ReflogEntry>> {
    let text =
        std::str::from_utf8(bytes).map_err(|err| GitError::InvalidFormat(err.to_string()))?;
    let mut entries = Vec::new();
    for line in text.lines() {
        let mut parts = line.splitn(3, ' ');
        let old = parts
            .next()
            .ok_or_else(|| GitError::InvalidFormat("missing reflog old oid".into()))?;
        let new = parts
            .next()
            .ok_or_else(|| GitError::InvalidFormat("missing reflog new oid".into()))?;
        let rest = parts
            .next()
            .ok_or_else(|| GitError::InvalidFormat("missing reflog committer".into()))?;
        let (committer, message) = rest.split_once('\t').unwrap_or((rest, ""));
        entries.push(ReflogEntry {
            old_oid: ObjectId::from_hex(format, old)?,
            new_oid: ObjectId::from_hex(format, new)?,
            committer: committer.as_bytes().to_vec(),
            message: message.as_bytes().to_vec(),
        });
    }
    Ok(entries)
}

/// Expire reflog entries, mirroring `git reflog expire` semantics.
///
/// Entries are kept when their committer timestamp is at or after `cutoff_unix`.
/// Entries whose `new_oid` is unreachable (per `is_reachable`) are held to the
/// stricter `expire_unreachable_cutoff` when one is supplied: such an entry is
/// dropped when its timestamp falls below either cutoff. When
/// `expire_unreachable_cutoff` is `None`, reachability does not relax the single
/// `cutoff_unix` bound.
///
/// The most recent entry (the one describing the ref's current value) is always
/// preserved, exactly as git refuses to expire the tip of a reflog, even when it
/// is older than the cutoff. Relative order of the surviving entries is kept.
///
/// This is a pure function over already-parsed entries so callers can read,
/// filter, and rewrite reflogs however they like; see
/// [`FileRefStore::expire_reflog_file`] for a filesystem convenience built on top
/// of it.
pub fn expire_reflog(
    entries: &[ReflogEntry],
    cutoff_unix: i64,
    expire_unreachable_cutoff: Option<i64>,
    is_reachable: impl Fn(&ObjectId) -> bool,
) -> Result<Vec<ReflogEntry>> {
    let last_index = entries.len().checked_sub(1);
    let mut retained = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        // Always keep the most recent entry: it records the current ref value
        // and git never expires it.
        if Some(index) == last_index {
            retained.push(entry.clone());
            continue;
        }
        let timestamp = entry.timestamp_seconds()?;
        let mut expired = timestamp < cutoff_unix;
        if let Some(unreachable_cutoff) = expire_unreachable_cutoff
            && !is_reachable(&entry.new_oid)
        {
            expired = expired || timestamp < unreachable_cutoff;
        }
        if !expired {
            retained.push(entry.clone());
        }
    }
    Ok(retained)
}

#[derive(Debug, Default, Clone)]
pub struct RefStore {
    refs: HashMap<String, RefTarget>,
    reflogs: BTreeMap<String, Vec<ReflogEntry>>,
}

impl RefStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, name: &str) -> Option<&RefTarget> {
        self.refs.get(name)
    }

    pub fn transaction(&mut self) -> RefTransaction<'_> {
        RefTransaction {
            store: self,
            updates: Vec::new(),
        }
    }

    pub fn reflog(&self, name: &str) -> &[ReflogEntry] {
        self.reflogs
            .get(name)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }
}

#[derive(Debug)]
pub struct RefUpdate {
    pub name: String,
    pub expected: Option<RefTarget>,
    pub new: RefTarget,
    pub reflog: Option<ReflogEntry>,
}

/// The compare-and-swap precondition a ref update is checked against (re-verified
/// while the ref is locked, so it is a true CAS, not a check-then-write).
///
/// [`RefUpdate::expected`] can express [`Any`](RefPrecondition::Any) (`None`) and
/// [`MustExistAndMatch`](RefPrecondition::MustExistAndMatch) (`Some`); the
/// create-only and match-or-create modes are reachable via
/// [`FileRefTransaction::update_to`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefPrecondition {
    /// No precondition: create or overwrite unconditionally.
    Any,
    /// The ref must currently exist (with any value).
    MustExist,
    /// The ref must currently not exist (create-only).
    MustNotExist,
    /// The ref must currently exist and point exactly at this target.
    MustExistAndMatch(RefTarget),
    /// If the ref exists it must point exactly at this target; if it is absent,
    /// the update is still allowed (match-or-create).
    ExistingMustMatch(RefTarget),
}

impl RefPrecondition {
    /// The precondition implied by a [`RefUpdate::expected`] value.
    fn from_expected(expected: Option<RefTarget>) -> Self {
        match expected {
            None => Self::Any,
            Some(target) => Self::MustExistAndMatch(target),
        }
    }

    /// Whether `current` — the ref's value right now, or `None` if absent —
    /// satisfies this precondition.
    fn is_satisfied_by(&self, current: Option<&RefTarget>) -> bool {
        match self {
            Self::Any => true,
            Self::MustExist => current.is_some(),
            Self::MustNotExist => current.is_none(),
            Self::MustExistAndMatch(target) => current == Some(target),
            Self::ExistingMustMatch(target) => match current {
                None => true,
                Some(current) => current == target,
            },
        }
    }

    /// A human-readable description of an unmet precondition, for errors.
    fn describe(&self, name: &str) -> String {
        match self {
            Self::Any => format!("ref {name} precondition not met"),
            Self::MustExist => format!("expected ref {name} to exist"),
            Self::MustNotExist => format!("expected ref {name} to not already exist"),
            Self::MustExistAndMatch(_) => format!("expected ref {name} to match"),
            Self::ExistingMustMatch(_) => {
                format!("expected ref {name} to match its current value")
            }
        }
    }
}

pub struct RefTransaction<'a> {
    store: &'a mut RefStore,
    updates: Vec<RefUpdate>,
}

impl<'a> RefTransaction<'a> {
    pub fn update(&mut self, update: RefUpdate) {
        self.updates.push(update);
    }

    pub fn commit(self) -> Result<()> {
        for update in &self.updates {
            if let Some(expected) = &update.expected
                && self.store.refs.get(&update.name) != Some(expected)
            {
                return Err(GitError::Transaction(format!(
                    "expected ref {} to match",
                    update.name
                )));
            }
        }
        for update in self.updates {
            self.store.refs.insert(update.name.clone(), update.new);
            if let Some(entry) = update.reflog {
                self.store
                    .reflogs
                    .entry(update.name)
                    .or_default()
                    .push(entry);
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct FileRefStore {
    git_dir: PathBuf,
    common_dir: PathBuf,
    format: ObjectFormat,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchCreate {
    pub name: String,
    pub oid: ObjectId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchDelete {
    pub name: String,
    pub oid: ObjectId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagCreate {
    pub name: String,
    pub oid: ObjectId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagDelete {
    pub name: String,
    pub oid: ObjectId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleRefUpdate {
    pub name: String,
    pub oid: ObjectId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleRefUpdateReflog {
    pub committer: Vec<u8>,
    pub message: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedBundleRefUpdate {
    pub name: String,
    pub old_oid: Option<ObjectId>,
    pub new_oid: ObjectId,
}

impl FileRefStore {
    pub fn new(git_dir: impl Into<PathBuf>, format: ObjectFormat) -> Self {
        let git_dir = git_dir.into();
        let common_dir = repository_common_dir(&git_dir);
        Self {
            git_dir,
            common_dir,
            format,
        }
    }

    pub fn read_ref(&self, name: &str) -> Result<Option<RefTarget>> {
        validate_ref_name(name)?;
        if self.uses_reftable()? {
            return self.read_reftable_ref(name);
        }
        if let Some(reference) = self.read_loose_ref(name)? {
            return Ok(Some(reference.target));
        }
        if let Some(reference) = self.read_packed_ref(name)? {
            return Ok(Some(reference.reference.target));
        }
        Ok(None)
    }

    pub fn read_reflog(&self, name: &str) -> Result<Vec<ReflogEntry>> {
        validate_ref_name(name)?;
        let path = self.reflog_path(name);
        if !path.exists() {
            return Ok(Vec::new());
        }
        parse_reflog(self.format, &fs::read(path)?)
    }

    pub fn write_reflog(&self, name: &str, entries: &[ReflogEntry]) -> Result<()> {
        validate_ref_name(name)?;
        let path = self.reflog_path(name);
        let parent = path
            .parent()
            .ok_or_else(|| GitError::InvalidPath("reflog path has no parent".into()))?;
        fs::create_dir_all(parent)?;
        let mut bytes = Vec::new();
        for entry in entries {
            bytes.extend_from_slice(&entry.to_line());
        }
        write_locked(&path, &bytes)
    }

    pub fn expire_reflog_older_than(&self, name: &str, cutoff_seconds: i64) -> Result<usize> {
        validate_ref_name(name)?;
        let path = self.reflog_path(name);
        if !path.exists() {
            return Ok(0);
        }
        let entries = parse_reflog(self.format, &fs::read(&path)?)?;
        let original_len = entries.len();
        let mut retained = Vec::new();
        for entry in entries {
            if entry.timestamp_seconds()? >= cutoff_seconds {
                retained.push(entry);
            }
        }
        let mut bytes = Vec::new();
        for entry in &retained {
            bytes.extend_from_slice(&entry.to_line());
        }
        write_locked(&path, &bytes)?;
        Ok(original_len - retained.len())
    }

    /// Read a ref's reflog, expire entries with [`expire_reflog`], and rewrite
    /// the file with the survivors.
    ///
    /// Reachability of each entry's `new_oid` is delegated to `is_reachable` so
    /// the caller can supply whatever object-graph knowledge it has. Rewriting is
    /// opt-in via `write`: when `false` nothing is written and the function only
    /// reports how many entries would be removed (a dry run). When `true` the
    /// reflog is rewritten atomically (lock file + rename) only if at least one
    /// entry was removed; an unchanged reflog is left untouched. Returns the
    /// number of entries removed.
    pub fn expire_reflog_file(
        &self,
        name: &str,
        cutoff_unix: i64,
        expire_unreachable_cutoff: Option<i64>,
        write: bool,
        is_reachable: impl Fn(&ObjectId) -> bool,
    ) -> Result<usize> {
        validate_ref_name(name)?;
        let path = self.reflog_path(name);
        if !path.exists() {
            return Ok(0);
        }
        let entries = parse_reflog(self.format, &fs::read(&path)?)?;
        let original_len = entries.len();
        let retained = expire_reflog(
            &entries,
            cutoff_unix,
            expire_unreachable_cutoff,
            is_reachable,
        )?;
        let removed = original_len - retained.len();
        if write && removed > 0 {
            let mut bytes = Vec::new();
            for entry in &retained {
                bytes.extend_from_slice(&entry.to_line());
            }
            write_locked(&path, &bytes)?;
        }
        Ok(removed)
    }

    pub fn list_refs(&self) -> Result<Vec<Ref>> {
        if self.uses_reftable()? {
            return self.list_reftable_refs();
        }
        let mut refs = BTreeMap::new();
        let packed_path = self.common_dir.join("packed-refs");
        if packed_path.exists() {
            for packed in parse_packed_refs(self.format, &fs::read(packed_path)?)? {
                refs.insert(packed.reference.name.clone(), packed.reference);
            }
        }
        let refs_dir = self.common_dir.join("refs");
        if refs_dir.exists() {
            self.collect_loose_refs(&refs_dir, "refs", &mut refs)?;
        }
        Ok(refs.into_values().collect())
    }

    pub fn write_packed_refs(&self, refs: &[PackedRef]) -> Result<()> {
        write_locked(
            &self.common_dir.join("packed-refs"),
            &write_packed_refs(refs)?,
        )
    }

    pub fn pack_refs(&self, prune_loose: bool) -> Result<Vec<PackedRef>> {
        self.pack_refs_with_peeler(prune_loose, |_, _| Ok(None))
    }

    pub fn pack_refs_with_peeler<F>(&self, prune_loose: bool, mut peel: F) -> Result<Vec<PackedRef>>
    where
        F: FnMut(&str, &ObjectId) -> Result<Option<ObjectId>>,
    {
        let mut packed_refs = BTreeMap::new();
        let packed_path = self.common_dir.join("packed-refs");
        if packed_path.exists() {
            for packed in parse_packed_refs(self.format, &fs::read(&packed_path)?)? {
                packed_refs.insert(packed.reference.name.clone(), packed);
            }
        }

        let mut loose_refs = BTreeMap::new();
        let refs_dir = self.common_dir.join("refs");
        if refs_dir.exists() {
            self.collect_loose_refs(&refs_dir, "refs", &mut loose_refs)?;
        }
        let mut packed_loose_names = Vec::new();
        for reference in loose_refs.into_values() {
            let RefTarget::Direct(oid) = reference.target else {
                continue;
            };
            let peeled = peel(&reference.name, &oid)?;
            packed_loose_names.push(reference.name.clone());
            packed_refs.insert(
                reference.name.clone(),
                PackedRef {
                    reference: Ref {
                        name: reference.name,
                        target: RefTarget::Direct(oid),
                    },
                    peeled,
                },
            );
        }

        let refs = packed_refs.into_values().collect::<Vec<_>>();
        self.write_packed_refs(&refs)?;
        if prune_loose {
            for name in packed_loose_names {
                self.delete_loose_ref(&name)?;
            }
        }
        Ok(refs)
    }

    pub fn current_branch_ref(&self) -> Result<Option<String>> {
        match self.read_ref("HEAD")? {
            Some(RefTarget::Symbolic(name)) if name.starts_with("refs/heads/") => Ok(Some(name)),
            _ => Ok(None),
        }
    }

    pub fn current_branch(&self) -> Result<Option<String>> {
        Ok(self
            .current_branch_ref()?
            .and_then(|name| name.strip_prefix("refs/heads/").map(str::to_string)))
    }

    pub fn transaction(&self) -> FileRefTransaction<'_> {
        FileRefTransaction {
            store: self,
            updates: Vec::new(),
        }
    }

    pub fn create_branch(
        &self,
        branch: &str,
        start: ObjectId,
        committer: Vec<u8>,
        message: Vec<u8>,
    ) -> Result<BranchCreate> {
        let name = branch_ref_name(branch)?;
        if self.read_ref(&name)?.is_some() {
            return Err(GitError::Transaction(format!(
                "branch {branch} already exists"
            )));
        }
        let zero = ObjectId::null(self.format);
        let mut tx = self.transaction();
        tx.update(RefUpdate {
            name: name.clone(),
            expected: None,
            new: RefTarget::Direct(start),
            reflog: Some(ReflogEntry {
                old_oid: zero,
                new_oid: start,
                committer,
                message,
            }),
        });
        tx.commit()?;
        Ok(BranchCreate { name, oid: start })
    }

    pub fn delete_branch(&self, branch: &str) -> Result<BranchDelete> {
        let name = branch_ref_name(branch)?;
        if matches!(self.read_ref("HEAD")?, Some(RefTarget::Symbolic(head)) if head == name) {
            return Err(GitError::Transaction(format!(
                "cannot delete branch {branch} checked out at HEAD"
            )));
        }
        let oid = self.delete_direct_ref(&name, "branch", branch)?;
        let _ = fs::remove_file(self.reflog_path(&name));
        Ok(BranchDelete { name, oid })
    }

    pub fn move_branch(
        &self,
        old_branch: &str,
        new_branch: &str,
        force: bool,
        committer: Vec<u8>,
    ) -> Result<()> {
        self.copy_or_move_branch(old_branch, new_branch, force, false, committer)
    }

    pub fn copy_branch(
        &self,
        old_branch: &str,
        new_branch: &str,
        force: bool,
        committer: Vec<u8>,
    ) -> Result<()> {
        self.copy_or_move_branch(old_branch, new_branch, force, true, committer)
    }

    fn copy_or_move_branch(
        &self,
        old_branch: &str,
        new_branch: &str,
        force: bool,
        copy: bool,
        committer: Vec<u8>,
    ) -> Result<()> {
        let old_name = branch_ref_name(old_branch)?;
        let new_name = branch_ref_name(new_branch)?;
        if old_name == new_name {
            return Ok(());
        }
        let Some(target) = self.read_ref(&old_name)? else {
            return Err(GitError::reference_not_found(format!("branch {old_branch}")));
        };
        let RefTarget::Direct(oid) = target else {
            return Err(GitError::InvalidFormat(format!(
                "branch {old_branch} is symbolic"
            )));
        };
        if self.read_ref(&new_name)?.is_some() {
            if !force {
                return Err(GitError::Transaction(format!(
                    "branch {new_branch} already exists"
                )));
            }
            let _ = self.delete_direct_ref(&new_name, "branch", new_branch)?;
            let _ = fs::remove_file(self.reflog_path(&new_name));
        }

        self.write_loose_ref(&Ref {
            name: new_name.clone(),
            target: RefTarget::Direct(oid),
        })?;
        let mut reflog = self.read_reflog(&old_name)?;
        reflog.push(ReflogEntry {
            old_oid: oid,
            new_oid: oid,
            committer,
            message: if copy {
                format!("Branch: copied {old_name} to {new_name}").into_bytes()
            } else {
                format!("Branch: renamed {old_name} to {new_name}").into_bytes()
            },
        });
        self.write_reflog(&new_name, &reflog)?;

        if !copy {
            let _ = self.delete_direct_ref(&old_name, "branch", old_branch)?;
            let _ = fs::remove_file(self.reflog_path(&old_name));
            if matches!(self.read_ref("HEAD")?, Some(RefTarget::Symbolic(head)) if head == old_name)
            {
                self.write_loose_ref(&Ref {
                    name: "HEAD".into(),
                    target: RefTarget::Symbolic(new_name),
                })?;
            }
        }
        Ok(())
    }

    pub fn create_tag(&self, tag: &str, target: ObjectId) -> Result<TagCreate> {
        let name = tag_ref_name(tag)?;
        if self.read_ref(&name)?.is_some() {
            return Err(GitError::Transaction(format!("tag {tag} already exists")));
        }
        let mut tx = self.transaction();
        tx.update(RefUpdate {
            name: name.clone(),
            expected: None,
            new: RefTarget::Direct(target),
            reflog: None,
        });
        tx.commit()?;
        Ok(TagCreate { name, oid: target })
    }

    pub fn apply_bundle_ref_updates(
        &self,
        refs: &[BundleRefUpdate],
        reflog: Option<BundleRefUpdateReflog>,
    ) -> Result<Vec<AppliedBundleRefUpdate>> {
        let (updates, applied) = prepare_bundle_ref_updates(refs, reflog.as_ref(), |name, oid| {
            if oid.format() != self.format {
                return Err(GitError::InvalidObjectId(format!(
                    "bundle ref {name} has {} object id for {} repository",
                    oid.format().name(),
                    self.format.name()
                )));
            }
            self.read_ref(name)
        })?;
        let mut tx = self.transaction();
        for update in updates {
            tx.update(update);
        }
        tx.commit()?;
        Ok(applied)
    }

    pub fn delete_tag(&self, tag: &str) -> Result<TagDelete> {
        let name = tag_ref_name(tag)?;
        let oid = self.delete_direct_ref(&name, "tag", tag)?;
        Ok(TagDelete { name, oid })
    }

    pub fn delete_ref(&self, name: &str) -> Result<RefDelete> {
        validate_ref_name(name)?;
        let oid = self.delete_direct_ref(name, "ref", name)?;
        let _ = fs::remove_file(self.reflog_path(name));
        Ok(RefDelete {
            name: name.into(),
            oid,
        })
    }

    pub fn delete_symbolic_ref(&self, name: &str) -> Result<bool> {
        validate_ref_name(name)?;
        if self.uses_reftable()? {
            let Some(target) = self.read_ref(name)? else {
                return Ok(false);
            };
            if !matches!(target, RefTarget::Symbolic(_)) {
                return Ok(false);
            }
            self.append_reftable_records(vec![ReftableRefRecord {
                name: name.to_string(),
                update_index: 0,
                value: ReftableRefValue::Deletion,
            }])?;
            let _ = fs::remove_file(self.reflog_path(name));
            return Ok(true);
        }
        let Some(reference) = self.read_loose_ref(name)? else {
            return Ok(false);
        };
        if !matches!(reference.target, RefTarget::Symbolic(_)) {
            return Ok(false);
        }
        self.delete_loose_ref(name)?;
        let _ = fs::remove_file(self.reflog_path(name));
        Ok(true)
    }

    fn delete_direct_ref(&self, name: &str, kind: &str, short_name: &str) -> Result<ObjectId> {
        if self.uses_reftable()? {
            let Some(target) = self.read_ref(name)? else {
                return Err(GitError::reference_not_found(format!("{kind} {short_name}")));
            };
            let RefTarget::Direct(oid) = target else {
                return Err(GitError::InvalidFormat(format!(
                    "{kind} {short_name} is symbolic"
                )));
            };
            self.append_reftable_records(vec![ReftableRefRecord {
                name: name.to_string(),
                update_index: 0,
                value: ReftableRefValue::Deletion,
            }])?;
            return Ok(oid);
        }
        let Some(reference) = self.read_loose_ref(name)? else {
            return self.delete_packed_ref(name, kind, short_name);
        };
        let oid = match reference.target {
            RefTarget::Direct(oid) => oid,
            RefTarget::Symbolic(target) => {
                return Err(GitError::InvalidFormat(format!(
                    "{kind} {short_name} is symbolic to {target}"
                )));
            }
        };
        self.delete_loose_ref(name)?;
        Ok(oid)
    }

    fn delete_packed_ref(&self, name: &str, kind: &str, short_name: &str) -> Result<ObjectId> {
        let path = self.common_dir.join("packed-refs");
        if !path.exists() {
            return Err(GitError::reference_not_found(format!("{kind} {short_name}")));
        }
        let mut refs = parse_packed_refs(self.format, &fs::read(&path)?)?;
        let Some(index) = refs
            .iter()
            .position(|reference| reference.reference.name == name)
        else {
            return Err(GitError::reference_not_found(format!("{kind} {short_name}")));
        };
        let removed = refs.remove(index);
        let RefTarget::Direct(oid) = removed.reference.target else {
            return Err(GitError::InvalidFormat(format!(
                "{kind} {short_name} is symbolic"
            )));
        };
        self.write_packed_refs(&refs)?;
        Ok(oid)
    }

    fn read_loose_ref(&self, name: &str) -> Result<Option<Ref>> {
        let path = self.ref_path(name);
        if !path.exists() {
            return Ok(None);
        }
        Ok(Some(parse_loose_ref(self.format, name, &fs::read(path)?)?))
    }

    fn read_packed_ref(&self, name: &str) -> Result<Option<PackedRef>> {
        let path = self.common_dir.join("packed-refs");
        if !path.exists() {
            return Ok(None);
        }
        Ok(parse_packed_refs(self.format, &fs::read(path)?)?
            .into_iter()
            .find(|reference| reference.reference.name == name))
    }

    fn read_reftable_ref(&self, name: &str) -> Result<Option<RefTarget>> {
        for table in self.reftables()?.into_iter().rev() {
            if let Some(record) = table.refs.into_iter().find(|record| record.name == name) {
                return reftable_ref_target(record.value);
            }
        }
        Ok(None)
    }

    fn list_reftable_refs(&self) -> Result<Vec<Ref>> {
        let mut refs = BTreeMap::<String, Ref>::new();
        for table in self.reftables()? {
            for record in table.refs {
                if !record.name.starts_with("refs/") {
                    continue;
                }
                match reftable_ref_target(record.value)? {
                    Some(target) => {
                        refs.insert(
                            record.name.clone(),
                            Ref {
                                name: record.name,
                                target,
                            },
                        );
                    }
                    None => {
                        refs.remove(&record.name);
                    }
                }
            }
        }
        Ok(refs.into_values().collect())
    }

    fn reftables(&self) -> Result<Vec<Reftable>> {
        let reftable_dir = self.common_dir.join("reftable");
        let tables_list = reftable_dir.join("tables.list");
        if !tables_list.exists() {
            return Ok(Vec::new());
        }
        let text = fs::read_to_string(&tables_list)?;
        let mut tables = Vec::new();
        for raw_line in text.lines() {
            let line = raw_line.trim();
            if line.is_empty() {
                continue;
            }
            if line.contains('/')
                || line.contains('\\')
                || Path::new(line).components().count() != 1
            {
                return Err(GitError::InvalidPath(format!(
                    "invalid reftable table name {line}"
                )));
            }
            let table = Reftable::parse(&fs::read(reftable_dir.join(line))?)?;
            if table.header.object_format != self.format {
                return Err(GitError::InvalidFormat(format!(
                    "reftable {line} has {} object ids in {} repository",
                    table.header.object_format.name(),
                    self.format.name()
                )));
            }
            tables.push(table);
        }
        Ok(tables)
    }

    fn uses_reftable(&self) -> Result<bool> {
        let config_path = self.common_dir.join("config");
        if !config_path.exists() {
            return Ok(false);
        }
        let config = GitConfig::parse(&fs::read(config_path)?)?;
        Ok(matches!(
            config.get("extensions", None, "refStorage"),
            Some(value) if value.eq_ignore_ascii_case("reftable")
        ))
    }

    fn append_reftable_records(&self, mut records: Vec<ReftableRefRecord>) -> Result<()> {
        if records.is_empty() {
            return Ok(());
        }
        let reftable_dir = self.common_dir.join("reftable");
        fs::create_dir_all(&reftable_dir)?;
        let tables_list = reftable_dir.join("tables.list");
        let mut table_names = if tables_list.exists() {
            fs::read_to_string(&tables_list)?
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let update_index = self.next_reftable_update_index(&table_names)?;
        for record in &mut records {
            record.update_index = update_index;
        }
        let table_name = reftable_table_name(update_index);
        let bytes = Reftable::write_ref_only(self.format, update_index, update_index, &records)?;
        write_locked(&reftable_dir.join(&table_name), &bytes)?;
        table_names.push(table_name);
        let mut list = Vec::new();
        for name in &table_names {
            list.extend_from_slice(name.as_bytes());
            list.push(b'\n');
        }
        write_locked(&tables_list, &list)
    }

    fn next_reftable_update_index(&self, table_names: &[String]) -> Result<u64> {
        let reftable_dir = self.common_dir.join("reftable");
        let mut max_update_index = 0;
        for name in table_names {
            let table = Reftable::parse(&fs::read(reftable_dir.join(name))?)?;
            max_update_index = max_update_index.max(table.header.max_update_index);
        }
        max_update_index
            .checked_add(1)
            .ok_or_else(|| GitError::InvalidFormat("reftable update index overflow".into()))
    }

    fn collect_loose_refs(
        &self,
        dir: &Path,
        prefix: &str,
        refs: &mut BTreeMap<String, Ref>,
    ) -> Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let name = format!("{prefix}/{}", entry.file_name().to_string_lossy());
            if path.is_dir() {
                self.collect_loose_refs(&path, &name, refs)?;
            } else if !name.ends_with(".lock") {
                let reference = parse_loose_ref(self.format, name.clone(), &fs::read(path)?)?;
                refs.insert(name, reference);
            }
        }
        Ok(())
    }

    fn write_loose_ref(&self, reference: &Ref) -> Result<()> {
        if self.uses_reftable()? {
            self.append_reftable_records(vec![ReftableRefRecord {
                name: reference.name.clone(),
                update_index: 0,
                value: reftable_value_from_ref_target(&reference.target),
            }])?;
            return Ok(());
        }
        let path = self.ref_path(&reference.name);
        let parent = path
            .parent()
            .ok_or_else(|| GitError::InvalidPath("ref path has no parent".into()))?;
        fs::create_dir_all(parent)?;
        write_locked(&path, &write_loose_ref(reference))
    }

    fn delete_loose_ref(&self, name: &str) -> Result<()> {
        let path = self.ref_path(name);
        let lock_path = lock_path_for(&path)?;
        {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&lock_path)?;
            file.write_all(b"delete\n")?;
            file.sync_all()?;
        }
        match fs::remove_file(&path) {
            Ok(()) => {
                fs::remove_file(lock_path)?;
                Ok(())
            }
            Err(err) => {
                let _ = fs::remove_file(lock_path);
                Err(GitError::Io(err.to_string()))
            }
        }
    }

    pub fn append_reflog(&self, name: &str, entry: &ReflogEntry) -> Result<()> {
        validate_ref_name(name)?;
        let path = self.reflog_path(name);
        let parent = path
            .parent()
            .ok_or_else(|| GitError::InvalidPath("reflog path has no parent".into()))?;
        fs::create_dir_all(parent)?;
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        file.write_all(&entry.to_line())?;
        file.sync_all()?;
        Ok(())
    }

    fn ref_path(&self, name: &str) -> PathBuf {
        self.ref_base_dir(name).join(name)
    }

    fn reflog_path(&self, name: &str) -> PathBuf {
        self.ref_base_dir(name).join("logs").join(name)
    }

    fn ref_base_dir(&self, name: &str) -> &Path {
        if name == "HEAD" {
            &self.git_dir
        } else {
            &self.common_dir
        }
    }
}

fn reftable_ref_target(value: ReftableRefValue) -> Result<Option<RefTarget>> {
    match value {
        ReftableRefValue::Deletion => Ok(None),
        ReftableRefValue::Direct(oid) | ReftableRefValue::Peeled { target: oid, .. } => {
            Ok(Some(RefTarget::Direct(oid)))
        }
        ReftableRefValue::Symbolic(target) => {
            validate_ref_name(&target)?;
            Ok(Some(RefTarget::Symbolic(target)))
        }
    }
}

fn reftable_value_from_ref_target(target: &RefTarget) -> ReftableRefValue {
    match target {
        RefTarget::Direct(oid) => ReftableRefValue::Direct(*oid),
        RefTarget::Symbolic(target) => ReftableRefValue::Symbolic(target.clone()),
    }
}

fn reftable_table_name(update_index: u64) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("0x{update_index:012x}-0x{update_index:012x}-sley-{nanos:x}.ref")
}

fn repository_common_dir(git_dir: &Path) -> PathBuf {
    if let Some(common_dir) = std::env::var_os("GIT_COMMON_DIR") {
        return PathBuf::from(common_dir);
    }
    let commondir = git_dir.join("commondir");
    if let Ok(value) = fs::read_to_string(&commondir) {
        let path = PathBuf::from(value.trim());
        let common = if path.is_absolute() {
            path
        } else {
            git_dir.join(path)
        };
        return fs::canonicalize(&common).unwrap_or(common);
    }
    git_dir.to_path_buf()
}

pub struct FileRefTransaction<'a> {
    store: &'a FileRefStore,
    updates: Vec<QueuedUpdate>,
}

/// One queued change inside a [`FileRefTransaction`], carrying the
/// compare-and-swap precondition to enforce under lock.
struct QueuedUpdate {
    name: String,
    precondition: RefPrecondition,
    new: RefTarget,
    reflog: Option<ReflogEntry>,
}

impl<'a> FileRefTransaction<'a> {
    /// Queue a ref update whose precondition comes from [`RefUpdate::expected`]
    /// (`None` = no check; `Some(target)` = the ref must currently match
    /// `target`). For create-only or match-or-create semantics use
    /// [`update_to`](FileRefTransaction::update_to).
    pub fn update(&mut self, update: RefUpdate) {
        self.updates.push(QueuedUpdate {
            name: update.name,
            precondition: RefPrecondition::from_expected(update.expected),
            new: update.new,
            reflog: update.reflog,
        });
    }

    /// Queue a ref update with an explicit compare-and-swap [`RefPrecondition`]
    /// (e.g. [`MustNotExist`](RefPrecondition::MustNotExist) for create-only, or
    /// [`ExistingMustMatch`](RefPrecondition::ExistingMustMatch) for
    /// match-or-create). The precondition is re-verified while the ref is
    /// locked.
    pub fn update_to(
        &mut self,
        name: impl Into<String>,
        new: RefTarget,
        precondition: RefPrecondition,
        reflog: Option<ReflogEntry>,
    ) {
        self.updates.push(QueuedUpdate {
            name: name.into(),
            precondition,
            new,
            reflog,
        });
    }

    /// Commit all queued updates atomically and durably.
    ///
    /// All updates succeed together or none take effect. For the loose-ref
    /// backend the sequence is:
    ///
    /// 1. Coalesce updates that target the same ref so each ref is touched once
    ///    (the final `new` value wins, reflog entries accumulate in order, and
    ///    the precondition from the first queued update is enforced).
    /// 2. Take an exclusive `<ref>.lock` file for every ref up front. Acquiring a
    ///    lock fails if another writer already holds it, guaranteeing isolation.
    /// 3. Re-verify every precondition *while holding the locks*, closing
    ///    the check-then-write race that a pre-lock verification would leave open.
    /// 4. Write each new value into its lock file and `fsync` it.
    /// 5. Atomically `rename` each lock file over the real ref.
    ///
    /// If any step fails, every ref already renamed in this commit is restored to
    /// the exact bytes it held beforehand (or removed if it did not exist), and
    /// all outstanding lock files are deleted, so the repository is left exactly
    /// as it was. Reflog entries are appended only after every ref update has
    /// landed.
    pub fn commit(self) -> Result<()> {
        let FileRefTransaction { store, updates } = self;
        let updates = coalesce_ref_updates(updates)?;
        for update in &updates {
            if !matches!(update.precondition, RefPrecondition::Any) {
                let current = store.read_ref(&update.name)?;
                if !update.precondition.is_satisfied_by(current.as_ref()) {
                    return Err(GitError::Transaction(
                        update.precondition.describe(&update.name),
                    ));
                }
            }
        }
        if store.uses_reftable()? {
            let mut records = Vec::with_capacity(updates.len());
            let mut reflogs = Vec::new();
            for update in updates {
                records.push(ReftableRefRecord {
                    name: update.name.clone(),
                    update_index: 0,
                    value: reftable_value_from_ref_target(&update.new),
                });
                for entry in update.reflog {
                    reflogs.push((update.name.clone(), entry));
                }
            }
            store.append_reftable_records(records)?;
            for (name, entry) in reflogs {
                store.append_reflog(&name, &entry)?;
            }
            return Ok(());
        }
        store.commit_loose(updates)
    }
}

impl FileRefStore {
    /// Atomic, all-or-nothing commit for the loose-ref backend. See
    /// [`FileRefTransaction::commit`] for the full ordering and rollback rules.
    fn commit_loose(&self, updates: Vec<CoalescedRefUpdate>) -> Result<()> {
        let mut pending = Vec::with_capacity(updates.len());
        // Acquire every lock first; bail (releasing what we hold) on any failure.
        for update in &updates {
            let path = self.ref_path(&update.name);
            let parent = path
                .parent()
                .ok_or_else(|| GitError::InvalidPath("ref path has no parent".into()))?;
            if let Err(err) = fs::create_dir_all(parent) {
                release_pending_locks(&pending);
                return Err(GitError::Io(err.to_string()));
            }
            let lock_path = match lock_path_for(&path) {
                Ok(lock_path) => lock_path,
                Err(err) => {
                    release_pending_locks(&pending);
                    return Err(err);
                }
            };
            if let Err(err) = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&lock_path)
            {
                release_pending_locks(&pending);
                return Err(GitError::Io(format!(
                    "could not lock ref {}: {err}",
                    update.name
                )));
            }
            pending.push(PendingRefWrite {
                path,
                lock_path,
                contents: write_loose_ref(&Ref {
                    name: update.name.clone(),
                    target: update.new.clone(),
                }),
            });
        }

        // Verify expectations under lock, then capture prior on-disk state for
        // rollback. read_ref consults loose then packed refs, matching the
        // pre-lock check, so a concurrent change is caught here.
        let mut originals = Vec::with_capacity(updates.len());
        for (update, write) in updates.iter().zip(&pending) {
            if !matches!(update.precondition, RefPrecondition::Any) {
                match self.read_ref(&update.name) {
                    Ok(current) if update.precondition.is_satisfied_by(current.as_ref()) => {}
                    Ok(_) => {
                        release_pending_locks(&pending);
                        return Err(GitError::Transaction(
                            update.precondition.describe(&update.name),
                        ));
                    }
                    Err(err) => {
                        release_pending_locks(&pending);
                        return Err(err);
                    }
                }
            }
            match read_optional_file(&write.path) {
                Ok(original) => originals.push(original),
                Err(err) => {
                    release_pending_locks(&pending);
                    return Err(err);
                }
            }
        }

        // Stage every new value into its lock file. Nothing has been renamed
        // yet, so on failure we only need to drop the lock files.
        for write in &pending {
            if let Err(err) = stage_lock_file(&write.lock_path, &write.contents) {
                release_pending_locks(&pending);
                return Err(err);
            }
        }

        // Atomically swap each lock file into place; on failure restore the refs
        // already renamed and drop the remaining locks.
        for index in 0..pending.len() {
            if let Err(err) = fs::rename(&pending[index].lock_path, &pending[index].path) {
                rollback_after_rename(&pending, &originals, index);
                return Err(GitError::Io(err.to_string()));
            }
        }

        // All refs are durable; append reflogs last, matching git's ordering.
        for update in updates {
            for entry in update.reflog {
                self.append_reflog(&update.name, &entry)?;
            }
        }
        Ok(())
    }
}

/// A ref update with all writes that targeted the same name folded together.
struct CoalescedRefUpdate {
    name: String,
    precondition: RefPrecondition,
    new: RefTarget,
    reflog: Vec<ReflogEntry>,
}

/// Fold repeated updates to the same ref into one, preserving first-seen order.
/// The last queued value wins, reflog entries accumulate in order, and the
/// precondition is taken from the first update (the state the caller
/// asserted before any change in this transaction).
fn coalesce_ref_updates(updates: Vec<QueuedUpdate>) -> Result<Vec<CoalescedRefUpdate>> {
    let mut order: Vec<String> = Vec::new();
    let mut by_name: HashMap<String, CoalescedRefUpdate> = HashMap::new();
    for update in updates {
        validate_ref_name(&update.name)?;
        match by_name.get_mut(&update.name) {
            Some(existing) => {
                existing.new = update.new;
                if let Some(entry) = update.reflog {
                    existing.reflog.push(entry);
                }
            }
            None => {
                order.push(update.name.clone());
                by_name.insert(
                    update.name.clone(),
                    CoalescedRefUpdate {
                        name: update.name,
                        precondition: update.precondition,
                        new: update.new,
                        reflog: update.reflog.into_iter().collect(),
                    },
                );
            }
        }
    }
    let mut coalesced = Vec::with_capacity(order.len());
    for name in order {
        if let Some(update) = by_name.remove(&name) {
            coalesced.push(update);
        }
    }
    Ok(coalesced)
}

/// A staged loose-ref write: the target ref path, its lock file, and the bytes
/// to install.
struct PendingRefWrite {
    path: PathBuf,
    lock_path: PathBuf,
    contents: Vec<u8>,
}

fn read_optional_file(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(GitError::Io(err.to_string())),
    }
}

fn stage_lock_file(lock_path: &Path, contents: &[u8]) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(lock_path)?;
    file.write_all(contents)?;
    file.sync_all()?;
    Ok(())
}

/// Delete every still-held lock file. Used when a transaction aborts before any
/// rename, so nothing on disk has changed yet.
fn release_pending_locks(pending: &[PendingRefWrite]) {
    for write in pending {
        let _ = fs::remove_file(&write.lock_path);
    }
}

/// Roll back after `renamed` refs have already been swapped into place: restore
/// each to its captured bytes (or remove it if it did not previously exist),
/// then drop the lock files that have not yet been renamed.
fn rollback_after_rename(
    pending: &[PendingRefWrite],
    originals: &[Option<Vec<u8>>],
    renamed: usize,
) {
    for index in 0..renamed {
        match &originals[index] {
            Some(bytes) => {
                let _ = restore_file_atomically(&pending[index].path, bytes);
            }
            None => {
                let _ = fs::remove_file(&pending[index].path);
            }
        }
    }
    for write in pending.iter().skip(renamed) {
        let _ = fs::remove_file(&write.lock_path);
    }
}

/// Best-effort atomic restore of `path` to `bytes` during rollback, reusing the
/// write-to-temp-then-rename dance so a crash mid-rollback cannot truncate a ref.
fn restore_file_atomically(path: &Path, bytes: &[u8]) -> Result<()> {
    write_locked(path, bytes)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FullRefName<'a> {
    name: &'a str,
}

impl<'a> FullRefName<'a> {
    pub fn new(name: &'a str) -> Result<Self> {
        validate_ref_name(name)?;
        Ok(Self { name })
    }

    pub fn as_str(&self) -> &str {
        self.name
    }

    pub fn into_str(self) -> &'a str {
        self.name
    }

    pub fn to_owned(&self) -> FullRefNameBuf {
        FullRefNameBuf {
            name: self.name.to_string(),
        }
    }

    pub fn as_branch(&self) -> Result<BranchRefName<'a>> {
        BranchRefName::from_full_ref(*self)
    }

    pub fn as_tag(&self) -> Result<TagRefName<'a>> {
        TagRefName::from_full_ref(*self)
    }

    pub fn as_remote(&self) -> Result<RemoteRefName<'a>> {
        RemoteRefName::from_full_ref(*self)
    }
}

impl AsRef<str> for FullRefName<'_> {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for FullRefName<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FullRefNameBuf {
    name: String,
}

impl FullRefNameBuf {
    pub fn new(name: impl Into<String>) -> Result<Self> {
        let name = name.into();
        validate_ref_name(&name)?;
        Ok(Self { name })
    }

    pub fn as_ref_name(&self) -> FullRefName<'_> {
        FullRefName { name: &self.name }
    }

    pub fn as_str(&self) -> &str {
        &self.name
    }

    pub fn into_string(self) -> String {
        self.name
    }

    pub fn as_branch(&self) -> Result<BranchRefName<'_>> {
        self.as_ref_name().as_branch()
    }

    pub fn as_tag(&self) -> Result<TagRefName<'_>> {
        self.as_ref_name().as_tag()
    }

    pub fn as_remote(&self) -> Result<RemoteRefName<'_>> {
        self.as_ref_name().as_remote()
    }
}

impl AsRef<str> for FullRefNameBuf {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl Borrow<str> for FullRefNameBuf {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl Deref for FullRefNameBuf {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.as_str()
    }
}

impl fmt::Display for FullRefNameBuf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BranchRefName<'a> {
    name: &'a str,
}

impl<'a> BranchRefName<'a> {
    pub const PREFIX: &'static str = "refs/heads/";

    pub fn from_full(name: &'a str) -> Result<Self> {
        let full = FullRefName::new(name)?;
        Self::from_full_ref(full)
    }

    pub fn from_full_ref(name: FullRefName<'a>) -> Result<Self> {
        validate_namespaced_ref(name.as_str(), Self::PREFIX, "branch")?;
        Ok(Self {
            name: name.into_str(),
        })
    }

    pub fn as_full_ref_name(&self) -> FullRefName<'a> {
        FullRefName { name: self.name }
    }

    pub fn as_str(&self) -> &str {
        self.name
    }

    pub fn branch_name(&self) -> &str {
        self.short_name()
    }

    pub fn short_name(&self) -> &str {
        &self.name[Self::PREFIX.len()..]
    }

    pub fn into_str(self) -> &'a str {
        self.name
    }

    pub fn to_owned(&self) -> BranchRefNameBuf {
        BranchRefNameBuf {
            name: self.name.to_string(),
        }
    }
}

impl AsRef<str> for BranchRefName<'_> {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for BranchRefName<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl<'a> From<BranchRefName<'a>> for FullRefName<'a> {
    fn from(name: BranchRefName<'a>) -> Self {
        name.as_full_ref_name()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BranchRefNameBuf {
    name: String,
}

impl BranchRefNameBuf {
    pub fn from_branch_name(branch: &str) -> Result<Self> {
        validate_short_ref_name("branch", branch)?;
        let name = format!("{}{}", BranchRefName::PREFIX, branch);
        Self::from_full(name)
    }

    pub fn from_full(name: impl Into<String>) -> Result<Self> {
        let name = name.into();
        BranchRefName::from_full(&name)?;
        Ok(Self { name })
    }

    pub fn as_ref_name(&self) -> BranchRefName<'_> {
        BranchRefName { name: &self.name }
    }

    pub fn as_full_ref_name(&self) -> FullRefName<'_> {
        FullRefName { name: &self.name }
    }

    pub fn as_str(&self) -> &str {
        &self.name
    }

    pub fn branch_name(&self) -> &str {
        self.short_name()
    }

    pub fn short_name(&self) -> &str {
        &self.name[BranchRefName::PREFIX.len()..]
    }

    pub fn into_string(self) -> String {
        self.name
    }
}

impl AsRef<str> for BranchRefNameBuf {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl Borrow<str> for BranchRefNameBuf {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl Deref for BranchRefNameBuf {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.as_str()
    }
}

impl fmt::Display for BranchRefNameBuf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl From<BranchRefNameBuf> for FullRefNameBuf {
    fn from(name: BranchRefNameBuf) -> Self {
        Self { name: name.name }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TagRefName<'a> {
    name: &'a str,
}

impl<'a> TagRefName<'a> {
    pub const PREFIX: &'static str = "refs/tags/";

    pub fn from_full(name: &'a str) -> Result<Self> {
        let full = FullRefName::new(name)?;
        Self::from_full_ref(full)
    }

    pub fn from_full_ref(name: FullRefName<'a>) -> Result<Self> {
        validate_namespaced_ref(name.as_str(), Self::PREFIX, "tag")?;
        Ok(Self {
            name: name.into_str(),
        })
    }

    pub fn as_full_ref_name(&self) -> FullRefName<'a> {
        FullRefName { name: self.name }
    }

    pub fn as_str(&self) -> &str {
        self.name
    }

    pub fn tag_name(&self) -> &str {
        self.short_name()
    }

    pub fn short_name(&self) -> &str {
        &self.name[Self::PREFIX.len()..]
    }

    pub fn into_str(self) -> &'a str {
        self.name
    }

    pub fn to_owned(&self) -> TagRefNameBuf {
        TagRefNameBuf {
            name: self.name.to_string(),
        }
    }
}

impl AsRef<str> for TagRefName<'_> {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for TagRefName<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl<'a> From<TagRefName<'a>> for FullRefName<'a> {
    fn from(name: TagRefName<'a>) -> Self {
        name.as_full_ref_name()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TagRefNameBuf {
    name: String,
}

impl TagRefNameBuf {
    pub fn from_tag_name(tag: &str) -> Result<Self> {
        validate_short_ref_name("tag", tag)?;
        let name = format!("{}{}", TagRefName::PREFIX, tag);
        Self::from_full(name)
    }

    pub fn from_full(name: impl Into<String>) -> Result<Self> {
        let name = name.into();
        TagRefName::from_full(&name)?;
        Ok(Self { name })
    }

    pub fn as_ref_name(&self) -> TagRefName<'_> {
        TagRefName { name: &self.name }
    }

    pub fn as_full_ref_name(&self) -> FullRefName<'_> {
        FullRefName { name: &self.name }
    }

    pub fn as_str(&self) -> &str {
        &self.name
    }

    pub fn tag_name(&self) -> &str {
        self.short_name()
    }

    pub fn short_name(&self) -> &str {
        &self.name[TagRefName::PREFIX.len()..]
    }

    pub fn into_string(self) -> String {
        self.name
    }
}

impl AsRef<str> for TagRefNameBuf {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl Borrow<str> for TagRefNameBuf {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl Deref for TagRefNameBuf {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.as_str()
    }
}

impl fmt::Display for TagRefNameBuf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl From<TagRefNameBuf> for FullRefNameBuf {
    fn from(name: TagRefNameBuf) -> Self {
        Self { name: name.name }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RemoteRefName<'a> {
    name: &'a str,
}

impl<'a> RemoteRefName<'a> {
    pub const PREFIX: &'static str = "refs/remotes/";

    pub fn from_full(name: &'a str) -> Result<Self> {
        let full = FullRefName::new(name)?;
        Self::from_full_ref(full)
    }

    pub fn from_full_ref(name: FullRefName<'a>) -> Result<Self> {
        validate_namespaced_ref(name.as_str(), Self::PREFIX, "remote")?;
        Ok(Self {
            name: name.into_str(),
        })
    }

    pub fn as_full_ref_name(&self) -> FullRefName<'a> {
        FullRefName { name: self.name }
    }

    pub fn as_str(&self) -> &str {
        self.name
    }

    pub fn short_name(&self) -> &str {
        &self.name[Self::PREFIX.len()..]
    }

    pub fn remote_name(&self) -> &str {
        match self.short_name().split_once('/') {
            Some((remote, _branch)) => remote,
            None => self.short_name(),
        }
    }

    pub fn remote_branch(&self) -> Option<&str> {
        self.short_name()
            .split_once('/')
            .map(|(_remote, branch)| branch)
    }

    pub fn into_str(self) -> &'a str {
        self.name
    }

    pub fn to_owned(&self) -> RemoteRefNameBuf {
        RemoteRefNameBuf {
            name: self.name.to_string(),
        }
    }
}

impl AsRef<str> for RemoteRefName<'_> {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for RemoteRefName<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl<'a> From<RemoteRefName<'a>> for FullRefName<'a> {
    fn from(name: RemoteRefName<'a>) -> Self {
        name.as_full_ref_name()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RemoteRefNameBuf {
    name: String,
}

impl RemoteRefNameBuf {
    pub fn from_short_name(name: &str) -> Result<Self> {
        validate_short_ref_name("remote ref", name)?;
        let name = format!("{}{}", RemoteRefName::PREFIX, name);
        Self::from_full(name)
    }

    pub fn from_remote_branch(remote: &str, branch: &str) -> Result<Self> {
        validate_remote_name(remote)?;
        validate_short_ref_name("remote branch", branch)?;
        let name = format!("{}{}/{}", RemoteRefName::PREFIX, remote, branch);
        Self::from_full(name)
    }

    pub fn from_full(name: impl Into<String>) -> Result<Self> {
        let name = name.into();
        RemoteRefName::from_full(&name)?;
        Ok(Self { name })
    }

    pub fn as_ref_name(&self) -> RemoteRefName<'_> {
        RemoteRefName { name: &self.name }
    }

    pub fn as_full_ref_name(&self) -> FullRefName<'_> {
        FullRefName { name: &self.name }
    }

    pub fn as_str(&self) -> &str {
        &self.name
    }

    pub fn short_name(&self) -> &str {
        &self.name[RemoteRefName::PREFIX.len()..]
    }

    pub fn remote_name(&self) -> &str {
        match self.short_name().split_once('/') {
            Some((remote, _branch)) => remote,
            None => self.short_name(),
        }
    }

    pub fn remote_branch(&self) -> Option<&str> {
        self.short_name()
            .split_once('/')
            .map(|(_remote, branch)| branch)
    }

    pub fn into_string(self) -> String {
        self.name
    }
}

impl AsRef<str> for RemoteRefNameBuf {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl Borrow<str> for RemoteRefNameBuf {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl Deref for RemoteRefNameBuf {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.as_str()
    }
}

impl fmt::Display for RemoteRefNameBuf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl From<RemoteRefNameBuf> for FullRefNameBuf {
    fn from(name: RemoteRefNameBuf) -> Self {
        Self { name: name.name }
    }
}

pub fn branch_ref_name(branch: &str) -> Result<String> {
    BranchRefNameBuf::from_branch_name(branch).map(BranchRefNameBuf::into_string)
}

pub fn tag_ref_name(tag: &str) -> Result<String> {
    TagRefNameBuf::from_tag_name(tag).map(TagRefNameBuf::into_string)
}

fn write_locked(path: &Path, bytes: &[u8]) -> Result<()> {
    let lock_path = lock_path_for(path)?;
    {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    match fs::rename(&lock_path, path) {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = fs::remove_file(lock_path);
            Err(GitError::Io(err.to_string()))
        }
    }
}

fn lock_path_for(path: &Path) -> Result<PathBuf> {
    let file_name = path
        .file_name()
        .ok_or_else(|| GitError::InvalidPath("ref path has no filename".into()))?;
    let mut lock_name = file_name.to_os_string();
    lock_name.push(".lock");
    Ok(path.with_file_name(lock_name))
}

pub fn validate_ref_name(name: &str) -> Result<()> {
    if name == "HEAD" {
        return Ok(());
    }
    let path = Path::new(name);
    if !name.starts_with("refs/")
        || name.contains("..")
        || name.contains('\\')
        || name.ends_with('/')
        || name.ends_with(".lock")
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir | std::path::Component::Prefix(_)
            )
        })
    {
        return Err(GitError::InvalidPath(format!("invalid ref name {name}")));
    }
    Ok(())
}

fn validate_namespaced_ref(name: &str, prefix: &str, kind: &str) -> Result<()> {
    validate_ref_name(name)?;
    if name
        .strip_prefix(prefix)
        .is_none_or(|short_name| short_name.is_empty())
    {
        return Err(GitError::InvalidPath(format!(
            "invalid {kind} ref name {name}"
        )));
    }
    Ok(())
}

fn validate_short_ref_name(kind: &str, name: &str) -> Result<()> {
    if name.is_empty()
        || name.starts_with('-')
        || name.starts_with('/')
        || name.ends_with('/')
        || name.contains(' ')
        || name.contains('\\')
    {
        return Err(GitError::InvalidPath(format!("invalid {kind} name {name}")));
    }
    Ok(())
}

fn validate_remote_name(remote: &str) -> Result<()> {
    validate_short_ref_name("remote", remote)?;
    if remote.contains('/') {
        return Err(GitError::InvalidPath(format!(
            "invalid remote name {remote}"
        )));
    }
    Ok(())
}

fn prepare_bundle_ref_updates<F>(
    refs: &[BundleRefUpdate],
    reflog: Option<&BundleRefUpdateReflog>,
    mut read_ref: F,
) -> Result<(Vec<RefUpdate>, Vec<AppliedBundleRefUpdate>)>
where
    F: FnMut(&str, &ObjectId) -> Result<Option<RefTarget>>,
{
    let mut seen = BTreeSet::new();
    let mut updates = Vec::with_capacity(refs.len());
    let mut applied = Vec::with_capacity(refs.len());
    for bundle_ref in refs {
        validate_ref_name(&bundle_ref.name)?;
        if !seen.insert(bundle_ref.name.clone()) {
            return Err(GitError::Transaction(format!(
                "duplicate bundle ref {}",
                bundle_ref.name
            )));
        }
        let old_oid = match read_ref(&bundle_ref.name, &bundle_ref.oid)? {
            Some(RefTarget::Direct(oid)) => Some(oid),
            Some(RefTarget::Symbolic(target)) => {
                return Err(GitError::Transaction(format!(
                    "bundle ref {} would overwrite symbolic ref {target}",
                    bundle_ref.name
                )));
            }
            None => None,
        };
        let reflog = match reflog {
            Some(reflog) => Some(ReflogEntry {
                old_oid: match &old_oid {
                    Some(oid) => *oid,
                    None => null_oid(bundle_ref.oid.format())?,
                },
                new_oid: bundle_ref.oid,
                committer: reflog.committer.clone(),
                message: reflog.message.clone(),
            }),
            None => None,
        };
        updates.push(RefUpdate {
            name: bundle_ref.name.clone(),
            expected: old_oid.map(RefTarget::Direct),
            new: RefTarget::Direct(bundle_ref.oid),
            reflog,
        });
        applied.push(AppliedBundleRefUpdate {
            name: bundle_ref.name.clone(),
            old_oid,
            new_oid: bundle_ref.oid,
        });
    }
    Ok((updates, applied))
}

fn null_oid(format: ObjectFormat) -> Result<ObjectId> {
    Ok(ObjectId::null(format))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn loose_ref_round_trips_direct() {
        let oid = "ce013625030ba8dba906f756967f9e9ca394464a";
        let reference = parse_loose_ref(ObjectFormat::Sha1, "refs/heads/main", oid.as_bytes())
            .expect("test operation should succeed");
        assert_eq!(write_loose_ref(&reference), format!("{oid}\n").into_bytes());
    }

    #[test]
    fn transaction_checks_expected_value() {
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .expect("test operation should succeed");
        let mut store = RefStore::new();
        let mut tx = store.transaction();
        tx.update(RefUpdate {
            name: "refs/heads/main".into(),
            expected: None,
            new: RefTarget::Direct(oid.clone()),
            reflog: None,
        });
        tx.commit().expect("test operation should succeed");
        assert_eq!(store.get("refs/heads/main"), Some(&RefTarget::Direct(oid)));
    }

    #[test]
    fn packed_refs_parse_peeled_refs() {
        let packed = b"# pack-refs with: peeled fully-peeled sorted \n\
ce013625030ba8dba906f756967f9e9ca394464a refs/tags/v1\n\
^e69de29bb2d1d6434b8b29ae775ad8c2e48c5391\n";
        let refs =
            parse_packed_refs(ObjectFormat::Sha1, packed).expect("test operation should succeed");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].reference.name, "refs/tags/v1");
        assert_eq!(
            refs[0]
                .peeled
                .as_ref()
                .expect("test operation should succeed")
                .to_hex(),
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
        );
    }

    #[test]
    fn packed_refs_write_sorted_with_peeled_refs() {
        let head_oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .expect("test operation should succeed");
        let tag_oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "18f002b4484b838b205a48b1e9e6763ba5e3a607",
        )
        .expect("test operation should succeed");
        let peeled_oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391",
        )
        .expect("test operation should succeed");
        let refs = vec![
            PackedRef {
                reference: Ref {
                    name: "refs/tags/v1".into(),
                    target: RefTarget::Direct(tag_oid.clone()),
                },
                peeled: Some(peeled_oid.clone()),
            },
            PackedRef {
                reference: Ref {
                    name: "refs/heads/main".into(),
                    target: RefTarget::Direct(head_oid.clone()),
                },
                peeled: None,
            },
        ];
        let bytes = write_packed_refs(&refs).expect("test operation should succeed");
        let expected = format!(
            "# pack-refs with: peeled fully-peeled sorted \n\
{head_oid} refs/heads/main\n\
{tag_oid} refs/tags/v1\n\
^{peeled_oid}\n"
        );
        assert_eq!(
            String::from_utf8(bytes.clone()).expect("test operation should succeed"),
            expected
        );
        let parsed =
            parse_packed_refs(ObjectFormat::Sha1, &bytes).expect("test operation should succeed");
        assert_eq!(parsed[0], refs[1]);
        assert_eq!(parsed[1], refs[0]);
    }

    #[test]
    fn full_ref_name_validates_and_round_trips_owned() {
        let full = FullRefName::new("refs/heads/main").expect("valid full branch ref");
        assert_eq!(full.as_str(), "refs/heads/main");
        assert_eq!(full.to_string(), "refs/heads/main");
        assert_eq!(full.to_owned().into_string(), "refs/heads/main");

        let head = FullRefNameBuf::new("HEAD").expect("valid HEAD ref");
        assert_eq!(head.as_ref_name().into_str(), "HEAD");

        assert!(FullRefName::new("main").is_err());
        assert!(FullRefNameBuf::new("refs/heads/bad.lock").is_err());
    }

    #[test]
    fn branch_ref_name_helpers_validate_short_and_full_names() {
        let branch =
            BranchRefNameBuf::from_branch_name("feature/topic").expect("valid branch short name");
        assert_eq!(branch.as_str(), "refs/heads/feature/topic");
        assert_eq!(branch.branch_name(), "feature/topic");
        assert_eq!(
            branch.as_full_ref_name().as_str(),
            "refs/heads/feature/topic"
        );
        assert_eq!(
            branch_ref_name("feature/topic").expect("valid branch short name"),
            branch.as_str()
        );

        let borrowed = BranchRefName::from_full("refs/heads/main").expect("valid full branch ref");
        assert_eq!(borrowed.branch_name(), "main");
        assert_eq!(borrowed.to_owned().into_string(), "refs/heads/main");
        assert_eq!(
            FullRefName::new("refs/heads/main")
                .expect("valid full branch ref")
                .as_branch()
                .expect("full ref is a branch")
                .branch_name(),
            "main"
        );

        assert!(BranchRefName::from_full("refs/tags/main").is_err());
        assert!(BranchRefName::from_full("refs/heads").is_err());
        assert!(BranchRefNameBuf::from_branch_name("-bad").is_err());
    }

    #[test]
    fn tag_ref_name_helpers_validate_short_and_full_names() {
        let tag = TagRefNameBuf::from_tag_name("v1.0").expect("valid tag short name");
        assert_eq!(tag.as_str(), "refs/tags/v1.0");
        assert_eq!(tag.tag_name(), "v1.0");
        assert_eq!(tag.as_full_ref_name().as_str(), "refs/tags/v1.0");
        assert_eq!(
            tag_ref_name("v1.0").expect("valid tag short name"),
            tag.as_str()
        );

        let borrowed = TagRefName::from_full("refs/tags/release/1").expect("valid full tag ref");
        assert_eq!(borrowed.tag_name(), "release/1");
        assert_eq!(borrowed.to_owned().into_string(), "refs/tags/release/1");
        assert_eq!(
            FullRefName::new("refs/tags/release/1")
                .expect("valid full tag ref")
                .as_tag()
                .expect("full ref is a tag")
                .tag_name(),
            "release/1"
        );

        assert!(TagRefName::from_full("refs/heads/v1.0").is_err());
        assert!(TagRefName::from_full("refs/tags").is_err());
        assert!(TagRefNameBuf::from_tag_name("bad tag").is_err());
    }

    #[test]
    fn remote_ref_name_helpers_validate_namespace_and_components() {
        let remote = RemoteRefNameBuf::from_remote_branch("origin", "feature/topic")
            .expect("valid remote branch ref");
        assert_eq!(remote.as_str(), "refs/remotes/origin/feature/topic");
        assert_eq!(remote.short_name(), "origin/feature/topic");
        assert_eq!(remote.remote_name(), "origin");
        assert_eq!(remote.remote_branch(), Some("feature/topic"));
        assert_eq!(
            remote.as_full_ref_name().as_str(),
            "refs/remotes/origin/feature/topic"
        );

        let head =
            RemoteRefName::from_full("refs/remotes/origin/HEAD").expect("valid remote HEAD ref");
        assert_eq!(head.remote_name(), "origin");
        assert_eq!(head.remote_branch(), Some("HEAD"));
        assert_eq!(
            FullRefName::new("refs/remotes/upstream/main")
                .expect("valid full remote ref")
                .as_remote()
                .expect("full ref is remote-tracking")
                .remote_name(),
            "upstream"
        );

        let short =
            RemoteRefNameBuf::from_short_name("origin/main").expect("valid remote short ref");
        assert_eq!(short.as_str(), "refs/remotes/origin/main");

        assert!(RemoteRefName::from_full("refs/heads/origin/main").is_err());
        assert!(RemoteRefName::from_full("refs/remotes/").is_err());
        assert!(RemoteRefNameBuf::from_remote_branch("origin/fork", "main").is_err());
    }

    #[test]
    fn file_ref_store_writes_ref_and_reflog() {
        let git_dir = temp_git_dir();
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .expect("test operation should succeed");
        let mut tx = store.transaction();
        tx.update(RefUpdate {
            name: "refs/heads/main".into(),
            expected: None,
            new: RefTarget::Direct(oid.clone()),
            reflog: Some(ReflogEntry {
                old_oid: zero_oid(ObjectFormat::Sha1).expect("test operation should succeed"),
                new_oid: oid.clone(),
                committer: b"Git Rs <sley@example.invalid> 0 +0000".to_vec(),
                message: b"update by test".to_vec(),
            }),
        });
        tx.commit().expect("test operation should succeed");
        assert_eq!(
            store
                .read_ref("refs/heads/main")
                .expect("test operation should succeed"),
            Some(RefTarget::Direct(oid))
        );
        let log = store
            .read_reflog("refs/heads/main")
            .expect("test operation should succeed");
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].message, b"update by test");
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn file_ref_store_applies_bundle_refs_with_reflog() {
        let git_dir = temp_git_dir();
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let old_main = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .expect("test operation should succeed");
        let new_main = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391",
        )
        .expect("test operation should succeed");
        let tag_oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "18f002b4484b838b205a48b1e9e6763ba5e3a607",
        )
        .expect("test operation should succeed");
        let mut tx = store.transaction();
        tx.update(RefUpdate {
            name: "refs/heads/main".into(),
            expected: None,
            new: RefTarget::Direct(old_main.clone()),
            reflog: None,
        });
        tx.commit().expect("test operation should succeed");

        let applied = store
            .apply_bundle_ref_updates(
                &[
                    BundleRefUpdate {
                        name: "refs/heads/main".into(),
                        oid: new_main.clone(),
                    },
                    BundleRefUpdate {
                        name: "refs/tags/v1.0".into(),
                        oid: tag_oid.clone(),
                    },
                ],
                Some(BundleRefUpdateReflog {
                    committer: b"Git Rs <sley@example.invalid> 0 +0000".to_vec(),
                    message: b"bundle: import refs".to_vec(),
                }),
            )
            .expect("test operation should succeed");

        assert_eq!(
            applied,
            vec![
                AppliedBundleRefUpdate {
                    name: "refs/heads/main".into(),
                    old_oid: Some(old_main.clone()),
                    new_oid: new_main.clone(),
                },
                AppliedBundleRefUpdate {
                    name: "refs/tags/v1.0".into(),
                    old_oid: None,
                    new_oid: tag_oid.clone(),
                }
            ]
        );
        assert_eq!(
            store
                .read_ref("refs/heads/main")
                .expect("test operation should succeed"),
            Some(RefTarget::Direct(new_main.clone()))
        );
        assert_eq!(
            store
                .read_ref("refs/tags/v1.0")
                .expect("test operation should succeed"),
            Some(RefTarget::Direct(tag_oid.clone()))
        );
        let main_log = store
            .read_reflog("refs/heads/main")
            .expect("test operation should succeed");
        assert_eq!(main_log.len(), 1);
        assert_eq!(main_log[0].old_oid, old_main);
        assert_eq!(main_log[0].new_oid, new_main);
        assert_eq!(main_log[0].message, b"bundle: import refs");
        let tag_log = store
            .read_reflog("refs/tags/v1.0")
            .expect("test operation should succeed");
        assert_eq!(tag_log.len(), 1);
        assert_eq!(
            tag_log[0].old_oid,
            zero_oid(ObjectFormat::Sha1).expect("test operation should succeed")
        );
        assert_eq!(tag_log[0].new_oid, tag_oid);
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn file_ref_store_rejects_bad_bundle_ref_before_writing() {
        let git_dir = temp_git_dir();
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .expect("test operation should succeed");

        let result = store.apply_bundle_ref_updates(
            &[
                BundleRefUpdate {
                    name: "refs/heads/main".into(),
                    oid: oid.clone(),
                },
                BundleRefUpdate {
                    name: "refs/heads/bad.lock".into(),
                    oid,
                },
            ],
            None,
        );

        assert!(result.is_err());
        assert_eq!(
            store
                .read_ref("refs/heads/main")
                .expect("test operation should succeed"),
            None
        );
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn file_ref_store_rejects_bundle_ref_over_symbolic_ref() {
        let git_dir = temp_git_dir();
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .expect("test operation should succeed");
        let mut tx = store.transaction();
        tx.update(RefUpdate {
            name: "refs/heads/main".into(),
            expected: None,
            new: RefTarget::Symbolic("refs/heads/base".into()),
            reflog: None,
        });
        tx.commit().expect("test operation should succeed");

        let result = store.apply_bundle_ref_updates(
            &[BundleRefUpdate {
                name: "refs/heads/main".into(),
                oid,
            }],
            None,
        );

        assert!(result.is_err());
        assert_eq!(
            store
                .read_ref("refs/heads/main")
                .expect("test operation should succeed"),
            Some(RefTarget::Symbolic("refs/heads/base".into()))
        );
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn file_ref_store_expires_reflog_entries_by_timestamp() {
        let git_dir = temp_git_dir();
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let first = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .expect("test operation should succeed");
        let second = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391",
        )
        .expect("test operation should succeed");
        let mut tx = store.transaction();
        tx.update(RefUpdate {
            name: "refs/heads/main".into(),
            expected: None,
            new: RefTarget::Direct(first.clone()),
            reflog: Some(ReflogEntry {
                old_oid: zero_oid(ObjectFormat::Sha1).expect("test operation should succeed"),
                new_oid: first.clone(),
                committer: b"Git Rs <sley@example.invalid> 0 +0000".to_vec(),
                message: b"old".to_vec(),
            }),
        });
        tx.update(RefUpdate {
            name: "refs/heads/main".into(),
            expected: None,
            new: RefTarget::Direct(second.clone()),
            reflog: Some(ReflogEntry {
                old_oid: first,
                new_oid: second.clone(),
                committer: b"Git Rs <sley@example.invalid> 100 +0000".to_vec(),
                message: b"new".to_vec(),
            }),
        });
        tx.commit().expect("test operation should succeed");

        let removed = store
            .expire_reflog_older_than("refs/heads/main", 50)
            .expect("test operation should succeed");
        assert_eq!(removed, 1);
        let log = store
            .read_reflog("refs/heads/main")
            .expect("test operation should succeed");
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].new_oid, second);
        assert_eq!(log[0].message, b"new");
        assert!(
            !git_dir
                .join("logs")
                .join("refs")
                .join("heads")
                .join("main.lock")
                .exists()
        );
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn file_ref_store_creates_branch() {
        let git_dir = temp_git_dir();
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .expect("test operation should succeed");
        let branch = store
            .create_branch(
                "feature",
                oid.clone(),
                b"Git Rs <sley@example.invalid> 0 +0000".to_vec(),
                b"branch: Created from main".to_vec(),
            )
            .expect("test operation should succeed");
        assert_eq!(branch.name, "refs/heads/feature");
        assert_eq!(
            store
                .read_ref("refs/heads/feature")
                .expect("test operation should succeed"),
            Some(RefTarget::Direct(oid))
        );
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn file_ref_store_deletes_loose_branch() {
        let git_dir = temp_git_dir();
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .expect("test operation should succeed");
        store
            .create_branch(
                "feature",
                oid.clone(),
                b"Git Rs <sley@example.invalid> 0 +0000".to_vec(),
                b"branch: Created from main".to_vec(),
            )
            .expect("test operation should succeed");
        let deleted = store
            .delete_branch("feature")
            .expect("test operation should succeed");
        assert_eq!(deleted.name, "refs/heads/feature");
        assert_eq!(deleted.oid, oid);
        assert_eq!(
            store
                .read_ref("refs/heads/feature")
                .expect("test operation should succeed"),
            None
        );
        assert!(!git_dir.join("refs").join("heads").join("feature").exists());
        assert!(
            !git_dir
                .join("logs")
                .join("refs")
                .join("heads")
                .join("feature")
                .exists()
        );
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn file_ref_store_deletes_generic_loose_ref() {
        let git_dir = temp_git_dir();
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .expect("test operation should succeed");
        let mut tx = store.transaction();
        tx.update(RefUpdate {
            name: "refs/heads/topic".into(),
            expected: None,
            new: RefTarget::Direct(oid.clone()),
            reflog: Some(ReflogEntry {
                old_oid: zero_oid(ObjectFormat::Sha1).expect("test operation should succeed"),
                new_oid: oid.clone(),
                committer: b"Git Rs <sley@example.invalid> 0 +0000".to_vec(),
                message: b"update by test".to_vec(),
            }),
        });
        tx.commit().expect("test operation should succeed");
        let deleted = store
            .delete_ref("refs/heads/topic")
            .expect("test operation should succeed");
        assert_eq!(deleted.name, "refs/heads/topic");
        assert_eq!(deleted.oid, oid);
        assert_eq!(
            store
                .read_ref("refs/heads/topic")
                .expect("test operation should succeed"),
            None
        );
        assert!(!git_dir.join("refs").join("heads").join("topic").exists());
        assert!(
            !git_dir
                .join("logs")
                .join("refs")
                .join("heads")
                .join("topic")
                .exists()
        );
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn file_ref_store_reports_current_branch() {
        let git_dir = temp_git_dir();
        fs::write(git_dir.join("HEAD"), b"ref: refs/heads/main\n")
            .expect("test operation should succeed");
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        assert_eq!(
            store
                .current_branch_ref()
                .expect("test operation should succeed"),
            Some("refs/heads/main".into())
        );
        assert_eq!(
            store
                .current_branch()
                .expect("test operation should succeed"),
            Some("main".into())
        );
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn file_ref_store_resolves_linked_worktree_head_through_common_refs() {
        let common = temp_git_dir();
        let admin = common.join("worktrees").join("linked");
        fs::create_dir_all(&admin).expect("test operation should succeed");
        fs::write(admin.join("commondir"), "../..\n").expect("test operation should succeed");
        fs::write(admin.join("HEAD"), b"ref: refs/heads/topic\n")
            .expect("test operation should succeed");
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha256,
            "08ffba112b648c22b5425f01bec2c37ffc524c4d48ef04337779df3973733050",
        )
        .expect("test operation should succeed");
        fs::create_dir_all(common.join("refs").join("heads"))
            .expect("test operation should succeed");
        fs::write(
            common.join("refs").join("heads").join("topic"),
            format!("{oid}\n"),
        )
        .expect("test operation should succeed");

        let store = FileRefStore::new(&admin, ObjectFormat::Sha256);
        assert_eq!(
            store
                .read_ref("HEAD")
                .expect("test operation should succeed"),
            Some(RefTarget::Symbolic("refs/heads/topic".into()))
        );
        assert_eq!(
            store
                .read_ref("refs/heads/topic")
                .expect("test operation should succeed"),
            Some(RefTarget::Direct(oid))
        );

        fs::remove_dir_all(common).expect("test operation should succeed");
    }

    #[test]
    fn file_ref_store_creates_tag() {
        let git_dir = temp_git_dir();
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .expect("test operation should succeed");
        let tag = store
            .create_tag("v1.0", oid.clone())
            .expect("test operation should succeed");
        assert_eq!(tag.name, "refs/tags/v1.0");
        assert_eq!(
            store
                .read_ref("refs/tags/v1.0")
                .expect("test operation should succeed"),
            Some(RefTarget::Direct(oid))
        );
        assert!(
            store
                .read_reflog("refs/tags/v1.0")
                .expect("test operation should succeed")
                .is_empty()
        );
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn file_ref_store_deletes_loose_tag() {
        let git_dir = temp_git_dir();
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .expect("test operation should succeed");
        store
            .create_tag("v1.0", oid.clone())
            .expect("test operation should succeed");
        let deleted = store
            .delete_tag("v1.0")
            .expect("test operation should succeed");
        assert_eq!(deleted.name, "refs/tags/v1.0");
        assert_eq!(deleted.oid, oid);
        assert_eq!(
            store
                .read_ref("refs/tags/v1.0")
                .expect("test operation should succeed"),
            None
        );
        assert!(!git_dir.join("refs").join("tags").join("v1.0").exists());
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn file_ref_store_reads_packed_ref() {
        let git_dir = temp_git_dir();
        fs::write(
            git_dir.join("packed-refs"),
            b"ce013625030ba8dba906f756967f9e9ca394464a refs/heads/main\n",
        )
        .expect("test operation should succeed");
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        assert!(matches!(
            store
                .read_ref("refs/heads/main")
                .expect("test operation should succeed"),
            Some(RefTarget::Direct(_))
        ));
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn file_ref_store_lists_loose_refs_over_packed_refs() {
        let git_dir = temp_git_dir();
        fs::write(
            git_dir.join("packed-refs"),
            b"e69de29bb2d1d6434b8b29ae775ad8c2e48c5391 refs/heads/main\n",
        )
        .expect("test operation should succeed");
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .expect("test operation should succeed");
        let mut tx = store.transaction();
        tx.update(RefUpdate {
            name: "refs/heads/main".into(),
            expected: None,
            new: RefTarget::Direct(oid.clone()),
            reflog: None,
        });
        tx.commit().expect("test operation should succeed");
        let refs = store.list_refs().expect("test operation should succeed");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].target, RefTarget::Direct(oid));
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn file_ref_store_writes_packed_refs() {
        let git_dir = temp_git_dir();
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .expect("test operation should succeed");
        store
            .write_packed_refs(&[PackedRef {
                reference: Ref {
                    name: "refs/heads/main".into(),
                    target: RefTarget::Direct(oid.clone()),
                },
                peeled: None,
            }])
            .expect("test operation should succeed");
        assert_eq!(
            store
                .read_ref("refs/heads/main")
                .expect("test operation should succeed"),
            Some(RefTarget::Direct(oid.clone()))
        );
        let refs = store.list_refs().expect("test operation should succeed");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].target, RefTarget::Direct(oid));
        assert!(git_dir.join("packed-refs").exists());
        assert!(!git_dir.join("packed-refs.lock").exists());
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn file_ref_store_reads_reftable_stack_and_ignores_dummy_head() {
        let git_dir = temp_git_dir();
        write_reftable_config(&git_dir);
        fs::write(git_dir.join("HEAD"), b"ref: refs/heads/.invalid\n")
            .expect("test operation should succeed");
        let head_oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .expect("test operation should succeed");
        let tag_oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "18f002b4484b838b205a48b1e9e6763ba5e3a607",
        )
        .expect("test operation should succeed");
        let peeled_oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391",
        )
        .expect("test operation should succeed");
        write_reftable_stack(
            &git_dir,
            &[(
                "000000000001-000000000001-rust.ref",
                vec![
                    sley_formats::ReftableRefRecord {
                        name: "HEAD".into(),
                        update_index: 1,
                        value: ReftableRefValue::Symbolic("refs/heads/main".into()),
                    },
                    sley_formats::ReftableRefRecord {
                        name: "refs/heads/main".into(),
                        update_index: 1,
                        value: ReftableRefValue::Direct(head_oid.clone()),
                    },
                    sley_formats::ReftableRefRecord {
                        name: "refs/tags/v1.0".into(),
                        update_index: 1,
                        value: ReftableRefValue::Peeled {
                            target: tag_oid.clone(),
                            peeled: peeled_oid,
                        },
                    },
                ],
            )],
        );

        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        assert_eq!(
            store
                .read_ref("HEAD")
                .expect("test operation should succeed"),
            Some(RefTarget::Symbolic("refs/heads/main".into()))
        );
        assert_eq!(
            store
                .read_ref("refs/heads/main")
                .expect("test operation should succeed"),
            Some(RefTarget::Direct(head_oid.clone()))
        );
        assert_eq!(
            store
                .read_ref("refs/tags/v1.0")
                .expect("test operation should succeed"),
            Some(RefTarget::Direct(tag_oid.clone()))
        );
        let refs = store.list_refs().expect("test operation should succeed");
        assert_eq!(
            refs,
            vec![
                Ref {
                    name: "refs/heads/main".into(),
                    target: RefTarget::Direct(head_oid),
                },
                Ref {
                    name: "refs/tags/v1.0".into(),
                    target: RefTarget::Direct(tag_oid),
                },
            ]
        );

        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn file_ref_store_applies_reftable_stack_overrides_and_deletions() {
        let git_dir = temp_git_dir();
        write_reftable_config(&git_dir);
        let first = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .expect("test operation should succeed");
        let second = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391",
        )
        .expect("test operation should succeed");
        write_reftable_stack(
            &git_dir,
            &[
                (
                    "000000000001-000000000001-base.ref",
                    vec![
                        sley_formats::ReftableRefRecord {
                            name: "refs/heads/main".into(),
                            update_index: 1,
                            value: ReftableRefValue::Direct(first),
                        },
                        sley_formats::ReftableRefRecord {
                            name: "refs/heads/topic".into(),
                            update_index: 1,
                            value: ReftableRefValue::Direct(second.clone()),
                        },
                    ],
                ),
                (
                    "000000000002-000000000002-tip.ref",
                    vec![
                        sley_formats::ReftableRefRecord {
                            name: "refs/heads/main".into(),
                            update_index: 2,
                            value: ReftableRefValue::Direct(second.clone()),
                        },
                        sley_formats::ReftableRefRecord {
                            name: "refs/heads/topic".into(),
                            update_index: 2,
                            value: ReftableRefValue::Deletion,
                        },
                    ],
                ),
            ],
        );

        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        assert_eq!(
            store
                .read_ref("refs/heads/main")
                .expect("test operation should succeed"),
            Some(RefTarget::Direct(second.clone()))
        );
        assert_eq!(
            store
                .read_ref("refs/heads/topic")
                .expect("test operation should succeed"),
            None
        );
        assert_eq!(
            store.list_refs().expect("test operation should succeed"),
            vec![Ref {
                name: "refs/heads/main".into(),
                target: RefTarget::Direct(second),
            }]
        );

        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn file_ref_store_writes_reftable_transaction_table() {
        let git_dir = temp_git_dir();
        write_reftable_config(&git_dir);
        let first = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .expect("test operation should succeed");
        let second = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391",
        )
        .expect("test operation should succeed");
        write_reftable_stack(
            &git_dir,
            &[(
                "000000000001-000000000001-base.ref",
                vec![sley_formats::ReftableRefRecord {
                    name: "refs/heads/main".into(),
                    update_index: 1,
                    value: ReftableRefValue::Direct(first),
                }],
            )],
        );

        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let mut tx = store.transaction();
        tx.update(RefUpdate {
            name: "HEAD".into(),
            expected: None,
            new: RefTarget::Symbolic("refs/heads/main".into()),
            reflog: None,
        });
        tx.update(RefUpdate {
            name: "refs/heads/main".into(),
            expected: None,
            new: RefTarget::Direct(second.clone()),
            reflog: None,
        });
        tx.commit().expect("test operation should succeed");

        assert_eq!(
            store
                .read_ref("HEAD")
                .expect("test operation should succeed"),
            Some(RefTarget::Symbolic("refs/heads/main".into()))
        );
        assert_eq!(
            store
                .read_ref("refs/heads/main")
                .expect("test operation should succeed"),
            Some(RefTarget::Direct(second.clone()))
        );
        assert_eq!(
            store
                .list_refs()
                .expect("test operation should succeed")
                .len(),
            1
        );
        assert!(!git_dir.join("HEAD").exists());
        let tables = fs::read_to_string(git_dir.join("reftable").join("tables.list"))
            .expect("test operation should succeed");
        assert_eq!(tables.lines().count(), 2);
        assert!(
            tables
                .lines()
                .last()
                .expect("test operation should succeed")
                .contains("sley"),
            "expected rust-written reftable in tables.list, got {tables}"
        );

        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn file_ref_store_deletes_reftable_refs_with_tombstones() {
        let git_dir = temp_git_dir();
        write_reftable_config(&git_dir);
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .expect("test operation should succeed");
        write_reftable_stack(
            &git_dir,
            &[(
                "000000000001-000000000001-base.ref",
                vec![
                    sley_formats::ReftableRefRecord {
                        name: "refs/heads/main".into(),
                        update_index: 1,
                        value: ReftableRefValue::Direct(oid.clone()),
                    },
                    sley_formats::ReftableRefRecord {
                        name: "refs/alias/main".into(),
                        update_index: 1,
                        value: ReftableRefValue::Symbolic("refs/heads/main".into()),
                    },
                ],
            )],
        );

        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        assert!(
            store
                .delete_symbolic_ref("refs/alias/main")
                .expect("test operation should succeed")
        );
        assert_eq!(
            store
                .read_ref("refs/alias/main")
                .expect("test operation should succeed"),
            None
        );
        let deleted = store
            .delete_ref("refs/heads/main")
            .expect("test operation should succeed");
        assert_eq!(deleted.oid, oid);
        assert_eq!(
            store
                .read_ref("refs/heads/main")
                .expect("test operation should succeed"),
            None
        );
        assert!(
            store
                .list_refs()
                .expect("test operation should succeed")
                .is_empty()
        );
        let tables = fs::read_to_string(git_dir.join("reftable").join("tables.list"))
            .expect("test operation should succeed");
        assert_eq!(tables.lines().count(), 3);

        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn file_ref_store_deletes_packed_branch() {
        let git_dir = temp_git_dir();
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let branch_oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .expect("test operation should succeed");
        let tag_oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391",
        )
        .expect("test operation should succeed");
        store
            .write_packed_refs(&[
                PackedRef {
                    reference: Ref {
                        name: "refs/heads/feature".into(),
                        target: RefTarget::Direct(branch_oid.clone()),
                    },
                    peeled: None,
                },
                PackedRef {
                    reference: Ref {
                        name: "refs/tags/v1.0".into(),
                        target: RefTarget::Direct(tag_oid.clone()),
                    },
                    peeled: None,
                },
            ])
            .expect("test operation should succeed");
        let deleted = store
            .delete_branch("feature")
            .expect("test operation should succeed");
        assert_eq!(deleted.name, "refs/heads/feature");
        assert_eq!(deleted.oid, branch_oid);
        assert_eq!(
            store
                .read_ref("refs/heads/feature")
                .expect("test operation should succeed"),
            None
        );
        assert_eq!(
            store
                .read_ref("refs/tags/v1.0")
                .expect("test operation should succeed"),
            Some(RefTarget::Direct(tag_oid))
        );
        assert!(!git_dir.join("packed-refs.lock").exists());
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn file_ref_store_deletes_packed_tag() {
        let git_dir = temp_git_dir();
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .expect("test operation should succeed");
        store
            .write_packed_refs(&[PackedRef {
                reference: Ref {
                    name: "refs/tags/v1.0".into(),
                    target: RefTarget::Direct(oid.clone()),
                },
                peeled: None,
            }])
            .expect("test operation should succeed");
        let deleted = store
            .delete_tag("v1.0")
            .expect("test operation should succeed");
        assert_eq!(deleted.name, "refs/tags/v1.0");
        assert_eq!(deleted.oid, oid);
        assert_eq!(
            store
                .read_ref("refs/tags/v1.0")
                .expect("test operation should succeed"),
            None
        );
        assert!(!git_dir.join("packed-refs.lock").exists());
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn file_ref_store_packs_loose_refs_and_prunes() {
        let git_dir = temp_git_dir();
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let main_oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .expect("test operation should succeed");
        let tag_oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391",
        )
        .expect("test operation should succeed");
        let mut tx = store.transaction();
        tx.update(RefUpdate {
            name: "refs/heads/main".into(),
            expected: None,
            new: RefTarget::Direct(main_oid.clone()),
            reflog: None,
        });
        tx.update(RefUpdate {
            name: "refs/tags/v1.0".into(),
            expected: None,
            new: RefTarget::Direct(tag_oid.clone()),
            reflog: None,
        });
        tx.commit().expect("test operation should succeed");

        let packed = store
            .pack_refs(true)
            .expect("test operation should succeed");
        assert_eq!(packed.len(), 2);
        assert_eq!(
            store
                .read_ref("refs/heads/main")
                .expect("test operation should succeed"),
            Some(RefTarget::Direct(main_oid))
        );
        assert_eq!(
            store
                .read_ref("refs/tags/v1.0")
                .expect("test operation should succeed"),
            Some(RefTarget::Direct(tag_oid))
        );
        assert!(!git_dir.join("refs").join("heads").join("main").exists());
        assert!(!git_dir.join("refs").join("tags").join("v1.0").exists());
        assert!(git_dir.join("packed-refs").exists());
        assert!(!git_dir.join("packed-refs.lock").exists());
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn file_ref_store_packs_loose_refs_without_pruning() {
        let git_dir = temp_git_dir();
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .expect("test operation should succeed");
        let mut tx = store.transaction();
        tx.update(RefUpdate {
            name: "refs/heads/main".into(),
            expected: None,
            new: RefTarget::Direct(oid.clone()),
            reflog: None,
        });
        tx.commit().expect("test operation should succeed");

        let packed = store
            .pack_refs(false)
            .expect("test operation should succeed");
        assert_eq!(packed.len(), 1);
        assert!(git_dir.join("refs").join("heads").join("main").exists());
        assert_eq!(
            store
                .read_ref("refs/heads/main")
                .expect("test operation should succeed"),
            Some(RefTarget::Direct(oid))
        );
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn file_ref_store_packs_loose_refs_with_peeled_ids() {
        let git_dir = temp_git_dir();
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let tag_oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .expect("test operation should succeed");
        let peeled_oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391",
        )
        .expect("test operation should succeed");
        let mut tx = store.transaction();
        tx.update(RefUpdate {
            name: "refs/tags/v1.0".into(),
            expected: None,
            new: RefTarget::Direct(tag_oid.clone()),
            reflog: None,
        });
        tx.commit().expect("test operation should succeed");

        let packed = store
            .pack_refs_with_peeler(true, |name, oid| {
                if name == "refs/tags/v1.0" && oid == &tag_oid {
                    Ok(Some(peeled_oid.clone()))
                } else {
                    Ok(None)
                }
            })
            .expect("test operation should succeed");
        assert_eq!(packed.len(), 1);
        assert_eq!(packed[0].peeled, Some(peeled_oid.clone()));
        let bytes =
            fs::read_to_string(git_dir.join("packed-refs")).expect("test operation should succeed");
        assert!(bytes.contains(&format!("^{peeled_oid}\n")));
        assert!(!git_dir.join("refs").join("tags").join("v1.0").exists());
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    fn reflog_entry(new_oid: &ObjectId, timestamp: i64, message: &str) -> ReflogEntry {
        ReflogEntry {
            old_oid: zero_oid(new_oid.format()).expect("test operation should succeed"),
            new_oid: new_oid.clone(),
            committer: format!("Git Rs <sley@example.invalid> {timestamp} +0000").into_bytes(),
            message: message.as_bytes().to_vec(),
        }
    }

    #[test]
    fn expire_reflog_drops_old_entries_and_keeps_latest() {
        let oid_a = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .expect("test operation should succeed");
        let oid_b = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391",
        )
        .expect("test operation should succeed");
        let oid_c = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "18f002b4484b838b205a48b1e9e6763ba5e3a607",
        )
        .expect("test operation should succeed");
        let entries = vec![
            reflog_entry(&oid_a, 10, "oldest"),
            reflog_entry(&oid_b, 100, "middle"),
            reflog_entry(&oid_c, 20, "latest"),
        ];

        // Cutoff drops the oldest entry; the most recent entry survives even
        // though its timestamp (20) is below the cutoff (50).
        let retained =
            expire_reflog(&entries, 50, None, |_| true).expect("test operation should succeed");
        assert_eq!(retained.len(), 2);
        assert_eq!(retained[0].message, b"middle");
        assert_eq!(retained[1].message, b"latest");
    }

    #[test]
    fn expire_reflog_applies_stricter_unreachable_cutoff() {
        let reachable = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .expect("test operation should succeed");
        let unreachable = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391",
        )
        .expect("test operation should succeed");
        let tip = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "18f002b4484b838b205a48b1e9e6763ba5e3a607",
        )
        .expect("test operation should succeed");
        // Both candidate entries sit above the lenient cutoff (50) but below the
        // stricter unreachable cutoff (150). Only the unreachable one is dropped.
        let entries = vec![
            reflog_entry(&reachable, 100, "reachable"),
            reflog_entry(&unreachable, 100, "unreachable"),
            reflog_entry(&tip, 200, "tip"),
        ];
        let retained = expire_reflog(&entries, 50, Some(150), |oid| {
            oid == &reachable || oid == &tip
        })
        .expect("test operation should succeed");
        assert_eq!(retained.len(), 2);
        assert_eq!(retained[0].message, b"reachable");
        assert_eq!(retained[1].message, b"tip");
    }

    #[test]
    fn expire_reflog_keeps_single_entry_below_cutoff() {
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .expect("test operation should succeed");
        let entries = vec![reflog_entry(&oid, 1, "only")];
        let retained = expire_reflog(&entries, i64::MAX, Some(i64::MAX), |_| false)
            .expect("test operation should succeed");
        assert_eq!(retained.len(), 1);
        assert_eq!(retained[0].message, b"only");
    }

    #[test]
    fn file_ref_store_expire_reflog_file_rewrites_and_dry_runs() {
        let git_dir = temp_git_dir();
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let first = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .expect("test operation should succeed");
        let second = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391",
        )
        .expect("test operation should succeed");
        store
            .write_reflog(
                "refs/heads/main",
                &[
                    reflog_entry(&first, 10, "old"),
                    reflog_entry(&second, 100, "new"),
                ],
            )
            .expect("test operation should succeed");

        // Dry run reports the removal count without touching the file.
        let would_remove = store
            .expire_reflog_file("refs/heads/main", 50, None, false, |_| true)
            .expect("test operation should succeed");
        assert_eq!(would_remove, 1);
        assert_eq!(
            store
                .read_reflog("refs/heads/main")
                .expect("test operation should succeed")
                .len(),
            2
        );

        // Opt-in rewrite drops the stale entry and leaves the latest.
        let removed = store
            .expire_reflog_file("refs/heads/main", 50, None, true, |_| true)
            .expect("test operation should succeed");
        assert_eq!(removed, 1);
        let log = store
            .read_reflog("refs/heads/main")
            .expect("test operation should succeed");
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].new_oid, second);
        assert!(
            !git_dir
                .join("logs")
                .join("refs")
                .join("heads")
                .join("main.lock")
                .exists()
        );
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn file_ref_transaction_commits_all_refs_atomically() {
        let git_dir = temp_git_dir();
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let main_oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .expect("test operation should succeed");
        let topic_oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391",
        )
        .expect("test operation should succeed");
        let tag_oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "18f002b4484b838b205a48b1e9e6763ba5e3a607",
        )
        .expect("test operation should succeed");
        let mut tx = store.transaction();
        tx.update(RefUpdate {
            name: "refs/heads/main".into(),
            expected: None,
            new: RefTarget::Direct(main_oid.clone()),
            reflog: Some(reflog_entry(&main_oid, 0, "create main")),
        });
        tx.update(RefUpdate {
            name: "refs/heads/topic".into(),
            expected: None,
            new: RefTarget::Direct(topic_oid.clone()),
            reflog: None,
        });
        tx.update(RefUpdate {
            name: "refs/tags/v1.0".into(),
            expected: None,
            new: RefTarget::Direct(tag_oid.clone()),
            reflog: None,
        });
        tx.commit().expect("test operation should succeed");

        assert_eq!(
            store
                .read_ref("refs/heads/main")
                .expect("test operation should succeed"),
            Some(RefTarget::Direct(main_oid.clone()))
        );
        assert_eq!(
            store
                .read_ref("refs/heads/topic")
                .expect("test operation should succeed"),
            Some(RefTarget::Direct(topic_oid))
        );
        assert_eq!(
            store
                .read_ref("refs/tags/v1.0")
                .expect("test operation should succeed"),
            Some(RefTarget::Direct(tag_oid))
        );
        let main_log = store
            .read_reflog("refs/heads/main")
            .expect("test operation should succeed");
        assert_eq!(main_log.len(), 1);
        assert_eq!(main_log[0].new_oid, main_oid);
        // No lock files survive a successful commit.
        assert!(
            !git_dir
                .join("refs")
                .join("heads")
                .join("main.lock")
                .exists()
        );
        assert!(
            !git_dir
                .join("refs")
                .join("heads")
                .join("topic.lock")
                .exists()
        );
        assert!(!git_dir.join("refs").join("tags").join("v1.0.lock").exists());
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn file_ref_transaction_rolls_back_all_refs_on_expected_mismatch() {
        let git_dir = temp_git_dir();
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let old_topic = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .expect("test operation should succeed");
        let new_main = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391",
        )
        .expect("test operation should succeed");
        let new_tag = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "18f002b4484b838b205a48b1e9e6763ba5e3a607",
        )
        .expect("test operation should succeed");
        let wrong_expected = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "0000000000000000000000000000000000000001",
        )
        .expect("test operation should succeed");

        // Seed an existing topic ref so the failing update has a real prior value
        // to be compared against (and left untouched).
        let mut seed = store.transaction();
        seed.update(RefUpdate {
            name: "refs/heads/topic".into(),
            expected: None,
            new: RefTarget::Direct(old_topic.clone()),
            reflog: None,
        });
        seed.commit().expect("test operation should succeed");

        let mut tx = store.transaction();
        // 1st ref: brand new, would succeed in isolation.
        tx.update(RefUpdate {
            name: "refs/heads/main".into(),
            expected: None,
            new: RefTarget::Direct(new_main.clone()),
            reflog: Some(reflog_entry(&new_main, 0, "create main")),
        });
        // 2nd ref: expected value does not match on disk -> whole tx must abort.
        tx.update(RefUpdate {
            name: "refs/heads/topic".into(),
            expected: Some(RefTarget::Direct(wrong_expected)),
            new: RefTarget::Direct(new_main.clone()),
            reflog: None,
        });
        // 3rd ref: brand new, must not be written because the tx aborts.
        tx.update(RefUpdate {
            name: "refs/tags/v1.0".into(),
            expected: None,
            new: RefTarget::Direct(new_tag),
            reflog: None,
        });
        let result = tx.commit();
        assert!(result.is_err());

        // Nothing changed: the new refs were never created and the existing one
        // keeps its original value.
        assert_eq!(
            store
                .read_ref("refs/heads/main")
                .expect("test operation should succeed"),
            None
        );
        assert_eq!(
            store
                .read_ref("refs/heads/topic")
                .expect("test operation should succeed"),
            Some(RefTarget::Direct(old_topic))
        );
        assert_eq!(
            store
                .read_ref("refs/tags/v1.0")
                .expect("test operation should succeed"),
            None
        );
        assert!(
            store
                .read_reflog("refs/heads/main")
                .expect("test operation should succeed")
                .is_empty()
        );

        // All lock files were released.
        assert!(
            !git_dir
                .join("refs")
                .join("heads")
                .join("main.lock")
                .exists()
        );
        assert!(
            !git_dir
                .join("refs")
                .join("heads")
                .join("topic.lock")
                .exists()
        );
        assert!(!git_dir.join("refs").join("tags").join("v1.0.lock").exists());
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    fn temp_git_dir() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "sley-refs-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("test operation should succeed");
        path
    }

    fn zero_oid(format: ObjectFormat) -> Result<ObjectId> {
        Ok(ObjectId::null(format))
    }

    fn write_reftable_config(git_dir: &Path) {
        fs::write(
            git_dir.join("config"),
            b"[core]\n\trepositoryformatversion = 1\n[extensions]\n\trefStorage = reftable\n",
        )
        .expect("test operation should succeed");
    }

    fn write_reftable_stack(
        git_dir: &Path,
        tables: &[(&str, Vec<sley_formats::ReftableRefRecord>)],
    ) {
        let reftable_dir = git_dir.join("reftable");
        fs::create_dir_all(&reftable_dir).expect("test operation should succeed");
        let mut list = String::new();
        for (idx, (name, refs)) in tables.iter().enumerate() {
            let update_index = (idx + 1) as u64;
            let bytes = sley_formats::Reftable::write_ref_only(
                ObjectFormat::Sha1,
                update_index,
                update_index,
                refs,
            )
            .expect("test operation should succeed");
            fs::write(reftable_dir.join(name), bytes).expect("test operation should succeed");
            list.push_str(name);
            list.push('\n');
        }
        fs::write(reftable_dir.join("tables.list"), list).expect("test operation should succeed");
    }
}
