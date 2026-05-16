use git_core::{GitError, ObjectFormat, ObjectId, Result};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

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
        Self {
            git_dir: git_dir.into(),
            format,
        }
    }

    pub fn read_ref(&self, name: &str) -> Result<Option<RefTarget>> {
        validate_ref_name(name)?;
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

    pub fn list_refs(&self) -> Result<Vec<Ref>> {
        let mut refs = BTreeMap::new();
        let packed_path = self.git_dir.join("packed-refs");
        if packed_path.exists() {
            for packed in parse_packed_refs(self.format, &fs::read(packed_path)?)? {
                refs.insert(packed.reference.name.clone(), packed.reference);
            }
        }
        let refs_dir = self.git_dir.join("refs");
        if refs_dir.exists() {
            self.collect_loose_refs(&refs_dir, "refs", &mut refs)?;
        }
        Ok(refs.into_values().collect())
    }

    pub fn write_packed_refs(&self, refs: &[PackedRef]) -> Result<()> {
        write_locked(&self.git_dir.join("packed-refs"), &write_packed_refs(refs)?)
    }

    pub fn pack_refs(&self, prune_loose: bool) -> Result<Vec<PackedRef>> {
        self.pack_refs_with_peeler(prune_loose, |_, _| Ok(None))
    }

    pub fn pack_refs_with_peeler<F>(&self, prune_loose: bool, mut peel: F) -> Result<Vec<PackedRef>>
    where
        F: FnMut(&str, &ObjectId) -> Result<Option<ObjectId>>,
    {
        let mut packed_refs = BTreeMap::new();
        let packed_path = self.git_dir.join("packed-refs");
        if packed_path.exists() {
            for packed in parse_packed_refs(self.format, &fs::read(&packed_path)?)? {
                packed_refs.insert(packed.reference.name.clone(), packed);
            }
        }

        let mut loose_refs = BTreeMap::new();
        let refs_dir = self.git_dir.join("refs");
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
        let zero = ObjectId::from_raw(self.format, &vec![0; self.format.raw_len()])?;
        let mut tx = self.transaction();
        tx.update(RefUpdate {
            name: name.clone(),
            expected: None,
            new: RefTarget::Direct(start.clone()),
            reflog: Some(ReflogEntry {
                old_oid: zero,
                new_oid: start.clone(),
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
            return Err(GitError::NotFound(format!("branch {old_branch}")));
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
            target: RefTarget::Direct(oid.clone()),
        })?;
        let mut reflog = self.read_reflog(&old_name)?;
        reflog.push(ReflogEntry {
            old_oid: oid.clone(),
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
            new: RefTarget::Direct(target.clone()),
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
        let path = self.git_dir.join("packed-refs");
        if !path.exists() {
            return Err(GitError::NotFound(format!("{kind} {short_name}")));
        }
        let mut refs = parse_packed_refs(self.format, &fs::read(&path)?)?;
        let Some(index) = refs
            .iter()
            .position(|reference| reference.reference.name == name)
        else {
            return Err(GitError::NotFound(format!("{kind} {short_name}")));
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
        let path = self.git_dir.join("packed-refs");
        if !path.exists() {
            return Ok(None);
        }
        Ok(parse_packed_refs(self.format, &fs::read(path)?)?
            .into_iter()
            .find(|reference| reference.reference.name == name))
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
        self.git_dir.join(name)
    }

    fn reflog_path(&self, name: &str) -> PathBuf {
        self.git_dir.join("logs").join(name)
    }
}

pub struct FileRefTransaction<'a> {
    store: &'a FileRefStore,
    updates: Vec<RefUpdate>,
}

impl<'a> FileRefTransaction<'a> {
    pub fn update(&mut self, update: RefUpdate) {
        self.updates.push(update);
    }

    pub fn commit(self) -> Result<()> {
        for update in &self.updates {
            validate_ref_name(&update.name)?;
            if let Some(expected) = &update.expected
                && self.store.read_ref(&update.name)?.as_ref() != Some(expected)
            {
                return Err(GitError::Transaction(format!(
                    "expected ref {} to match",
                    update.name
                )));
            }
        }
        for update in self.updates {
            self.store.write_loose_ref(&Ref {
                name: update.name.clone(),
                target: update.new,
            })?;
            if let Some(entry) = update.reflog {
                self.store.append_reflog(&update.name, &entry)?;
            }
        }
        Ok(())
    }
}

pub fn branch_ref_name(branch: &str) -> Result<String> {
    if branch.is_empty()
        || branch.starts_with('-')
        || branch.starts_with('/')
        || branch.ends_with('/')
        || branch.contains(' ')
        || branch.contains('\\')
    {
        return Err(GitError::InvalidPath(format!(
            "invalid branch name {branch}"
        )));
    }
    let name = format!("refs/heads/{branch}");
    validate_ref_name(&name)?;
    Ok(name)
}

pub fn tag_ref_name(tag: &str) -> Result<String> {
    if tag.is_empty()
        || tag.starts_with('-')
        || tag.starts_with('/')
        || tag.ends_with('/')
        || tag.contains(' ')
        || tag.contains('\\')
    {
        return Err(GitError::InvalidPath(format!("invalid tag name {tag}")));
    }
    let name = format!("refs/tags/{tag}");
    validate_ref_name(&name)?;
    Ok(name)
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
                    Some(oid) => oid.clone(),
                    None => null_oid(bundle_ref.oid.format())?,
                },
                new_oid: bundle_ref.oid.clone(),
                committer: reflog.committer.clone(),
                message: reflog.message.clone(),
            }),
            None => None,
        };
        updates.push(RefUpdate {
            name: bundle_ref.name.clone(),
            expected: old_oid.clone().map(RefTarget::Direct),
            new: RefTarget::Direct(bundle_ref.oid.clone()),
            reflog,
        });
        applied.push(AppliedBundleRefUpdate {
            name: bundle_ref.name.clone(),
            old_oid,
            new_oid: bundle_ref.oid.clone(),
        });
    }
    Ok((updates, applied))
}

fn null_oid(format: ObjectFormat) -> Result<ObjectId> {
    ObjectId::from_raw(format, &vec![0; format.raw_len()])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn loose_ref_round_trips_direct() {
        let oid = "ce013625030ba8dba906f756967f9e9ca394464a";
        let reference =
            parse_loose_ref(ObjectFormat::Sha1, "refs/heads/main", oid.as_bytes()).unwrap();
        assert_eq!(write_loose_ref(&reference), format!("{oid}\n").into_bytes());
    }

    #[test]
    fn transaction_checks_expected_value() {
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .unwrap();
        let mut store = RefStore::new();
        let mut tx = store.transaction();
        tx.update(RefUpdate {
            name: "refs/heads/main".into(),
            expected: None,
            new: RefTarget::Direct(oid.clone()),
            reflog: None,
        });
        tx.commit().unwrap();
        assert_eq!(store.get("refs/heads/main"), Some(&RefTarget::Direct(oid)));
    }

    #[test]
    fn packed_refs_parse_peeled_refs() {
        let packed = b"# pack-refs with: peeled fully-peeled sorted \n\
ce013625030ba8dba906f756967f9e9ca394464a refs/tags/v1\n\
^e69de29bb2d1d6434b8b29ae775ad8c2e48c5391\n";
        let refs = parse_packed_refs(ObjectFormat::Sha1, packed).unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].reference.name, "refs/tags/v1");
        assert_eq!(
            refs[0].peeled.as_ref().unwrap().to_hex(),
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
        );
    }

    #[test]
    fn packed_refs_write_sorted_with_peeled_refs() {
        let head_oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .unwrap();
        let tag_oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "18f002b4484b838b205a48b1e9e6763ba5e3a607",
        )
        .unwrap();
        let peeled_oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391",
        )
        .unwrap();
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
        let bytes = write_packed_refs(&refs).unwrap();
        let expected = format!(
            "# pack-refs with: peeled fully-peeled sorted \n\
{head_oid} refs/heads/main\n\
{tag_oid} refs/tags/v1\n\
^{peeled_oid}\n"
        );
        assert_eq!(String::from_utf8(bytes.clone()).unwrap(), expected);
        let parsed = parse_packed_refs(ObjectFormat::Sha1, &bytes).unwrap();
        assert_eq!(parsed[0], refs[1]);
        assert_eq!(parsed[1], refs[0]);
    }

    #[test]
    fn file_ref_store_writes_ref_and_reflog() {
        let git_dir = temp_git_dir();
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .unwrap();
        let mut tx = store.transaction();
        tx.update(RefUpdate {
            name: "refs/heads/main".into(),
            expected: None,
            new: RefTarget::Direct(oid.clone()),
            reflog: Some(ReflogEntry {
                old_oid: zero_oid(ObjectFormat::Sha1).unwrap(),
                new_oid: oid.clone(),
                committer: b"Git Rs <git-rs@example.invalid> 0 +0000".to_vec(),
                message: b"update by test".to_vec(),
            }),
        });
        tx.commit().unwrap();
        assert_eq!(
            store.read_ref("refs/heads/main").unwrap(),
            Some(RefTarget::Direct(oid))
        );
        let log = store.read_reflog("refs/heads/main").unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].message, b"update by test");
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn file_ref_store_applies_bundle_refs_with_reflog() {
        let git_dir = temp_git_dir();
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let old_main = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .unwrap();
        let new_main = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391",
        )
        .unwrap();
        let tag_oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "18f002b4484b838b205a48b1e9e6763ba5e3a607",
        )
        .unwrap();
        let mut tx = store.transaction();
        tx.update(RefUpdate {
            name: "refs/heads/main".into(),
            expected: None,
            new: RefTarget::Direct(old_main.clone()),
            reflog: None,
        });
        tx.commit().unwrap();

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
                    committer: b"Git Rs <git-rs@example.invalid> 0 +0000".to_vec(),
                    message: b"bundle: import refs".to_vec(),
                }),
            )
            .unwrap();

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
            store.read_ref("refs/heads/main").unwrap(),
            Some(RefTarget::Direct(new_main.clone()))
        );
        assert_eq!(
            store.read_ref("refs/tags/v1.0").unwrap(),
            Some(RefTarget::Direct(tag_oid.clone()))
        );
        let main_log = store.read_reflog("refs/heads/main").unwrap();
        assert_eq!(main_log.len(), 1);
        assert_eq!(main_log[0].old_oid, old_main);
        assert_eq!(main_log[0].new_oid, new_main);
        assert_eq!(main_log[0].message, b"bundle: import refs");
        let tag_log = store.read_reflog("refs/tags/v1.0").unwrap();
        assert_eq!(tag_log.len(), 1);
        assert_eq!(tag_log[0].old_oid, zero_oid(ObjectFormat::Sha1).unwrap());
        assert_eq!(tag_log[0].new_oid, tag_oid);
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn file_ref_store_rejects_bad_bundle_ref_before_writing() {
        let git_dir = temp_git_dir();
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .unwrap();

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
        assert_eq!(store.read_ref("refs/heads/main").unwrap(), None);
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn file_ref_store_rejects_bundle_ref_over_symbolic_ref() {
        let git_dir = temp_git_dir();
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .unwrap();
        let mut tx = store.transaction();
        tx.update(RefUpdate {
            name: "refs/heads/main".into(),
            expected: None,
            new: RefTarget::Symbolic("refs/heads/base".into()),
            reflog: None,
        });
        tx.commit().unwrap();

        let result = store.apply_bundle_ref_updates(
            &[BundleRefUpdate {
                name: "refs/heads/main".into(),
                oid,
            }],
            None,
        );

        assert!(result.is_err());
        assert_eq!(
            store.read_ref("refs/heads/main").unwrap(),
            Some(RefTarget::Symbolic("refs/heads/base".into()))
        );
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn file_ref_store_expires_reflog_entries_by_timestamp() {
        let git_dir = temp_git_dir();
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let first = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .unwrap();
        let second = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391",
        )
        .unwrap();
        let mut tx = store.transaction();
        tx.update(RefUpdate {
            name: "refs/heads/main".into(),
            expected: None,
            new: RefTarget::Direct(first.clone()),
            reflog: Some(ReflogEntry {
                old_oid: zero_oid(ObjectFormat::Sha1).unwrap(),
                new_oid: first.clone(),
                committer: b"Git Rs <git-rs@example.invalid> 0 +0000".to_vec(),
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
                committer: b"Git Rs <git-rs@example.invalid> 100 +0000".to_vec(),
                message: b"new".to_vec(),
            }),
        });
        tx.commit().unwrap();

        let removed = store
            .expire_reflog_older_than("refs/heads/main", 50)
            .unwrap();
        assert_eq!(removed, 1);
        let log = store.read_reflog("refs/heads/main").unwrap();
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
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn file_ref_store_creates_branch() {
        let git_dir = temp_git_dir();
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .unwrap();
        let branch = store
            .create_branch(
                "feature",
                oid.clone(),
                b"Git Rs <git-rs@example.invalid> 0 +0000".to_vec(),
                b"branch: Created from main".to_vec(),
            )
            .unwrap();
        assert_eq!(branch.name, "refs/heads/feature");
        assert_eq!(
            store.read_ref("refs/heads/feature").unwrap(),
            Some(RefTarget::Direct(oid))
        );
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn file_ref_store_deletes_loose_branch() {
        let git_dir = temp_git_dir();
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .unwrap();
        store
            .create_branch(
                "feature",
                oid.clone(),
                b"Git Rs <git-rs@example.invalid> 0 +0000".to_vec(),
                b"branch: Created from main".to_vec(),
            )
            .unwrap();
        let deleted = store.delete_branch("feature").unwrap();
        assert_eq!(deleted.name, "refs/heads/feature");
        assert_eq!(deleted.oid, oid);
        assert_eq!(store.read_ref("refs/heads/feature").unwrap(), None);
        assert!(!git_dir.join("refs").join("heads").join("feature").exists());
        assert!(
            !git_dir
                .join("logs")
                .join("refs")
                .join("heads")
                .join("feature")
                .exists()
        );
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn file_ref_store_deletes_generic_loose_ref() {
        let git_dir = temp_git_dir();
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .unwrap();
        let mut tx = store.transaction();
        tx.update(RefUpdate {
            name: "refs/heads/topic".into(),
            expected: None,
            new: RefTarget::Direct(oid.clone()),
            reflog: Some(ReflogEntry {
                old_oid: zero_oid(ObjectFormat::Sha1).unwrap(),
                new_oid: oid.clone(),
                committer: b"Git Rs <git-rs@example.invalid> 0 +0000".to_vec(),
                message: b"update by test".to_vec(),
            }),
        });
        tx.commit().unwrap();
        let deleted = store.delete_ref("refs/heads/topic").unwrap();
        assert_eq!(deleted.name, "refs/heads/topic");
        assert_eq!(deleted.oid, oid);
        assert_eq!(store.read_ref("refs/heads/topic").unwrap(), None);
        assert!(!git_dir.join("refs").join("heads").join("topic").exists());
        assert!(
            !git_dir
                .join("logs")
                .join("refs")
                .join("heads")
                .join("topic")
                .exists()
        );
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn file_ref_store_reports_current_branch() {
        let git_dir = temp_git_dir();
        fs::write(git_dir.join("HEAD"), b"ref: refs/heads/main\n").unwrap();
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        assert_eq!(
            store.current_branch_ref().unwrap(),
            Some("refs/heads/main".into())
        );
        assert_eq!(store.current_branch().unwrap(), Some("main".into()));
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn file_ref_store_creates_tag() {
        let git_dir = temp_git_dir();
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .unwrap();
        let tag = store.create_tag("v1.0", oid.clone()).unwrap();
        assert_eq!(tag.name, "refs/tags/v1.0");
        assert_eq!(
            store.read_ref("refs/tags/v1.0").unwrap(),
            Some(RefTarget::Direct(oid))
        );
        assert!(store.read_reflog("refs/tags/v1.0").unwrap().is_empty());
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn file_ref_store_deletes_loose_tag() {
        let git_dir = temp_git_dir();
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .unwrap();
        store.create_tag("v1.0", oid.clone()).unwrap();
        let deleted = store.delete_tag("v1.0").unwrap();
        assert_eq!(deleted.name, "refs/tags/v1.0");
        assert_eq!(deleted.oid, oid);
        assert_eq!(store.read_ref("refs/tags/v1.0").unwrap(), None);
        assert!(!git_dir.join("refs").join("tags").join("v1.0").exists());
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn file_ref_store_reads_packed_ref() {
        let git_dir = temp_git_dir();
        fs::write(
            git_dir.join("packed-refs"),
            b"ce013625030ba8dba906f756967f9e9ca394464a refs/heads/main\n",
        )
        .unwrap();
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        assert!(matches!(
            store.read_ref("refs/heads/main").unwrap(),
            Some(RefTarget::Direct(_))
        ));
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn file_ref_store_lists_loose_refs_over_packed_refs() {
        let git_dir = temp_git_dir();
        fs::write(
            git_dir.join("packed-refs"),
            b"e69de29bb2d1d6434b8b29ae775ad8c2e48c5391 refs/heads/main\n",
        )
        .unwrap();
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .unwrap();
        let mut tx = store.transaction();
        tx.update(RefUpdate {
            name: "refs/heads/main".into(),
            expected: None,
            new: RefTarget::Direct(oid.clone()),
            reflog: None,
        });
        tx.commit().unwrap();
        let refs = store.list_refs().unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].target, RefTarget::Direct(oid));
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn file_ref_store_writes_packed_refs() {
        let git_dir = temp_git_dir();
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .unwrap();
        store
            .write_packed_refs(&[PackedRef {
                reference: Ref {
                    name: "refs/heads/main".into(),
                    target: RefTarget::Direct(oid.clone()),
                },
                peeled: None,
            }])
            .unwrap();
        assert_eq!(
            store.read_ref("refs/heads/main").unwrap(),
            Some(RefTarget::Direct(oid.clone()))
        );
        let refs = store.list_refs().unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].target, RefTarget::Direct(oid));
        assert!(git_dir.join("packed-refs").exists());
        assert!(!git_dir.join("packed-refs.lock").exists());
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn file_ref_store_deletes_packed_branch() {
        let git_dir = temp_git_dir();
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let branch_oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .unwrap();
        let tag_oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391",
        )
        .unwrap();
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
            .unwrap();
        let deleted = store.delete_branch("feature").unwrap();
        assert_eq!(deleted.name, "refs/heads/feature");
        assert_eq!(deleted.oid, branch_oid);
        assert_eq!(store.read_ref("refs/heads/feature").unwrap(), None);
        assert_eq!(
            store.read_ref("refs/tags/v1.0").unwrap(),
            Some(RefTarget::Direct(tag_oid))
        );
        assert!(!git_dir.join("packed-refs.lock").exists());
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn file_ref_store_deletes_packed_tag() {
        let git_dir = temp_git_dir();
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .unwrap();
        store
            .write_packed_refs(&[PackedRef {
                reference: Ref {
                    name: "refs/tags/v1.0".into(),
                    target: RefTarget::Direct(oid.clone()),
                },
                peeled: None,
            }])
            .unwrap();
        let deleted = store.delete_tag("v1.0").unwrap();
        assert_eq!(deleted.name, "refs/tags/v1.0");
        assert_eq!(deleted.oid, oid);
        assert_eq!(store.read_ref("refs/tags/v1.0").unwrap(), None);
        assert!(!git_dir.join("packed-refs.lock").exists());
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn file_ref_store_packs_loose_refs_and_prunes() {
        let git_dir = temp_git_dir();
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let main_oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .unwrap();
        let tag_oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391",
        )
        .unwrap();
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
        tx.commit().unwrap();

        let packed = store.pack_refs(true).unwrap();
        assert_eq!(packed.len(), 2);
        assert_eq!(
            store.read_ref("refs/heads/main").unwrap(),
            Some(RefTarget::Direct(main_oid))
        );
        assert_eq!(
            store.read_ref("refs/tags/v1.0").unwrap(),
            Some(RefTarget::Direct(tag_oid))
        );
        assert!(!git_dir.join("refs").join("heads").join("main").exists());
        assert!(!git_dir.join("refs").join("tags").join("v1.0").exists());
        assert!(git_dir.join("packed-refs").exists());
        assert!(!git_dir.join("packed-refs.lock").exists());
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn file_ref_store_packs_loose_refs_without_pruning() {
        let git_dir = temp_git_dir();
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .unwrap();
        let mut tx = store.transaction();
        tx.update(RefUpdate {
            name: "refs/heads/main".into(),
            expected: None,
            new: RefTarget::Direct(oid.clone()),
            reflog: None,
        });
        tx.commit().unwrap();

        let packed = store.pack_refs(false).unwrap();
        assert_eq!(packed.len(), 1);
        assert!(git_dir.join("refs").join("heads").join("main").exists());
        assert_eq!(
            store.read_ref("refs/heads/main").unwrap(),
            Some(RefTarget::Direct(oid))
        );
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn file_ref_store_packs_loose_refs_with_peeled_ids() {
        let git_dir = temp_git_dir();
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let tag_oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .unwrap();
        let peeled_oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391",
        )
        .unwrap();
        let mut tx = store.transaction();
        tx.update(RefUpdate {
            name: "refs/tags/v1.0".into(),
            expected: None,
            new: RefTarget::Direct(tag_oid.clone()),
            reflog: None,
        });
        tx.commit().unwrap();

        let packed = store
            .pack_refs_with_peeler(true, |name, oid| {
                if name == "refs/tags/v1.0" && oid == &tag_oid {
                    Ok(Some(peeled_oid.clone()))
                } else {
                    Ok(None)
                }
            })
            .unwrap();
        assert_eq!(packed.len(), 1);
        assert_eq!(packed[0].peeled, Some(peeled_oid.clone()));
        let bytes = fs::read_to_string(git_dir.join("packed-refs")).unwrap();
        assert!(bytes.contains(&format!("^{peeled_oid}\n")));
        assert!(!git_dir.join("refs").join("tags").join("v1.0").exists());
        fs::remove_dir_all(git_dir).unwrap();
    }

    fn temp_git_dir() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "git-rs-refs-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn zero_oid(format: ObjectFormat) -> Result<ObjectId> {
        ObjectId::from_raw(format, &vec![0; format.raw_len()])
    }
}
