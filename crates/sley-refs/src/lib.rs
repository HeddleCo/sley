// sley#7: untrusted-input parsing crate — fallible ops propagate errors;
// the only retained `expect`s would be documented compile-time invariants.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use sley_config::GitConfig;
use sley_core::{GitError, ObjectFormat, ObjectId, Result};
use sley_formats::{
    Reftable, ReftableLogRecord, ReftableLogUpdate, ReftableLogValue, ReftableRefRecord,
    ReftableRefValue, ReftableWriteOptions,
};
use std::borrow::Borrow;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::env;
use std::fmt;
use std::fs;
use std::io::Write;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;
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

/// A backend-independent export used to move all live refs and reflogs as one
/// logical operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefSnapshot {
    pub refs: Vec<Ref>,
    pub reflogs: Vec<(String, Vec<ReflogEntry>)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefDelete {
    pub name: String,
    pub oid: ObjectId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteRef {
    pub name: String,
    pub expected_old: Option<ObjectId>,
    pub reflog: Option<DeleteRefReflog>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteRefReflog {
    pub committer: Vec<u8>,
    pub message: Vec<u8>,
}

#[derive(Debug)]
pub enum RefDeleteError {
    NotFound,
    ExpectedMismatch {
        expected: Option<ObjectId>,
        actual: Option<ObjectId>,
    },
    Locked,
    InvalidName,
    Io(std::io::Error),
}

impl fmt::Display for RefDeleteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => f.write_str("ref not found"),
            Self::ExpectedMismatch { expected, actual } => {
                write!(
                    f,
                    "ref expected old oid mismatch: expected {:?}, actual {:?}",
                    expected, actual
                )
            }
            Self::Locked => f.write_str("ref is locked"),
            Self::InvalidName => f.write_str("invalid ref name"),
            Self::Io(err) => write!(f, "io error: {err}"),
        }
    }
}

impl std::error::Error for RefDeleteError {}

impl From<std::io::Error> for RefDeleteError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

/// Parse the leading object id of a loose ref body, tolerating any trailing
/// content that begins with whitespace (git's `parse_loose_ref_contents`).
fn parse_leading_oid(format: ObjectFormat, value: &str) -> Option<ObjectId> {
    let hexsz = format.hex_len();
    let bytes = value.as_bytes();
    if bytes.len() < hexsz {
        return None;
    }
    if bytes.len() > hexsz && !bytes[hexsz].is_ascii_whitespace() {
        return None;
    }
    ObjectId::from_hex(format, &value[..hexsz]).ok()
}

fn loose_ref_bytes_are_reftable_sentinel(name: &str, bytes: &[u8]) -> bool {
    name == "refs/heads" && bytes == b"this repository uses the reftable format\n"
}

pub fn parse_loose_ref(format: ObjectFormat, name: impl Into<String>, bytes: &[u8]) -> Result<Ref> {
    let name = name.into();
    let value = std::str::from_utf8(bytes)
        .map_err(|err| GitError::InvalidFormat(err.to_string()))?
        .trim_end_matches('\n');
    if name == "FETCH_HEAD" {
        let oid = value
            .lines()
            .find_map(|line| line.split_whitespace().next())
            .ok_or_else(|| GitError::InvalidFormat("FETCH_HEAD is empty".into()))?;
        return Ok(Ref {
            name,
            target: RefTarget::Direct(ObjectId::from_hex(format, oid)?),
        });
    }
    let target = if let Some(symbolic) = value.strip_prefix("ref: ") {
        RefTarget::Symbolic(symbolic.to_string())
    } else {
        // git's parse_loose_ref_contents reads the leading <hexsz> hex digits
        // and tolerates trailing content as long as it begins with whitespace
        // (a bare "<oid> garbage" still resolves to <oid>; `refs verify` flags
        // the trailing separately as trailingRefContent).
        let oid = parse_leading_oid(format, value).ok_or_else(|| {
            GitError::InvalidFormat(format!(
                "reference {name} has neither a valid OID nor a target"
            ))
        })?;
        RefTarget::Direct(oid)
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackRefDecision {
    Pack { peeled: Option<ObjectId> },
    Skip,
}

pub fn parse_packed_refs(format: ObjectFormat, bytes: &[u8]) -> Result<Vec<PackedRef>> {
    parse_packed_refs_filtered(format, bytes, |_| true)
}

fn parse_packed_refs_with_prefix(
    format: ObjectFormat,
    bytes: &[u8],
    prefix: &str,
) -> Result<Vec<PackedRef>> {
    if !bytes.is_empty() && !bytes.ends_with(b"\n") {
        let line_start = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |index| index + 1);
        let line = String::from_utf8_lossy(&bytes[line_start..]);
        return Err(GitError::InvalidFormat(format!(
            "fatal: unterminated line in .git/packed-refs: {line}"
        )));
    }
    let text =
        std::str::from_utf8(bytes).map_err(|err| GitError::InvalidFormat(err.to_string()))?;
    let mut refs: Vec<PackedRef> = Vec::new();
    let mut saw_ref = false;
    let mut included_last_ref = false;
    for raw_line in text.lines() {
        let line = raw_line.trim_end();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(peeled) = line.strip_prefix('^') {
            if !saw_ref {
                return Err(GitError::InvalidFormat(
                    "peeled packed ref without preceding ref".into(),
                ));
            }
            if included_last_ref {
                let oid = ObjectId::from_hex(format, peeled)?;
                if let Some(last) = refs.last_mut() {
                    last.peeled = Some(oid);
                }
            }
            continue;
        }
        let (oid, name) = line
            .split_once(' ')
            .ok_or_else(|| packed_refs_unexpected_line(line))?;
        // git validates EVERY packed-refs line as it reads the file, regardless
        // of whether the ref matches the caller's prefix; a malformed line
        // anywhere aborts with "unexpected line in .git/packed-refs" (t1463
        // "reject packed-refs containing junk"). So validate before applying the
        // prefix filter — skipping validation for non-matching lines would let
        // junk pass silently.
        if oid.len() != format.hex_len()
            || !oid.bytes().all(|byte| byte.is_ascii_hexdigit())
            || validate_ref_name_for_read(name).is_err()
        {
            return Err(packed_refs_unexpected_line(line));
        }
        saw_ref = true;
        included_last_ref = name.starts_with(prefix);
        if !included_last_ref {
            continue;
        }
        let oid = ObjectId::from_hex(format, oid)?;
        refs.push(PackedRef {
            reference: Ref {
                name: name.into(),
                target: RefTarget::Direct(oid),
            },
            peeled: None,
        });
    }
    Ok(refs)
}

fn parse_packed_refs_filtered(
    format: ObjectFormat,
    bytes: &[u8],
    mut include: impl FnMut(&str) -> bool,
) -> Result<Vec<PackedRef>> {
    if !bytes.is_empty() && !bytes.ends_with(b"\n") {
        let line_start = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |index| index + 1);
        let line = String::from_utf8_lossy(&bytes[line_start..]);
        return Err(GitError::InvalidFormat(format!(
            "fatal: unterminated line in .git/packed-refs: {line}"
        )));
    }
    let text =
        std::str::from_utf8(bytes).map_err(|err| GitError::InvalidFormat(err.to_string()))?;
    let mut refs: Vec<PackedRef> = Vec::new();
    let mut saw_ref = false;
    let mut included_last_ref = false;
    for raw_line in text.lines() {
        let line = raw_line.trim_end();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(peeled) = line.strip_prefix('^') {
            let oid = ObjectId::from_hex(format, peeled)?;
            if !saw_ref {
                return Err(GitError::InvalidFormat(
                    "peeled packed ref without preceding ref".into(),
                ));
            }
            if included_last_ref && let Some(last) = refs.last_mut() {
                last.peeled = Some(oid);
            }
            continue;
        }
        let (oid, name) = line
            .split_once(' ')
            .ok_or_else(|| packed_refs_unexpected_line(line))?;
        if oid.len() != format.hex_len()
            || !oid.bytes().all(|byte| byte.is_ascii_hexdigit())
            || validate_ref_name_for_read(name).is_err()
        {
            return Err(packed_refs_unexpected_line(line));
        }
        let oid = ObjectId::from_hex(format, oid)?;
        saw_ref = true;
        included_last_ref = include(name);
        if included_last_ref {
            refs.push(PackedRef {
                reference: Ref {
                    name: name.into(),
                    target: RefTarget::Direct(oid),
                },
                peeled: None,
            });
        }
    }
    Ok(refs)
}

fn packed_ref_names_with_prefix(
    format: ObjectFormat,
    bytes: &[u8],
    prefix: &str,
    strip_prefix: bool,
    names: &mut Vec<String>,
) -> Result<()> {
    if !bytes.is_empty() && !bytes.ends_with(b"\n") {
        let line_start = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |index| index + 1);
        let line = String::from_utf8_lossy(&bytes[line_start..]);
        return Err(GitError::InvalidFormat(format!(
            "fatal: unterminated line in .git/packed-refs: {line}"
        )));
    }
    let text =
        std::str::from_utf8(bytes).map_err(|err| GitError::InvalidFormat(err.to_string()))?;
    let mut saw_ref = false;
    for raw_line in text.lines() {
        let line = raw_line.trim_end();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('^') {
            if !saw_ref {
                return Err(GitError::InvalidFormat(
                    "peeled packed ref without preceding ref".into(),
                ));
            }
            continue;
        }
        let (oid, name) = line
            .split_once(' ')
            .ok_or_else(|| packed_refs_unexpected_line(line))?;
        // Validate every line before the prefix filter (see
        // `parse_packed_refs_with_prefix`): git rejects a malformed packed-refs
        // file even when the junk line is outside the requested prefix.
        if oid.len() != format.hex_len()
            || !oid.bytes().all(|byte| byte.is_ascii_hexdigit())
            || validate_ref_name_for_read(name).is_err()
        {
            return Err(packed_refs_unexpected_line(line));
        }
        saw_ref = true;
        if !name.starts_with(prefix) {
            continue;
        }
        let name = if strip_prefix {
            &name[prefix.len()..]
        } else {
            name
        };
        names.push(name.to_owned());
    }
    Ok(())
}

fn packed_refs_unexpected_line(line: &str) -> GitError {
    GitError::InvalidFormat(format!(
        "fatal: unexpected line in .git/packed-refs: {line}"
    ))
}

fn packed_refs_have_prefix(format: ObjectFormat, bytes: &[u8], prefix: &str) -> Result<bool> {
    if !bytes.is_empty() && !bytes.ends_with(b"\n") {
        let line_start = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |index| index + 1);
        let line = String::from_utf8_lossy(&bytes[line_start..]);
        return Err(GitError::InvalidFormat(format!(
            "fatal: unterminated line in .git/packed-refs: {line}"
        )));
    }
    let text =
        std::str::from_utf8(bytes).map_err(|err| GitError::InvalidFormat(err.to_string()))?;
    let mut found = false;
    let mut saw_ref = false;
    for raw_line in text.lines() {
        let line = raw_line.trim_end();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(peeled) = line.strip_prefix('^') {
            ObjectId::from_hex(format, peeled)?;
            if !saw_ref {
                return Err(GitError::InvalidFormat(
                    "peeled packed ref without preceding ref".into(),
                ));
            }
            continue;
        }
        let (oid, name) = line
            .split_once(' ')
            .ok_or_else(|| packed_refs_unexpected_line(line))?;
        if oid.len() != format.hex_len()
            || !oid.bytes().all(|byte| byte.is_ascii_hexdigit())
            || validate_ref_name_for_read(name).is_err()
        {
            return Err(packed_refs_unexpected_line(line));
        }
        ObjectId::from_hex(format, oid)?;
        saw_ref = true;
        found |= name.starts_with(prefix);
    }
    Ok(found)
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
    storage_dir: PathBuf,
    format: ObjectFormat,
    ref_storage: RefStorageConfiguration,
    reftable_lock_timeout_millis: Option<u64>,
    reftable_write_options: ReftableWriteOptions,
    combine_reftable_logs: bool,
    reference_fsync: ReferenceFsyncPolicy,
}

#[derive(Debug, Clone)]
struct RefStorageConfiguration {
    backend: Option<(RefBackendKind, Option<PathBuf>)>,
    invalid_value: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReferenceFsyncMethod {
    Fsync,
    WriteoutOnly,
    Batch,
}

impl ReferenceFsyncMethod {
    const fn platform_default() -> Self {
        #[cfg(target_os = "windows")]
        {
            Self::Batch
        }
        #[cfg(all(not(target_os = "windows"), target_os = "macos"))]
        {
            Self::WriteoutOnly
        }
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        {
            Self::Fsync
        }
    }

    fn from_config(value: Option<&str>) -> Self {
        match value {
            Some("fsync") => Self::Fsync,
            Some("writeout-only") => Self::WriteoutOnly,
            Some("batch") => Self::Batch,
            _ => Self::platform_default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ReferenceFsyncPolicy {
    enabled: bool,
    method: ReferenceFsyncMethod,
}

impl ReferenceFsyncPolicy {
    fn from_config(core_fsync: Option<&str>, core_fsync_method: Option<&str>) -> Self {
        let enabled =
            core_fsync.is_some_and(core_fsync_includes_reference) && git_test_fsync_enabled();
        Self {
            enabled,
            method: ReferenceFsyncMethod::from_config(core_fsync_method),
        }
    }

    fn method_if_enabled(self) -> Option<ReferenceFsyncMethod> {
        self.enabled.then_some(self.method)
    }
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

fn configured_ref_storage_backend(common_dir: &Path, honor_env: bool) -> RefStorageConfiguration {
    if honor_env && let Ok(value) = env::var("GIT_REFERENCE_BACKEND") {
        return match parse_ref_storage_backend_value(&value) {
            Ok((kind, path)) => RefStorageConfiguration {
                backend: Some((
                    kind,
                    path.map(|path| ref_storage_path_from_config(common_dir, path)),
                )),
                invalid_value: None,
            },
            Err(_) => RefStorageConfiguration {
                backend: None,
                invalid_value: Some(value),
            },
        };
    }
    let value = GitConfig::read(common_dir.join("config"))
        .ok()
        .and_then(|config| {
            config
                .get("extensions", None, "refStorage")
                .map(str::to_string)
        });
    let Some(value) = value else {
        return RefStorageConfiguration {
            backend: None,
            invalid_value: None,
        };
    };
    match parse_ref_storage_backend_value(&value) {
        Ok((kind, path)) => RefStorageConfiguration {
            backend: Some((
                kind,
                path.map(|path| ref_storage_path_from_config(common_dir, path)),
            )),
            invalid_value: None,
        },
        Err(_) => RefStorageConfiguration {
            backend: None,
            invalid_value: Some(value),
        },
    }
}

fn configured_reference_fsync(common_dir: &Path) -> ReferenceFsyncPolicy {
    let context = sley_config::ConfigIncludeContext::new(
        Some(sley_config::git_dir_for_include_context(common_dir)),
        sley_config::repo_current_branch_name(common_dir),
    );
    let config = sley_config::load_effective_config(common_dir, &context).ok();
    ReferenceFsyncPolicy::from_config(
        config
            .as_ref()
            .and_then(|config| config.get("core", None, "fsync")),
        config
            .as_ref()
            .and_then(|config| config.get("core", None, "fsyncMethod")),
    )
}

fn core_fsync_includes_reference(value: &str) -> bool {
    const COMPONENTS: [(&str, bool); 11] = [
        ("loose-object", false),
        ("pack", false),
        ("pack-metadata", false),
        ("commit-graph", false),
        ("index", false),
        ("objects", false),
        ("reference", true),
        ("derived-metadata", false),
        ("committed", true),
        ("added", true),
        ("all", true),
    ];
    let mut positive = false;
    let mut negative = false;
    for raw_component in value.split(',') {
        let component = raw_component.trim();
        if component == "none" || component.is_empty() {
            continue;
        }
        let (negated, component) = component
            .strip_prefix('-')
            .map_or((false, component), |component| (true, component));
        if component.is_empty() {
            continue;
        }
        let matches_reference = COMPONENTS
            .iter()
            .any(|(name, includes_reference)| *includes_reference && name.starts_with(component));
        if matches_reference {
            if negated {
                negative = true;
            } else {
                positive = true;
            }
        }
    }
    // Git starts from FSYNC_COMPONENTS_DEFAULT (which excludes references),
    // then applies accumulated negatives and positives. Positives win when the
    // same component appears in both sets.
    (platform_default_includes_reference_fsync() && !negative) || positive
}

fn platform_default_includes_reference_fsync() -> bool {
    // FSYNC_COMPONENTS_DEFAULT excludes reference files on every platform in
    // Git v2.55. Keep this as a function because Git exposes the default as a
    // platform-overridable compile-time policy.
    false
}

fn git_test_fsync_enabled() -> bool {
    let Ok(value) = env::var("GIT_TEST_FSYNC") else {
        return true;
    };
    !matches!(
        value.to_ascii_lowercase().as_str(),
        "0" | "false" | "no" | "off" | ""
    )
}

fn ref_storage_path_from_config(common_dir: &Path, path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        common_dir.join(path)
    }
}

fn parse_ref_storage_backend_value(value: &str) -> Result<(RefBackendKind, Option<PathBuf>)> {
    let (backend, path) = if let Some((backend, path)) = value.split_once("://") {
        if path.is_empty() {
            return Err(GitError::InvalidFormat(format!(
                "invalid value for 'extensions.refstorage': '{value}'"
            )));
        }
        (backend, Some(PathBuf::from(path)))
    } else {
        (value, None)
    };
    let kind = match backend {
        "files" => RefBackendKind::Files,
        "reftable" => RefBackendKind::Reftable,
        _ => {
            return Err(GitError::InvalidFormat(format!(
                "invalid value for 'extensions.refstorage': '{value}'"
            )));
        }
    };
    Ok((kind, path))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RefBackendKind {
    Files,
    Reftable,
}

impl FileRefStore {
    pub fn new(git_dir: impl Into<PathBuf>, format: ObjectFormat) -> Self {
        Self::new_with_reference_backend_env(git_dir, format, true)
    }

    pub fn new_without_reference_backend_env(
        git_dir: impl Into<PathBuf>,
        format: ObjectFormat,
    ) -> Self {
        Self::new_with_reference_backend_env(git_dir, format, false)
    }

    fn new_with_reference_backend_env(
        git_dir: impl Into<PathBuf>,
        format: ObjectFormat,
        honor_reference_backend_env: bool,
    ) -> Self {
        let git_dir = git_dir.into();
        let common_dir = repository_common_dir(&git_dir);
        let configured = configured_ref_storage_backend(&common_dir, honor_reference_backend_env);
        let reference_fsync = configured_reference_fsync(&common_dir);
        let storage_dir = match configured.backend.as_ref() {
            Some((_, Some(path))) => path.clone(),
            Some((RefBackendKind::Reftable, None)) if git_dir != common_dir => git_dir.clone(),
            _ => common_dir.clone(),
        };
        Self {
            git_dir,
            common_dir,
            storage_dir,
            format,
            ref_storage: configured,
            reftable_lock_timeout_millis: None,
            reftable_write_options: ReftableWriteOptions::default(),
            combine_reftable_logs: false,
            reference_fsync,
        }
    }

    pub fn with_reftable_lock_timeout_millis(mut self, timeout_millis: Option<u64>) -> Self {
        self.reftable_lock_timeout_millis = timeout_millis;
        self
    }

    /// Override the writer settings used for newly appended or compacted
    /// reftable tables.
    pub fn with_reftable_write_options(mut self, options: ReftableWriteOptions) -> Self {
        self.reftable_write_options = options;
        self
    }

    pub fn with_reftable_combined_logs(mut self, combine: bool) -> Self {
        self.combine_reftable_logs = combine;
        self
    }

    /// Override the effective `core.fsync` settings for reference writes.
    ///
    /// The CLI uses this for command-line `-c` values, which live outside the
    /// process environment. Passing `None` for either value retains Git's
    /// platform default for that setting.
    pub fn with_reference_fsync_config(
        mut self,
        core_fsync: Option<&str>,
        core_fsync_method: Option<&str>,
    ) -> Self {
        if core_fsync.is_some() || core_fsync_method.is_some() {
            let enabled = core_fsync
                .map(core_fsync_includes_reference)
                .unwrap_or(self.reference_fsync.enabled)
                && git_test_fsync_enabled();
            let method = core_fsync_method.map_or(self.reference_fsync.method, |value| {
                ReferenceFsyncMethod::from_config(Some(value))
            });
            self.reference_fsync = ReferenceFsyncPolicy { enabled, method };
        }
        self
    }

    pub fn read_ref(&self, name: &str) -> Result<Option<RefTarget>> {
        validate_ref_name_for_read(name)?;
        self.read_ref_unchecked(name)
    }

    fn read_ref_unchecked(&self, name: &str) -> Result<Option<RefTarget>> {
        if self.uses_reftable()? {
            let (store, name) = self.reftable_store_for_ref(name)?;
            if let Some(target) = store.read_reftable_ref(&name)? {
                return Ok(Some(target));
            }
            if name != "HEAD" && is_root_ref_syntax(&name) {
                return Ok(self
                    .read_loose_ref(&name)?
                    .map(|reference| reference.target));
            }
            return Ok(None);
        }
        if let Some(reference) = self.read_loose_ref(name)? {
            return Ok(Some(reference.target));
        }
        if let Some(reference) = self.read_packed_ref(name)? {
            return Ok(Some(reference.reference.target));
        }
        Ok(None)
    }

    /// Raw existence check matching git's `refs_read_raw_ref` (builtin/refs.c
    /// cmd_refs_exists). A ref "exists" if its loose file is present (regardless
    /// of contents — dangling symrefs, bad object ids, and refs written with a
    /// bad name all count) or if it is recorded in packed-refs / the reftable.
    /// Unlike [`Self::read_ref`], no name validation is performed and the object the
    /// ref points at is never read. Returns:
    ///   * `Ok(true)`  — the raw ref exists.
    ///   * `Ok(false)` — ENOENT or EISDIR (a bare directory where the ref would
    ///     live and no packed entry); git maps both to exit code 2.
    pub fn raw_ref_exists(&self, name: &str) -> Result<bool> {
        if self.uses_reftable()? {
            let (store, name) = self.reftable_store_for_ref(name)?;
            if store.read_reftable_ref(&name)?.is_some() {
                return Ok(true);
            }
            if name != "HEAD" && is_root_ref_syntax(&name) {
                return Ok(self.read_loose_ref(&name)?.is_some());
            }
            return Ok(false);
        }
        // git routes root-ref-syntax names (HEAD, FETCH_HEAD, MERGE_HEAD, …) and
        // per-worktree namespaces to the per-worktree gitdir; mirror
        // files_ref_path's REF_WORKTREE_CURRENT vs REF_WORKTREE_SHARED split.
        let base = if current_worktree_ref(name) {
            self.current_worktree_storage_dir()
        } else {
            self.storage_dir.clone()
        };
        let path = base.join(name);
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_dir() => {
                // A directory at the loose path is EISDIR unless packed-refs
                // still carries the name.
                Ok(self.read_packed_ref(name)?.is_some())
            }
            Ok(_) => Ok(true),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                Ok(self.read_packed_ref(name)?.is_some())
            }
            Err(err) => Err(err.into()),
        }
    }

    pub fn read_reflog(&self, name: &str) -> Result<Vec<ReflogEntry>> {
        validate_ref_name_for_read(name)?;
        if self.uses_reftable()? {
            return self.read_reftable_logs(name);
        }
        let path = self.reflog_path(name);
        if !path.exists() {
            return Ok(Vec::new());
        }
        parse_reflog(self.format, &fs::read(path)?)
    }

    pub fn reflog_exists(&self, name: &str) -> Result<bool> {
        validate_ref_name_for_read(name)?;
        if self.uses_reftable()? {
            return self.reftable_log_exists(name);
        }
        Ok(self.reflog_path(name).exists())
    }

    pub fn should_write_reflog_for_update(&self, name: &str, create_reflog: bool) -> Result<bool> {
        validate_ref_name_for_read(name)?;
        let reflog_exists = if self.uses_reftable()? {
            !self.read_reftable_logs(name)?.is_empty()
        } else {
            self.reflog_path(name).exists()
        };
        if create_reflog || reflog_exists {
            return Ok(true);
        }
        let context = sley_config::ConfigIncludeContext::new(
            Some(sley_config::git_dir_for_include_context(&self.common_dir)),
            sley_config::repo_current_branch_name(&self.common_dir),
        );
        let Ok(config) = sley_config::load_effective_config(&self.common_dir, &context) else {
            return Ok(false);
        };
        if let Some(value) = config.get("core", None, "logAllRefUpdates") {
            return Ok(log_all_ref_updates_matches(name, value));
        }
        if config.get_bool("core", None, "bare").unwrap_or(false) {
            return Ok(false);
        }
        Ok(log_all_ref_updates_matches(name, "true"))
    }

    pub fn write_reflog(&self, name: &str, entries: &[ReflogEntry]) -> Result<()> {
        validate_ref_name_for_read(name)?;
        if self.uses_reftable()? {
            return self.rewrite_reftable_logs(name, entries, true);
        }
        let path = self.reflog_path(name);
        let parent = path
            .parent()
            .ok_or_else(|| GitError::InvalidPath("reflog path has no parent".into()))?;
        fs::create_dir_all(parent)?;
        let mut bytes = Vec::new();
        for entry in entries {
            bytes.extend_from_slice(&entry.to_line());
        }
        write_locked(&path, &bytes, self.reference_fsync.method_if_enabled())
    }

    /// Export the live refs and, when requested, all non-empty reflogs from the
    /// active backend. Reftable exports are folded from one stable stack read;
    /// files-backend exports retain the backend's normal per-file consistency.
    ///
    /// The reftable implementation folds refs and logs while reading the stack
    /// once. This avoids the quadratic `list_reflog_names` followed by one full
    /// stack scan per name that a storage migration would otherwise perform.
    pub fn export_snapshot(&self, include_reflogs: bool) -> Result<RefSnapshot> {
        if self.uses_reftable()? {
            let mut refs = BTreeMap::<String, Ref>::new();
            let mut logs = BTreeMap::<(String, u64), Option<ReftableLogUpdate>>::new();
            for table in self.reftables()? {
                for record in table.refs {
                    let is_migratable_root = !record.name.starts_with("refs/")
                        && is_root_ref_syntax(&record.name)
                        && !matches!(record.name.as_str(), "FETCH_HEAD" | "MERGE_HEAD");
                    if !record.name.starts_with("refs/") && !is_migratable_root {
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
                if include_reflogs {
                    for record in table.logs {
                        let value = match record.value {
                            ReftableLogValue::Deletion => None,
                            ReftableLogValue::Update(update) => Some(update),
                        };
                        logs.insert((record.refname, record.update_index), value);
                    }
                }
            }

            let mut reflogs = BTreeMap::<String, Vec<ReflogEntry>>::new();
            if include_reflogs {
                let null = ObjectId::null(self.format);
                for ((name, _), update) in logs {
                    let Some(update) = update else {
                        continue;
                    };
                    if !reftable_log_update_is_empty_marker(&update, null) {
                        reflogs
                            .entry(name)
                            .or_default()
                            .push(reflog_entry_from_reftable(update));
                    }
                }
            }
            return Ok(RefSnapshot {
                refs: refs.into_values().collect(),
                reflogs: reflogs.into_iter().collect(),
            });
        }

        let refs = self.list_all_refs()?;
        if !include_reflogs {
            return Ok(RefSnapshot {
                refs,
                reflogs: Vec::new(),
            });
        }
        let mut names = self
            .list_reflog_names()?
            .into_iter()
            .collect::<BTreeSet<_>>();
        names.extend(refs.iter().map(|reference| reference.name.clone()));
        let mut reflogs = Vec::new();
        for name in names {
            let entries = self.read_reflog(&name)?;
            if !entries.is_empty() {
                reflogs.push((name, entries));
            }
        }
        Ok(RefSnapshot { refs, reflogs })
    }

    pub fn import_snapshot(
        &self,
        refs: &[Ref],
        reflogs: &[(String, Vec<ReflogEntry>)],
        pack_files_refs: bool,
    ) -> Result<()> {
        if self.uses_reftable()? {
            let ref_records = refs
                .iter()
                .map(|reference| ReftableRefRecord {
                    name: reference.name.clone(),
                    update_index: 0,
                    value: reftable_value_from_ref_target(&reference.target),
                })
                .collect::<Vec<_>>();
            let mut log_records = Vec::new();
            let mut update_index = 1u64;
            for (name, entries) in reflogs {
                for entry in entries {
                    log_records.push(ReftableLogRecord {
                        refname: name.clone(),
                        update_index,
                        value: ReftableLogValue::Update(self.reftable_update_from_reflog(entry)?),
                    });
                    update_index = update_index.checked_add(1).ok_or_else(|| {
                        GitError::InvalidFormat("reftable update index overflow".into())
                    })?;
                }
            }
            if !ref_records.is_empty() || !log_records.is_empty() {
                self.append_reftable_table_spanning(ref_records, log_records)?;
            }
            return Ok(());
        }

        if pack_files_refs {
            let mut packed = Vec::new();
            let mut tx = self.transaction();
            for reference in refs {
                if packable_loose_ref_name(&reference.name)
                    && matches!(&reference.target, RefTarget::Direct(_))
                {
                    packed.push(PackedRef {
                        reference: reference.clone(),
                        peeled: None,
                    });
                } else {
                    tx.update(RefUpdate {
                        name: reference.name.clone(),
                        expected: None,
                        new: reference.target.clone(),
                        reflog: None,
                    });
                }
            }
            // A migration directory is private to the caller, so direct refs
            // can be materialized in their final packed form. Writing them as
            // loose files only to rescan, pack, and prune them adds two file
            // operations and a lock per ref without improving atomicity.
            self.write_packed_refs(&packed)?;
            tx.commit()?;
        } else {
            let mut tx = self.transaction();
            for reference in refs {
                tx.update(RefUpdate {
                    name: reference.name.clone(),
                    expected: None,
                    new: reference.target.clone(),
                    reflog: None,
                });
            }
            tx.commit()?;
        }
        for (name, entries) in reflogs {
            self.write_reflog(name, entries)?;
        }
        Ok(())
    }

    pub fn expire_reflog_older_than(&self, name: &str, cutoff_seconds: i64) -> Result<usize> {
        validate_ref_name_for_read(name)?;
        if self.uses_reftable()? {
            let entries = self.read_reftable_logs(name)?;
            let original_len = entries.len();
            let mut retained = Vec::new();
            for entry in entries {
                if entry.timestamp_seconds()? >= cutoff_seconds {
                    retained.push(entry);
                }
            }
            let removed = original_len - retained.len();
            if removed > 0 {
                self.rewrite_reftable_logs(name, &retained, true)?;
            }
            return Ok(removed);
        }
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
        write_locked(&path, &bytes, self.reference_fsync.method_if_enabled())?;
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
        if self.uses_reftable()? {
            let entries = self.read_reftable_logs(name)?;
            let original_len = entries.len();
            let retained = expire_reflog(
                &entries,
                cutoff_unix,
                expire_unreachable_cutoff,
                is_reachable,
            )?;
            let removed = original_len - retained.len();
            if write && removed > 0 {
                self.rewrite_reftable_logs(name, &retained, true)?;
            }
            return Ok(removed);
        }
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
            write_locked(&path, &bytes, self.reference_fsync.method_if_enabled())?;
        }
        Ok(removed)
    }

    pub fn list_refs(&self) -> Result<Vec<Ref>> {
        self.list_refs_with_prefix("refs/")
    }

    pub fn list_refs_with_prefix(&self, prefix: &str) -> Result<Vec<Ref>> {
        if self.uses_reftable()? {
            let mut refs = BTreeMap::<String, Ref>::new();
            for reference in self
                .reftable_store_with_storage(self.shared_reftable_storage_dir())
                .list_reftable_refs_with_prefix(prefix)?
            {
                refs.insert(reference.name.clone(), reference);
            }
            if self.git_dir != self.common_dir {
                for reference in self.list_reftable_refs_with_prefix(prefix)? {
                    if reftable_current_worktree_ref(&reference.name) {
                        refs.insert(reference.name.clone(), reference);
                    }
                }
            }
            return Ok(refs.into_values().collect());
        }
        let mut refs = Vec::new();
        let packed_path = self.storage_dir.join("packed-refs");
        if packed_path.exists() {
            for packed in
                parse_packed_refs_with_prefix(self.format, &fs::read(packed_path)?, prefix)?
            {
                refs.push(packed.reference);
            }
        }
        let mut loose_refs = BTreeMap::new();
        self.collect_loose_refs_with_prefix(prefix, &mut loose_refs)?;
        self.collect_current_worktree_loose_refs_with_prefix(prefix, &mut loose_refs)?;
        if !loose_refs.is_empty() {
            refs.retain(|reference| !loose_refs.contains_key(&reference.name));
            refs.extend(loose_refs.into_values());
        }
        refs.retain(|reference| reference.name.starts_with(prefix));
        refs.retain(|reference| {
            if validate_ref_name(&reference.name).is_ok() {
                true
            } else {
                warn_broken_ref_name(&reference.name);
                false
            }
        });
        refs.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(refs)
    }

    pub fn list_ref_names_with_prefix(&self, prefix: &str) -> Result<Vec<String>> {
        if self.uses_reftable()? {
            return Ok(self
                .list_refs_with_prefix(prefix)?
                .into_iter()
                .map(|reference| reference.name)
                .collect());
        }
        let mut names = Vec::new();
        let packed_path = self.storage_dir.join("packed-refs");
        if packed_path.exists() {
            packed_ref_names_with_prefix(
                self.format,
                &fs::read(packed_path)?,
                prefix,
                false,
                &mut names,
            )?;
        }
        let mut loose_names = BTreeSet::new();
        self.collect_loose_ref_names_with_prefix(prefix, &mut loose_names)?;
        self.collect_current_worktree_loose_ref_names_with_prefix(prefix, &mut loose_names)?;
        if !loose_names.is_empty() {
            names.retain(|name| !loose_names.contains(name));
            names.extend(loose_names);
        }
        names.retain(|name| name.starts_with(prefix));
        names.retain(|name| {
            if validate_ref_name(name).is_ok() {
                true
            } else {
                warn_broken_ref_name(name);
                false
            }
        });
        names.sort();
        names.dedup();
        Ok(names)
    }

    pub fn list_short_ref_names_with_prefix(&self, prefix: &str) -> Result<Vec<String>> {
        if self.uses_reftable()? {
            let mut names = self
                .list_reftable_refs_with_prefix(prefix)?
                .into_iter()
                .filter_map(|reference| reference.name.strip_prefix(prefix).map(str::to_owned))
                .collect::<Vec<_>>();
            names.sort();
            return Ok(names);
        }
        let mut names = Vec::new();
        let packed_path = self.storage_dir.join("packed-refs");
        if packed_path.exists() {
            packed_ref_names_with_prefix(
                self.format,
                &fs::read(packed_path)?,
                prefix,
                true,
                &mut names,
            )?;
        }
        let mut loose_full_names = BTreeSet::new();
        self.collect_loose_ref_names_with_prefix(prefix, &mut loose_full_names)?;
        self.collect_current_worktree_loose_ref_names_with_prefix(prefix, &mut loose_full_names)?;
        let loose_names = loose_full_names
            .into_iter()
            .filter_map(|name| {
                if validate_ref_name(&name).is_ok() {
                    name.strip_prefix(prefix).map(str::to_owned)
                } else {
                    warn_broken_ref_name(&name);
                    None
                }
            })
            .collect::<BTreeSet<_>>();
        if !loose_names.is_empty() {
            names.retain(|name| !loose_names.contains(name));
            names.extend(loose_names);
        }
        names.sort();
        names.dedup();
        Ok(names)
    }

    pub fn list_all_refs(&self) -> Result<Vec<Ref>> {
        let mut refs = self.list_refs()?;
        let mut seen = refs
            .iter()
            .map(|reference| reference.name.clone())
            .collect::<BTreeSet<_>>();
        for reference in self.list_root_refs()? {
            if seen.insert(reference.name.clone()) {
                refs.push(reference);
            }
        }
        refs.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(refs)
    }

    pub fn list_reflog_names(&self) -> Result<Vec<String>> {
        let mut names = BTreeSet::new();
        if self.uses_reftable()? {
            let null = ObjectId::null(self.format);
            let mut logs_by_index = BTreeMap::<(String, u64), bool>::new();
            for table in self.reftables()? {
                for record in table.logs {
                    let has_visible_entry = match record.value {
                        ReftableLogValue::Deletion => false,
                        ReftableLogValue::Update(update) => {
                            !reftable_log_update_is_empty_marker(&update, null)
                        }
                    };
                    logs_by_index.insert((record.refname, record.update_index), has_visible_entry);
                }
            }
            for ((name, _), has_visible_entry) in logs_by_index {
                if has_visible_entry {
                    names.insert(name);
                }
            }
            return Ok(names.into_iter().collect());
        }
        self.collect_reflog_names(&self.storage_dir.join("logs"), "logs", &mut names)?;
        let worktree_logs = self.git_dir.join("logs");
        if worktree_logs != self.storage_dir.join("logs") {
            self.collect_reflog_names(&worktree_logs, "logs", &mut names)?;
        }
        Ok(names.into_iter().collect())
    }

    pub fn has_refs_with_prefix(&self, prefix: &str) -> Result<bool> {
        if self.uses_reftable()? {
            return Ok(self
                .list_reftable_refs()?
                .iter()
                .any(|reference| reference.name.starts_with(prefix)));
        }
        let packed_path = self.storage_dir.join("packed-refs");
        if packed_path.exists()
            && packed_refs_have_prefix(self.format, &fs::read(&packed_path)?, prefix)?
        {
            return Ok(true);
        }
        self.loose_refs_have_prefix(prefix)
    }

    pub fn write_packed_refs(&self, refs: &[PackedRef]) -> Result<()> {
        self.write_packed_refs_with_timeout(refs, 0)
    }

    pub fn write_packed_refs_with_timeout(
        &self,
        refs: &[PackedRef],
        timeout_millis: u64,
    ) -> Result<()> {
        let path = self.packed_refs_write_path()?;
        write_locked_with_timeout(
            &path,
            &write_packed_refs(refs)?,
            timeout_millis,
            self.reference_fsync.method_if_enabled(),
        )
    }

    pub fn pack_refs(&self, prune_loose: bool) -> Result<Vec<PackedRef>> {
        self.pack_refs_with_peeler(prune_loose, |_, _| Ok(None))
    }

    pub fn pack_refs_with_peeler<F>(&self, prune_loose: bool, mut peel: F) -> Result<Vec<PackedRef>>
    where
        F: FnMut(&str, &ObjectId) -> Result<Option<ObjectId>>,
    {
        self.pack_refs_selected_with_timeout(
            prune_loose,
            false,
            0,
            |_| true,
            |name, oid| peel(name, oid).map(|peeled| PackRefDecision::Pack { peeled }),
        )
    }

    pub fn pack_refs_selected_with_timeout<F, S>(
        &self,
        prune_loose: bool,
        auto: bool,
        timeout_millis: u64,
        mut should_pack: S,
        mut decide: F,
    ) -> Result<Vec<PackedRef>>
    where
        F: FnMut(&str, &ObjectId) -> Result<PackRefDecision>,
        S: FnMut(&str) -> bool,
    {
        if self.uses_reftable()? {
            self.compact_reftable_stack()?;
            return Ok(Vec::new());
        }
        let mut packed_refs = BTreeMap::new();
        let packed_path = self.storage_dir.join("packed-refs");
        if packed_path.exists() {
            for packed in parse_packed_refs(self.format, &fs::read(&packed_path)?)? {
                packed_refs.insert(packed.reference.name.clone(), packed);
            }
        }

        let mut loose_refs = BTreeMap::new();
        let refs_dir = self.storage_dir.join("refs");
        if refs_dir.exists() {
            self.collect_loose_refs(&refs_dir, "refs", &mut loose_refs)?;
        }
        let loose_refs = loose_refs
            .into_values()
            .filter(|reference| packable_loose_ref_name(&reference.name))
            .filter(|reference| should_pack(&reference.name))
            .collect::<Vec<_>>();
        if auto && !pack_refs_auto_required_for(&packed_path, loose_refs.len())? {
            return Ok(packed_refs.into_values().collect());
        }
        let mut packed_loose_refs = Vec::new();
        for reference in loose_refs {
            let RefTarget::Direct(oid) = reference.target else {
                continue;
            };
            let peeled = match decide(&reference.name, &oid)? {
                PackRefDecision::Pack { peeled } => peeled,
                PackRefDecision::Skip => continue,
            };
            packed_loose_refs.push((reference.name.clone(), oid));
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
        self.write_packed_refs_with_timeout(&refs, timeout_millis)?;
        if prune_loose {
            for (name, oid) in packed_loose_refs {
                self.prune_loose_ref_after_pack(&name, &oid)?;
            }
        }
        Ok(refs)
    }

    pub fn pack_refs_auto_required<S>(&self, mut should_pack: S) -> Result<bool>
    where
        S: FnMut(&str) -> bool,
    {
        if self.uses_reftable()? {
            return Ok(true);
        }
        let mut loose_refs = BTreeMap::new();
        let refs_dir = self.storage_dir.join("refs");
        if refs_dir.exists() {
            self.collect_loose_refs(&refs_dir, "refs", &mut loose_refs)?;
        }
        let count = loose_refs
            .values()
            .filter(|reference| packable_loose_ref_name(&reference.name))
            .filter(|reference| matches!(reference.target, RefTarget::Direct(_)))
            .filter(|reference| should_pack(&reference.name))
            .count();
        pack_refs_auto_required_for(&self.storage_dir.join("packed-refs"), count)
    }

    fn packed_refs_write_path(&self) -> Result<PathBuf> {
        let path = self.storage_dir.join("packed-refs");
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let target = fs::read_link(&path)?;
                if target.is_absolute() {
                    Ok(target)
                } else {
                    let parent = path.parent().ok_or_else(|| {
                        GitError::InvalidPath("packed-refs path has no parent".into())
                    })?;
                    Ok(parent.join(target))
                }
            }
            Ok(_) => Ok(path),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(path),
            Err(err) => Err(err.into()),
        }
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
            changes: Vec::new(),
            hook: None,
        }
    }

    pub fn uses_alternate_ref_storage(&self) -> bool {
        self.storage_dir != self.common_dir
    }

    pub fn current_worktree_ref_storage_dir(&self) -> PathBuf {
        self.current_worktree_storage_dir()
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
        let name = branch_ref_name_for_read(branch)?;
        if matches!(self.read_ref("HEAD")?, Some(RefTarget::Symbolic(head)) if head == name) {
            return Err(GitError::Transaction(format!(
                "cannot delete branch {branch} checked out at HEAD"
            )));
        }
        let oid = self.delete_direct_ref(&name, "branch", branch)?;
        self.remove_reflog_file(&name);
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

    /// Find an existing ref (other than `exclude`) that would have a
    /// directory/file conflict with creating `new_name`: either an existing ref
    /// is a path-prefix of `new_name` (it occupies a directory component
    /// `new_name` needs), or `new_name` is a path-prefix of an existing ref
    /// (`new_name` would occupy a directory another ref needs). Returns the
    /// conflicting ref name.
    fn conflicting_ref_for_path(&self, new_name: &str, exclude: &str) -> Result<Option<String>> {
        for reference in self.list_refs()? {
            let name = &reference.name;
            if name == new_name || name == exclude {
                continue;
            }
            // `name` sits above `new_name`: name = refs/heads/r, new = refs/heads/r/q
            if new_name.starts_with(&format!("{name}/")) {
                return Ok(Some(name.clone()));
            }
            // `name` sits below `new_name`: new = refs/heads/r, name = refs/heads/r/q
            if name.starts_with(&format!("{new_name}/")) {
                return Ok(Some(name.clone()));
            }
        }
        Ok(None)
    }

    fn copy_or_move_branch(
        &self,
        old_branch: &str,
        new_branch: &str,
        force: bool,
        copy: bool,
        committer: Vec<u8>,
    ) -> Result<()> {
        let old_name = branch_ref_name_for_source(old_branch)?;
        let new_name = branch_ref_name(new_branch)?;
        if old_name == new_name {
            return Ok(());
        }
        let Some(target) = self.read_ref(&old_name)? else {
            return Err(GitError::reference_not_found(format!(
                "branch {old_branch}"
            )));
        };
        let RefTarget::Direct(oid) = target else {
            return Err(GitError::InvalidFormat(format!(
                "branch {old_branch} is symbolic"
            )));
        };
        // Detect a directory/file conflict against some *other* ref before
        // mutating anything (git's rename_ref fails up front, leaving the old
        // branch intact): e.g. renaming `q` -> `r/q` while `r` exists, or `q` ->
        // `r` while `r/x` exists. For renames, the old ref itself is excluded
        // because a self-nesting rename (`m` -> `m/m`) is handled by removing it
        // first. Copies leave the source ref in place, so the source can
        // conflict with the destination.
        let conflict_exclude = if copy { "" } else { &old_name };
        if let Some(conflict) = self.conflicting_ref_for_path(&new_name, conflict_exclude)? {
            return Err(GitError::Transaction(format!(
                "'{conflict}' exists; cannot create '{new_name}'"
            )));
        }
        // git's validate_branchname uses refs_ref_exists (RESOLVE_REF_READING):
        // a *dangling* symref destination does not "exist", so a rename onto it
        // proceeds without --force and overwrites the symref file (t3200 #16).
        let dest_entry = self.read_ref(&new_name)?;
        let dest_resolves = resolve_ref_peeled(self, &new_name)?.is_some();
        if dest_resolves && !force {
            return Err(GitError::Transaction(format!(
                "branch {new_branch} already exists"
            )));
        }
        // Remove any existing destination ref (direct or symbolic) before
        // writing. A dangling symref must be removed as a symref; a real branch
        // as a direct ref.
        match dest_entry {
            Some(RefTarget::Symbolic(_)) => {
                self.delete_symbolic_ref(&new_name)?;
                self.remove_reflog_file(&new_name);
            }
            Some(RefTarget::Direct(_)) => {
                let _ = self.delete_direct_ref(&new_name, "branch", new_branch)?;
                self.remove_reflog_file(&new_name);
            }
            None => {}
        }

        // Capture the old reflog before removing anything; it is carried over
        // to the new ref.
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

        // A directory/file conflict can occur when the new ref's path nests
        // under the old ref (`m` -> `m/m`) or vice-versa; remove the old loose
        // ref AND its reflog first so neither file blocks creating the new
        // directory under refs/ or logs/refs/ (t3200 #17, #18).
        if !copy {
            let _ = self.delete_direct_ref(&old_name, "branch", old_branch)?;
            self.remove_reflog_file(&old_name);
        }

        self.write_loose_ref(&Ref {
            name: new_name.clone(),
            target: RefTarget::Direct(oid),
        })?;
        self.write_reflog(&new_name, &reflog)?;

        if !copy
            && matches!(self.read_ref("HEAD")?, Some(RefTarget::Symbolic(head)) if head == old_name)
        {
            self.write_loose_ref(&Ref {
                name: "HEAD".into(),
                target: RefTarget::Symbolic(new_name),
            })?;
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
        let name = TagRefNameBuf::from_tag_name_unrestricted(tag)?.into_string();
        let oid = self.delete_direct_ref(&name, "tag", tag)?;
        self.remove_reflog_file(&name);
        Ok(TagDelete { name, oid })
    }

    pub fn delete_ref(&self, name: &str) -> Result<RefDelete> {
        validate_ref_name_for_read(name)?;
        let oid = self.delete_direct_ref(name, "ref", name)?;
        self.remove_reflog_file(name);
        Ok(RefDelete {
            name: name.into(),
            oid,
        })
    }

    pub fn delete_ref_checked(
        &self,
        delete: DeleteRef,
    ) -> std::result::Result<RefDelete, RefDeleteError> {
        validate_ref_name_for_read(&delete.name).map_err(|_| RefDeleteError::InvalidName)?;
        if self.uses_reftable().map_err(ref_delete_error_from_git)? {
            return self.delete_reftable_ref_checked(delete);
        }
        self.delete_files_ref_checked(delete)
    }

    pub fn delete_symbolic_ref(&self, name: &str) -> Result<bool> {
        validate_ref_name_for_read(name)?;
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
            self.remove_reflog_file(name);
            return Ok(true);
        }
        let Some(reference) = self.read_loose_ref(name)? else {
            return Ok(false);
        };
        if !matches!(reference.target, RefTarget::Symbolic(_)) {
            return Ok(false);
        }
        self.delete_loose_ref(name)?;
        self.remove_reflog_file(name);
        Ok(true)
    }

    fn delete_direct_ref(&self, name: &str, kind: &str, short_name: &str) -> Result<ObjectId> {
        if self.uses_reftable()? {
            let Some(target) = self.read_ref(name)? else {
                return Err(GitError::reference_not_found(format!(
                    "{kind} {short_name}"
                )));
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
            // git drops the reflog when the ref goes away; tombstone the log
            // records so `git reflog` / `git stash list` stop seeing them.
            self.remove_reflog_file(name);
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
        // A loose ref can shadow a packed entry of the same name (e.g. an update
        // after `pack-refs --all` writes a loose file over the packed copy).
        // git removes the ref from packed-refs too, so unlinking only the loose
        // file would leave the stale packed entry resurfacing. Drop any packed
        // copy as well; its absence is not an error.
        match self.delete_packed_ref(name, kind, short_name) {
            Ok(_) | Err(GitError::NotFound(_)) => {}
            Err(err) => return Err(err),
        }
        Ok(oid)
    }

    fn delete_packed_ref(&self, name: &str, kind: &str, short_name: &str) -> Result<ObjectId> {
        let path = self.storage_dir.join("packed-refs");
        if !path.exists() {
            return Err(GitError::reference_not_found(format!(
                "{kind} {short_name}"
            )));
        }
        let mut refs = parse_packed_refs(self.format, &fs::read(&path)?)?;
        let Some(index) = refs
            .iter()
            .position(|reference| reference.reference.name == name)
        else {
            return Err(GitError::reference_not_found(format!(
                "{kind} {short_name}"
            )));
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

    fn delete_reftable_ref_checked(
        &self,
        delete: DeleteRef,
    ) -> std::result::Result<RefDelete, RefDeleteError> {
        let target = self
            .read_ref(&delete.name)
            .map_err(ref_delete_error_from_git)?;
        let oid = checked_delete_oid(delete.expected_old, target)?;
        self.append_reftable_records(vec![ReftableRefRecord {
            name: delete.name.clone(),
            update_index: 0,
            value: ReftableRefValue::Deletion,
        }])
        .map_err(ref_delete_error_from_git)?;
        // Git unlinks logs/refs/<name> on delete (pruning now-empty dirs); it
        // does not keep a deletion entry. Mirror delete_ref / delete_branch.
        self.remove_reflog_file(&delete.name);
        Ok(RefDelete {
            name: delete.name,
            oid,
        })
    }

    fn delete_files_ref_checked(
        &self,
        delete: DeleteRef,
    ) -> std::result::Result<RefDelete, RefDeleteError> {
        let name = delete.name;
        let path = self.ref_path(&name);
        let parent = path.parent().ok_or(RefDeleteError::InvalidName)?;
        fs::create_dir_all(parent).map_err(RefDeleteError::from)?;

        let loose_lock_path = lock_path_for(&path).map_err(|_| RefDeleteError::InvalidName)?;
        let _prune_guard = RefDirPruneGuard {
            store: self,
            name: name.clone(),
        };
        let loose_lock = DeleteLock::acquire(loose_lock_path)?;

        let packed_path = self.storage_dir.join("packed-refs");
        let packed_lock_path =
            lock_path_for(&packed_path).map_err(|_| RefDeleteError::InvalidName)?;
        let mut packed_lock = DeleteLock::acquire(packed_lock_path)?;

        let loose_ref = self
            .read_loose_ref(&name)
            .map_err(ref_delete_error_from_git)?;
        let packed_original = match fs::read(&packed_path) {
            Ok(bytes) => Some(bytes),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => return Err(RefDeleteError::Io(err)),
        };
        let mut packed_refs = match &packed_original {
            Some(bytes) => {
                parse_packed_refs(self.format, bytes).map_err(ref_delete_error_from_git)?
            }
            None => Vec::new(),
        };
        let packed_index = packed_refs
            .iter()
            .position(|reference| reference.reference.name == name);

        let current = if let Some(reference) = loose_ref.as_ref() {
            Some(reference.target.clone())
        } else {
            packed_index.map(|index| packed_refs[index].reference.target.clone())
        };
        let oid = checked_delete_oid(delete.expected_old, current)?;

        let packed_changed = if let Some(index) = packed_index {
            packed_refs.remove(index);
            true
        } else {
            false
        };

        if packed_changed {
            let packed_bytes =
                write_packed_refs(&packed_refs).map_err(ref_delete_error_from_git)?;
            packed_lock.write_all(&packed_bytes, self.reference_fsync.method_if_enabled())?;
            let lock_path = packed_lock.close();
            if let Err(err) = fs::rename(&lock_path, &packed_path) {
                let _ = fs::remove_file(&lock_path);
                return Err(RefDeleteError::Io(err));
            }
        } else {
            packed_lock.remove();
        }

        if loose_ref.is_some()
            && let Err(err) = fs::remove_file(&path)
        {
            if packed_changed && let Some(bytes) = packed_original.as_ref() {
                let _ = restore_file_atomically(&packed_path, bytes);
            }
            return Err(RefDeleteError::Io(err));
        }
        loose_lock.remove();

        // Git unlinks logs/refs/<name> on delete (pruning now-empty dirs); it
        // does not keep a deletion entry. Mirror delete_ref / delete_branch.
        self.remove_reflog_file(&name);
        Ok(RefDelete { name, oid })
    }

    fn read_loose_ref(&self, name: &str) -> Result<Option<Ref>> {
        let path = self.ref_path(name);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(err)
                if err.kind() == std::io::ErrorKind::NotFound || err.raw_os_error() == Some(20) =>
            {
                return Ok(None);
            }
            Err(err) => return Err(err.into()),
        };
        if metadata.is_dir() {
            return Ok(None);
        }
        if metadata.file_type().is_symlink()
            && let Ok(target) = fs::read_link(&path)
        {
            let target = target.to_string_lossy();
            if target.starts_with("refs/") {
                return Ok(Some(Ref {
                    name: name.to_string(),
                    target: RefTarget::Symbolic(target.into_owned()),
                }));
            }
        }
        let bytes = fs::read(path)?;
        if loose_ref_bytes_are_reftable_sentinel(name, &bytes) {
            return Ok(None);
        }
        Ok(Some(parse_loose_ref(self.format, name, &bytes)?))
    }

    fn read_packed_ref(&self, name: &str) -> Result<Option<PackedRef>> {
        let path = self.storage_dir.join("packed-refs");
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
        self.list_reftable_refs_with_prefix("refs/")
    }

    fn list_reftable_refs_with_prefix(&self, prefix: &str) -> Result<Vec<Ref>> {
        let mut refs = BTreeMap::<String, Ref>::new();
        for table in self.reftables()? {
            for record in table.refs {
                if !record.name.starts_with("refs/") || !record.name.starts_with(prefix) {
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

    /// List the live one-level references held by the active ref backend.
    ///
    /// `FETCH_HEAD` and `MERGE_HEAD` are working-state files rather than
    /// migratable refs, so they are deliberately excluded for both backends.
    /// Callers which expose a narrower root-ref surface (for example
    /// `for-each-ref --include-root-refs`) should apply their own semantic
    /// predicate to this backend-independent inventory.
    pub fn list_root_refs(&self) -> Result<Vec<Ref>> {
        if self.uses_reftable()? {
            let mut refs = BTreeMap::<String, Ref>::new();
            for table in self.reftables()? {
                for record in table.refs {
                    if record.name.starts_with("refs/")
                        || !is_root_ref_syntax(&record.name)
                        || matches!(record.name.as_str(), "FETCH_HEAD" | "MERGE_HEAD")
                    {
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
            return Ok(refs.into_values().collect());
        }

        let mut refs = Vec::new();
        for entry in fs::read_dir(&self.git_dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if !is_root_ref_syntax(&name) || matches!(name.as_str(), "FETCH_HEAD" | "MERGE_HEAD") {
                continue;
            }
            if let Ok(reference) = parse_loose_ref(self.format, name, &fs::read(entry.path())?) {
                refs.push(reference);
            }
        }
        refs.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(refs)
    }

    fn reftables(&self) -> Result<Vec<Reftable>> {
        let reftable_dir = self.storage_dir.join("reftable");
        let tables_list = reftable_dir.join("tables.list");
        for _ in 0..10 {
            let text = match fs::read_to_string(&tables_list) {
                Ok(text) => text,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    if !tables_list.exists() {
                        return Ok(Vec::new());
                    }
                    thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Err(err) => return Err(err.into()),
            };
            let mut tables = Vec::new();
            let mut reload = false;
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
                let bytes = match fs::read(reftable_dir.join(line)) {
                    Ok(bytes) => bytes,
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                        reload = true;
                        break;
                    }
                    Err(err) => return Err(err.into()),
                };
                let table = Reftable::parse(&bytes)?;
                if table.header.object_format != self.format {
                    return Err(GitError::InvalidFormat(format!(
                        "reftable {line} has {} object ids in {} repository",
                        table.header.object_format.name(),
                        self.format.name()
                    )));
                }
                tables.push(table);
            }
            if reload {
                thread::sleep(Duration::from_millis(10));
                continue;
            }
            return Ok(tables);
        }
        Err(GitError::Io(format!(
            "cannot read stable reftable stack {}",
            tables_list.display()
        )))
    }

    fn reftable_store_with_storage(&self, storage_dir: PathBuf) -> FileRefStore {
        FileRefStore {
            git_dir: self.git_dir.clone(),
            common_dir: self.common_dir.clone(),
            storage_dir,
            format: self.format,
            ref_storage: self.ref_storage.clone(),
            reftable_lock_timeout_millis: self.reftable_lock_timeout_millis,
            reftable_write_options: self.reftable_write_options,
            combine_reftable_logs: self.combine_reftable_logs,
            reference_fsync: self.reference_fsync,
        }
    }

    /// Directory holding the *shared* (non-per-worktree) reftable stack.
    ///
    /// Normally this is the common dir, but an `extensions.refStorage` /
    /// `GIT_REFERENCE_BACKEND` URI with a `://path` payload relocates the
    /// stack to that path (e.g. `reftable:///abs/path`). `storage_dir` cannot
    /// stand in for this: for a *linked worktree* with no alternate path it is
    /// the per-worktree gitdir, not the shared stack. So resolve the shared
    /// location explicitly from the configured backend, mirroring the path
    /// resolution in `FileRefStore::new`. (Without this, shared refs under an
    /// alternate-path backend are looked up in the empty default `reftable/`.)
    fn shared_reftable_storage_dir(&self) -> PathBuf {
        match self.ref_storage.backend.as_ref() {
            Some((_, Some(path))) => path.clone(),
            _ => self.common_dir.clone(),
        }
    }

    fn current_worktree_storage_dir(&self) -> PathBuf {
        // In the normal linked-worktree layout `storage_dir` is already the
        // worktree's administrative gitdir. Only an explicitly relocated
        // reftable backend needs the `worktrees/<id>` suffix below.
        if self.storage_dir == self.git_dir {
            return self.git_dir.clone();
        }
        if self.storage_dir != self.common_dir {
            if self.git_dir != self.common_dir
                && let Some(worktree) = self.git_dir.file_name()
            {
                return self.storage_dir.join("worktrees").join(worktree);
            }
            return self.storage_dir.clone();
        }
        self.git_dir.clone()
    }

    fn reftable_store_for_ref(&self, name: &str) -> Result<(FileRefStore, String)> {
        if let Some((worktree, rewritten)) = reftable_other_worktree_ref(name) {
            let storage_dir = self
                .shared_reftable_storage_dir()
                .join("worktrees")
                .join(worktree);
            return Ok((
                self.reftable_store_with_storage(storage_dir),
                rewritten.to_string(),
            ));
        }
        if reftable_current_worktree_ref(name) {
            return Ok((
                self.reftable_store_with_storage(self.current_worktree_storage_dir()),
                name.to_string(),
            ));
        }
        Ok((
            self.reftable_store_with_storage(self.shared_reftable_storage_dir()),
            name.to_string(),
        ))
    }

    pub fn uses_reftable(&self) -> Result<bool> {
        if let Some(value) = self.ref_storage.invalid_value.as_deref() {
            return parse_ref_storage_backend_value(value)
                .map(|(kind, _)| kind == RefBackendKind::Reftable);
        }
        Ok(matches!(
            self.ref_storage.backend,
            Some((RefBackendKind::Reftable, _))
        ))
    }

    pub fn reftable_table_count(&self) -> Result<usize> {
        Ok(self.reftable_table_names()?.len())
    }

    fn append_reftable_records(&self, records: Vec<ReftableRefRecord>) -> Result<()> {
        if records.is_empty() {
            return Ok(());
        }
        self.append_reftable_table(records, Vec::new())?;
        Ok(())
    }

    fn next_reftable_update_index(&self, table_names: &[String]) -> Result<u64> {
        let reftable_dir = self.storage_dir.join("reftable");
        let mut max_update_index = 0;
        for name in table_names {
            let table = Reftable::parse(&fs::read(reftable_dir.join(name))?)?;
            max_update_index = max_update_index.max(table.header.max_update_index);
        }
        max_update_index
            .checked_add(1)
            .ok_or_else(|| GitError::InvalidFormat("reftable update index overflow".into()))
    }

    /// Read the table list (file names) backing the reftable stack, oldest first.
    fn reftable_table_names(&self) -> Result<Vec<String>> {
        self.reftable_table_names_from(&self.storage_dir.join("reftable").join("tables.list"))
    }

    fn reftable_table_names_from(&self, tables_list: &Path) -> Result<Vec<String>> {
        if !tables_list.exists() {
            return Ok(Vec::new());
        }
        Ok(fs::read_to_string(tables_list)?
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect())
    }

    fn reftable_lock_timeout_millis(&self) -> Result<u64> {
        if let Some(timeout_millis) = self.reftable_lock_timeout_millis {
            return Ok(timeout_millis);
        }
        let config_path = self.common_dir.join("config");
        let Ok(config) = GitConfig::read(config_path) else {
            return Ok(0);
        };
        Ok(config
            .get("reftable", None, "lockTimeout")
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0))
    }

    fn acquire_reftable_list_lock(&self, list_path: PathBuf) -> Result<ReftableListLock> {
        let lock_path = lock_path_for(&list_path)?;
        let timeout_millis = self.reftable_lock_timeout_millis()?;
        ReftableListLock::acquire(list_path, lock_path, timeout_millis)
    }

    /// Append a single combined ref+log table to the stack, allocating the next
    /// update index. Either slice may be empty (a log-only table for an
    /// `append_reflog` / `delete-reflog`, or a ref-only table for a plain ref
    /// write). Returns the allocated update index.
    fn append_reftable_table(
        &self,
        refs: Vec<ReftableRefRecord>,
        logs: Vec<ReftableLogRecord>,
    ) -> Result<u64> {
        self.append_reftable_table_with_compaction(refs, logs, true)
    }

    fn append_reftable_table_with_compaction(
        &self,
        mut refs: Vec<ReftableRefRecord>,
        mut logs: Vec<ReftableLogRecord>,
        auto_compact: bool,
    ) -> Result<u64> {
        let reftable_dir = self.storage_dir.join("reftable");
        fs::create_dir_all(&reftable_dir)?;
        let list_path = reftable_dir.join("tables.list");
        let list_lock = self.acquire_reftable_list_lock(list_path.clone())?;
        let mut table_names = self.reftable_table_names_from(&list_path)?;
        let update_index = self.next_reftable_update_index(&table_names)?;
        for record in &mut refs {
            record.update_index = update_index;
        }
        for record in &mut logs {
            record.update_index = update_index;
        }
        let table_name = reftable_table_name(update_index, update_index);
        let bytes = Reftable::write_with_options(
            self.format,
            update_index,
            update_index,
            &refs,
            &logs,
            self.reftable_write_options,
        )?;
        let table_path = reftable_dir.join(&table_name);
        write_locked(
            &table_path,
            &bytes,
            self.reference_fsync.method_if_enabled(),
        )?;
        self.apply_reftable_shared_file_mode(&table_path)?;
        table_names.push(table_name);
        let mut list = Vec::new();
        for name in &table_names {
            list.extend_from_slice(name.as_bytes());
            list.push(b'\n');
        }
        list_lock.commit(&list, self.reference_fsync.method_if_enabled())?;
        self.apply_reftable_shared_file_mode(&list_path)?;
        if auto_compact && logs.is_empty() && table_names.len() > 6 {
            self.auto_compact_reftable_stack()?;
        }
        Ok(update_index)
    }

    pub fn compact_reftable_stack(&self) -> Result<()> {
        let old_names = self.reftable_table_names()?;
        self.compact_reftable_stack_range(0, old_names.len(), true)
    }

    fn compact_reftable_stack_range(
        &self,
        start: usize,
        end: usize,
        fail_on_locked_table: bool,
    ) -> Result<()> {
        let reftable_dir = self.storage_dir.join("reftable");
        let list_path = reftable_dir.join("tables.list");
        let list_lock = self.acquire_reftable_list_lock(list_path.clone())?;
        let old_names = self.reftable_table_names_from(&list_path)?;
        if start >= end || end > old_names.len() {
            return Ok(());
        }
        let compact_names = old_names[start..end].to_vec();
        if compact_names.is_empty() {
            return Ok(());
        }
        if fail_on_locked_table {
            for name in &compact_names {
                if reftable_dir.join(format!("{name}.lock")).exists() {
                    return Err(GitError::Io(format!(
                        "cannot lock references: {}: File exists",
                        reftable_dir.join(format!("{name}.lock")).display()
                    )));
                }
            }
        }

        let mut refs: BTreeMap<String, ReftableRefRecord> = BTreeMap::new();
        let mut logs: BTreeMap<(String, u64), ReftableLogRecord> = BTreeMap::new();
        let mut min_index = u64::MAX;
        let mut max_index = 0u64;
        let drop_tombstones = start == 0;
        for name in &compact_names {
            let table = Reftable::parse(&fs::read(reftable_dir.join(name))?)?;
            for record in table.refs {
                match record.value {
                    ReftableRefValue::Deletion if drop_tombstones => {
                        refs.remove(&record.name);
                    }
                    _ => {
                        min_index = min_index.min(record.update_index);
                        max_index = max_index.max(record.update_index);
                        refs.insert(record.name.clone(), record);
                    }
                }
            }
            for record in table.logs {
                let key = (record.refname.clone(), record.update_index);
                match record.value {
                    ReftableLogValue::Deletion if drop_tombstones => {
                        logs.remove(&key);
                    }
                    _ => {
                        min_index = min_index.min(record.update_index);
                        max_index = max_index.max(record.update_index);
                        logs.insert(key, record);
                    }
                }
            }
        }

        if refs.is_empty() && logs.is_empty() {
            min_index = compact_names
                .iter()
                .filter_map(|name| Reftable::parse(&fs::read(reftable_dir.join(name)).ok()?).ok())
                .map(|table| table.header.min_update_index)
                .min()
                .unwrap_or(1);
            max_index = min_index;
        }

        let table_name = reftable_table_name(min_index, max_index);
        let refs = refs.into_values().collect::<Vec<_>>();
        let logs = logs.into_values().collect::<Vec<_>>();
        let bytes = Reftable::write_with_options(
            self.format,
            min_index,
            max_index,
            &refs,
            &logs,
            self.reftable_write_options,
        )?;
        let table_path = reftable_dir.join(&table_name);
        write_locked(
            &table_path,
            &bytes,
            self.reference_fsync.method_if_enabled(),
        )?;
        self.apply_reftable_shared_file_mode(&table_path)?;
        let mut list = Vec::new();
        for name in &old_names[..start] {
            list.extend_from_slice(name.as_bytes());
            list.push(b'\n');
        }
        list.extend_from_slice(table_name.as_bytes());
        list.push(b'\n');
        for name in &old_names[end..] {
            list.extend_from_slice(name.as_bytes());
            list.push(b'\n');
        }
        list_lock.commit(&list, self.reference_fsync.method_if_enabled())?;
        self.apply_reftable_shared_file_mode(&list_path)?;
        for name in compact_names {
            if name != table_name {
                let _ = fs::remove_file(reftable_dir.join(name));
            }
        }
        Ok(())
    }

    fn apply_reftable_shared_file_mode(&self, path: &Path) -> Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let config_path = self.common_dir.join("config");
            let Ok(config) = GitConfig::read(config_path) else {
                return Ok(());
            };
            let Some(value) = config.get("core", None, "sharedRepository") else {
                return Ok(());
            };
            let mode_or = match value {
                "1" | "group" | "true" => 0o660,
                "2" | "all" | "world" | "everybody" => 0o664,
                _ => return Ok(()),
            };
            let metadata = fs::metadata(path)?;
            let old_mode = metadata.permissions().mode();
            let mut permissions = metadata.permissions();
            permissions.set_mode((old_mode | mode_or) & 0o7777);
            fs::set_permissions(path, permissions)?;
        }
        #[cfg(not(unix))]
        {
            let _ = path;
        }
        Ok(())
    }

    fn auto_compact_reftable_stack(&self) -> Result<()> {
        if reftable_autocompaction_disabled() {
            return Ok(());
        }
        let names = self.reftable_table_names()?;
        if names.len() <= 1 {
            return Ok(());
        }
        let reftable_dir = self.storage_dir.join("reftable");
        let header_overhead = match self.format {
            ObjectFormat::Sha1 => 23,
            ObjectFormat::Sha256 => 27,
        };
        let sizes = names
            .iter()
            .map(|name| {
                Ok(fs::metadata(reftable_dir.join(name))?
                    .len()
                    .saturating_sub(header_overhead))
            })
            .collect::<Result<Vec<_>>>()?;
        let factor = self.reftable_geometric_factor()?;
        let Some((start, end)) = reftable_compaction_segment(&sizes, factor) else {
            return Ok(());
        };

        // Git locks a candidate segment newest-to-oldest. If it encounters a
        // table already owned by another compactor, it still merges the newer
        // suffix when at least two of those tables are available. This is what
        // lets writers continue restoring the geometric sequence behind a
        // long-lived lock on an older table.
        let start = names[start..end]
            .iter()
            .rposition(|name| reftable_dir.join(format!("{name}.lock")).exists())
            .map_or(start, |locked| start + locked + 1);
        if end.saturating_sub(start) < 2 {
            return Ok(());
        }
        self.compact_reftable_stack_range(start, end, false)?;
        Ok(())
    }

    fn reftable_geometric_factor(&self) -> Result<u64> {
        let config_path = self.common_dir.join("config");
        let Ok(config) = GitConfig::read(config_path) else {
            return Ok(2);
        };
        Ok(config
            .get("reftable", None, "geometricFactor")
            .and_then(|value| value.parse::<u8>().ok())
            .filter(|factor| *factor != 0)
            .map(u64::from)
            .unwrap_or(2))
    }

    /// Merge the log records for `name` across the whole stack into the reflog
    /// entries `git reflog` expects, in *oldest-first* order (the loose-file
    /// order callers reverse for display). Later tables in the stack override
    /// earlier ones for the same `(refname, update_index)`, deletions mask
    /// entries, and the old==new==null existence marker is dropped (it records
    /// reflog existence, not a real entry).
    fn read_reftable_logs(&self, name: &str) -> Result<Vec<ReflogEntry>> {
        // Collect the newest value for each update_index, honoring deletions.
        // update_index ascending == chronological order.
        let mut by_index: BTreeMap<u64, Option<ReftableLogUpdate>> = BTreeMap::new();
        for table in self.reftables()? {
            for record in table.logs {
                if record.refname != name {
                    continue;
                }
                match record.value {
                    ReftableLogValue::Deletion => {
                        by_index.insert(record.update_index, None);
                    }
                    ReftableLogValue::Update(update) => {
                        by_index.insert(record.update_index, Some(update));
                    }
                }
            }
        }
        let null = ObjectId::null(self.format);
        let mut entries = Vec::new();
        for update in by_index.into_values().flatten() {
            // Drop the existence marker (old==new==null): it is not a real entry.
            if reftable_log_update_is_empty_marker(&update, null) {
                continue;
            }
            entries.push(reflog_entry_from_reftable(update));
        }
        Ok(entries)
    }

    fn reftable_log_exists(&self, name: &str) -> Result<bool> {
        let mut by_index: BTreeMap<u64, bool> = BTreeMap::new();
        for table in self.reftables()? {
            for record in table.logs {
                if record.refname != name {
                    continue;
                }
                match record.value {
                    ReftableLogValue::Deletion => {
                        by_index.insert(record.update_index, false);
                    }
                    ReftableLogValue::Update(_) => {
                        by_index.insert(record.update_index, true);
                    }
                }
            }
        }
        Ok(by_index.into_values().any(|exists| exists))
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
                let bytes = fs::read(path)?;
                if loose_ref_bytes_are_reftable_sentinel(&name, &bytes) {
                    continue;
                }
                // git marks a loose ref whose content is unparseable, or that
                // resolves to the null OID, as REF_ISBROKEN and skips it from
                // iteration with a warning instead of aborting the whole walk.
                match parse_loose_ref(self.format, name.clone(), &bytes) {
                    Ok(reference) if ref_target_is_broken(&reference.target) => {
                        warn_broken_ref(&name);
                    }
                    Ok(reference) => {
                        refs.insert(name, reference);
                    }
                    Err(_) => warn_broken_ref(&name),
                }
            }
        }
        Ok(())
    }

    fn collect_loose_refs_with_prefix(
        &self,
        prefix: &str,
        refs: &mut BTreeMap<String, Ref>,
    ) -> Result<()> {
        self.collect_loose_refs_with_prefix_from_base(&self.storage_dir, prefix, refs)
    }

    fn collect_current_worktree_loose_refs_with_prefix(
        &self,
        prefix: &str,
        refs: &mut BTreeMap<String, Ref>,
    ) -> Result<()> {
        let base = self.current_worktree_storage_dir();
        if base == self.storage_dir {
            return Ok(());
        }
        for namespace in CURRENT_WORKTREE_REF_PREFIXES {
            if ref_prefixes_overlap(prefix, namespace) {
                let scan_prefix = if prefix.starts_with(namespace) {
                    prefix
                } else {
                    namespace
                };
                self.collect_loose_refs_with_prefix_from_base(&base, scan_prefix, refs)?;
            }
        }
        Ok(())
    }

    fn collect_loose_refs_with_prefix_from_base(
        &self,
        base: &Path,
        prefix: &str,
        refs: &mut BTreeMap<String, Ref>,
    ) -> Result<()> {
        if !safe_ref_prefix_for_directory_scan(prefix) {
            return self.collect_all_loose_refs_from_base(base, refs);
        }

        let trimmed = prefix.trim_end_matches('/');
        if prefix.ends_with('/') {
            let candidate = base.join(trimmed);
            match fs::metadata(&candidate) {
                Ok(meta) if meta.is_dir() => {
                    self.collect_loose_refs(&candidate, trimmed, refs)?;
                    return Ok(());
                }
                Ok(_) => return Ok(()),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(err) => return Err(err.into()),
            }
        }

        let Some((parent_prefix, _)) = trimmed.rsplit_once('/') else {
            return self.collect_all_loose_refs_from_base(base, refs);
        };
        let parent = base.join(parent_prefix);
        match fs::metadata(&parent) {
            Ok(meta) if meta.is_dir() => self.collect_loose_refs(&parent, parent_prefix, refs),
            Ok(_) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err.into()),
        }
    }

    fn collect_loose_ref_names(
        &self,
        dir: &Path,
        prefix: &str,
        names: &mut BTreeSet<String>,
    ) -> Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let name = format!("{prefix}/{}", entry.file_name().to_string_lossy());
            if path.is_dir() {
                self.collect_loose_ref_names(&path, &name, names)?;
            } else if !name.ends_with(".lock") {
                names.insert(name);
            }
        }
        Ok(())
    }

    fn collect_loose_ref_names_with_prefix(
        &self,
        prefix: &str,
        names: &mut BTreeSet<String>,
    ) -> Result<()> {
        self.collect_loose_ref_names_with_prefix_from_base(&self.storage_dir, prefix, names)
    }

    fn collect_current_worktree_loose_ref_names_with_prefix(
        &self,
        prefix: &str,
        names: &mut BTreeSet<String>,
    ) -> Result<()> {
        let base = self.current_worktree_storage_dir();
        if base == self.storage_dir {
            return Ok(());
        }
        for namespace in CURRENT_WORKTREE_REF_PREFIXES {
            if ref_prefixes_overlap(prefix, namespace) {
                let scan_prefix = if prefix.starts_with(namespace) {
                    prefix
                } else {
                    namespace
                };
                self.collect_loose_ref_names_with_prefix_from_base(&base, scan_prefix, names)?;
            }
        }
        Ok(())
    }

    fn collect_loose_ref_names_with_prefix_from_base(
        &self,
        base: &Path,
        prefix: &str,
        names: &mut BTreeSet<String>,
    ) -> Result<()> {
        if !safe_ref_prefix_for_directory_scan(prefix) {
            return self.collect_all_loose_ref_names_from_base(base, names);
        }

        let trimmed = prefix.trim_end_matches('/');
        if prefix.ends_with('/') {
            let candidate = base.join(trimmed);
            match fs::metadata(&candidate) {
                Ok(meta) if meta.is_dir() => {
                    self.collect_loose_ref_names(&candidate, trimmed, names)?;
                    return Ok(());
                }
                Ok(_) => return Ok(()),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(err) => return Err(err.into()),
            }
        }

        let Some((parent_prefix, _)) = trimmed.rsplit_once('/') else {
            return self.collect_all_loose_ref_names_from_base(base, names);
        };
        let parent = base.join(parent_prefix);
        match fs::metadata(&parent) {
            Ok(meta) if meta.is_dir() => {
                self.collect_loose_ref_names(&parent, parent_prefix, names)
            }
            Ok(_) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err.into()),
        }
    }

    fn collect_all_loose_ref_names_from_base(
        &self,
        base: &Path,
        names: &mut BTreeSet<String>,
    ) -> Result<()> {
        let refs_dir = base.join("refs");
        if refs_dir.exists() {
            self.collect_loose_ref_names(&refs_dir, "refs", names)?;
        }
        Ok(())
    }

    fn collect_all_loose_refs_from_base(
        &self,
        base: &Path,
        refs: &mut BTreeMap<String, Ref>,
    ) -> Result<()> {
        let refs_dir = base.join("refs");
        if refs_dir.exists() {
            self.collect_loose_refs(&refs_dir, "refs", refs)?;
        }
        Ok(())
    }

    fn collect_reflog_names(
        &self,
        dir: &Path,
        prefix: &str,
        names: &mut BTreeSet<String>,
    ) -> Result<()> {
        let Ok(entries) = fs::read_dir(dir) else {
            return Ok(());
        };
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            let name = format!("{prefix}/{}", entry.file_name().to_string_lossy());
            if path.is_dir() {
                self.collect_reflog_names(&path, &name, names)?;
            } else if let Some(name) = name.strip_prefix("logs/") {
                names.insert(name.to_string());
            }
        }
        Ok(())
    }

    fn loose_refs_have_prefix(&self, prefix: &str) -> Result<bool> {
        if !prefix.starts_with("refs/") || !prefix.ends_with('/') {
            return Ok(self
                .list_refs()?
                .iter()
                .any(|reference| reference.name.starts_with(prefix)));
        }
        let loose_prefix = prefix.trim_end_matches('/');
        let dir = self.common_dir.join(loose_prefix);
        match fs::metadata(&dir) {
            Ok(meta) if meta.is_dir() => {
                let mut refs = BTreeMap::new();
                self.collect_loose_refs(&dir, loose_prefix, &mut refs)?;
                Ok(!refs.is_empty())
            }
            Ok(_) => Ok(false),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(err.into()),
        }
    }

    fn write_loose_ref(&self, reference: &Ref) -> Result<()> {
        if self.uses_reftable()? {
            let (store, name) = self.reftable_store_for_ref(&reference.name)?;
            store.append_reftable_records(vec![ReftableRefRecord {
                name,
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
        write_locked(
            &path,
            &write_loose_ref(reference),
            self.reference_fsync.method_if_enabled(),
        )
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
            if let Some(method) = self.reference_fsync.method_if_enabled() {
                sync_reference_file(&file, method)?;
            }
        }
        match fs::remove_file(&path) {
            Ok(()) => {
                fs::remove_file(lock_path)?;
                self.prune_empty_ref_dirs(name);
                Ok(())
            }
            Err(err) => {
                let _ = fs::remove_file(lock_path);
                Err(GitError::Io(err.to_string()))
            }
        }
    }

    /// Remove a loose ref after its value has been committed to packed-refs.
    ///
    /// Unlike a normal ref deletion, this is only a cleanup of a redundant
    /// copy: the packed-refs lock has already provided the durable update. Do
    /// not fsync the temporary per-ref lock, which would turn packing a large
    /// loose-ref set into one durability barrier per ref. Holding the lock
    /// still excludes cooperating writers, and the value check preserves a
    /// loose update that raced with creation of the packed snapshot.
    fn prune_loose_ref_after_pack(&self, name: &str, packed_oid: &ObjectId) -> Result<bool> {
        let path = self.ref_path(name);
        let lock_path = lock_path_for(&path)?;
        let lock = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)?;
        let outcome = (|| -> Result<bool> {
            let bytes = match fs::read(&path) {
                Ok(bytes) => bytes,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
                Err(err) => return Err(err.into()),
            };
            let reference = match parse_loose_ref(self.format, name.to_string(), &bytes) {
                Ok(reference) => reference,
                Err(_) => return Ok(false),
            };
            if reference.target != RefTarget::Direct(*packed_oid) {
                return Ok(false);
            }
            fs::remove_file(&path)?;
            Ok(true)
        })();
        drop(lock);
        let cleanup = fs::remove_file(&lock_path);
        let deleted = outcome?;
        cleanup?;
        if deleted {
            self.prune_empty_ref_dirs(name);
        }
        Ok(deleted)
    }

    /// Remove now-empty parent directories left after deleting a loose ref,
    /// stopping at the first-level namespace under `refs/`. git does this so
    /// that, e.g., deleting `refs/heads/l/m` lets `refs/heads/l` be created as a
    /// file afterwards (t3200 #14), but it keeps `refs/heads` itself around
    /// (t1400 #30).
    fn prune_empty_ref_dirs(&self, name: &str) {
        let boundary = self.ref_prune_boundary(name);
        if let Some(parent) = self.ref_path(name).parent() {
            prune_empty_dirs_up_to(parent, &boundary);
        }
    }

    fn ref_prune_boundary(&self, name: &str) -> PathBuf {
        let base = self.ref_base_dir(name).to_path_buf();
        let mut components = name.split('/');
        if components.next() == Some("refs")
            && let Some(namespace) = components.next()
        {
            return base.join("refs").join(namespace);
        }
        base
    }

    /// Remove a ref's reflog file and prune any empty parent directories it
    /// leaves behind under `logs/refs/`, stopping at the `logs/refs` boundary.
    /// Without this, deleting `refs/heads/l/m` leaves `logs/refs/heads/l/` and a
    /// later `refs/heads/l` cannot create its own `logs/refs/heads/l` reflog
    /// file (t3200 #14, #18).
    fn remove_reflog_file(&self, name: &str) {
        // Reftable repos keep the reflog inside the table stack, not as a loose
        // file: deleting a ref must tombstone its log records or `git stash
        // list` / `git reflog` keep surfacing them (t0610 'basic: stash').
        if matches!(self.uses_reftable(), Ok(true)) {
            let _ = self.tombstone_reftable_logs(name);
            return;
        }
        let path = self.reflog_path(name);
        let _ = fs::remove_file(&path);
        let base = self.ref_base_dir(name).to_path_buf();
        let logs_refs_root = base.join("logs").join("refs");
        if let Some(parent) = path.parent() {
            prune_empty_dirs_up_to(parent, &logs_refs_root);
        }
    }

    /// Mask every live log record for `name` with deletion tombstones, so the
    /// reflog reads as absent. Mirrors git unlinking `logs/<name>` on the loose
    /// backend; the reftable analogue is an all-tombstone table.
    fn tombstone_reftable_logs(&self, name: &str) -> Result<()> {
        self.rewrite_reftable_logs(name, &[], false)
    }

    /// Mirror git's `files_log_ref_write`: when a transaction updates a branch
    /// that `HEAD` symbolically points at, the same reflog entry is also written
    /// to `logs/HEAD`. Without this, `git reflog` (which reads the HEAD reflog)
    /// misses commits/merges/resets done on the checked-out branch.
    ///
    /// `head_branch` is HEAD's symref target captured **before** the transaction
    /// mutated any refs — using the post-apply value would mis-mirror a
    /// transaction that re-points HEAD onto the branch it just updated (e.g.
    /// rebase's finish step, which manages `logs/HEAD` itself). When the
    /// transaction explicitly updates `HEAD` it owns the HEAD reflog and nothing
    /// is mirrored.
    fn head_reflog_mirror(
        head_branch: Option<&str>,
        reflogs: &[(String, ReflogEntry)],
    ) -> Vec<(String, ReflogEntry)> {
        let Some(head_branch) = head_branch else {
            return Vec::new();
        };
        // A transaction that touches HEAD directly is managing the HEAD reflog
        // on its own terms (detach, rebase finish, checkout); don't double-write.
        if reflogs.iter().any(|(name, _)| name == "HEAD") {
            return Vec::new();
        }
        reflogs
            .iter()
            .filter(|(name, _)| name == head_branch)
            .map(|(_, entry)| ("HEAD".to_string(), entry.clone()))
            .collect()
    }

    /// HEAD's symref target (`refs/heads/<branch>`) if HEAD is symbolic, else
    /// `None`. Read once at the start of a transaction commit so the HEAD-reflog
    /// mirror reflects the pre-transaction state.
    fn head_symref_target(&self) -> Option<String> {
        match self.read_ref("HEAD") {
            Ok(Some(RefTarget::Symbolic(branch))) => Some(branch),
            _ => None,
        }
    }

    /// Convert a reflog entry to its reftable representation, applying the
    /// backend's message ceiling. Git truncates transaction log messages to
    /// half the configured block size before handing them to the writer so a
    /// pathological commit subject cannot make an otherwise-valid ref update
    /// fail solely because of its reflog text.
    fn reftable_update_from_reflog(&self, entry: &ReflogEntry) -> Result<ReftableLogUpdate> {
        let mut update = reftable_update_from_reflog(entry)?;
        let block_size = if self.reftable_write_options.block_size == 0 {
            ReftableWriteOptions::default().block_size
        } else {
            self.reftable_write_options.block_size
        };
        let limit = (block_size / 2) as usize;
        if update.message.len() > limit {
            let mut end = limit;
            while !update.message.is_char_boundary(end) {
                end -= 1;
            }
            update.message.truncate(end);
        }
        Ok(update)
    }

    pub fn append_reflog(&self, name: &str, entry: &ReflogEntry) -> Result<()> {
        validate_ref_name_for_read(name)?;
        if self.uses_reftable()? {
            let update = self.reftable_update_from_reflog(entry)?;
            self.append_reftable_table(
                Vec::new(),
                vec![ReftableLogRecord {
                    refname: name.to_string(),
                    update_index: 0,
                    value: ReftableLogValue::Update(update),
                }],
            )?;
            self.auto_compact_reftable_stack()?;
            return Ok(());
        }
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
        Ok(())
    }

    /// Append a completed loose-backend transaction's reflogs with one
    /// directory preparation per parent. The caller has already selected the
    /// files backend and validated every ref name while coalescing the
    /// transaction, so repeating backend/config probes and parent creation for
    /// each log entry only adds filesystem work to large batches.
    fn append_loose_reflogs(&self, reflogs: Vec<(String, ReflogEntry)>) -> Result<()> {
        let mut prepared_parents = BTreeSet::new();
        for (name, entry) in reflogs {
            let path = self.reflog_path(&name);
            let parent = path
                .parent()
                .ok_or_else(|| GitError::InvalidPath("reflog path has no parent".into()))?;
            if prepared_parents.insert(parent.to_path_buf()) {
                fs::create_dir_all(parent)?;
            }
            let mut file = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)?;
            file.write_all(&entry.to_line())?;
        }
        Ok(())
    }

    /// Replace the entire reflog for `name` in the reftable stack with `entries`
    /// (chronological, oldest first). Writes a single new table that tombstones
    /// every currently-live log update index for `name` and re-adds the desired
    /// entries at fresh update indexes that preserve their order. This is the
    /// stack-friendly analogue of rewriting a loose `logs/<name>` file — used by
    /// `write_reflog` / reflog expiry. An empty `entries` slice clears the
    /// reflog (an empty `git reflog`).
    fn rewrite_reftable_logs(
        &self,
        name: &str,
        entries: &[ReflogEntry],
        preserve_empty: bool,
    ) -> Result<()> {
        // Gather every update index that currently carries a live log record for
        // `name`, so we can mask them with deletion tombstones.
        let mut live_indexes: BTreeSet<u64> = BTreeSet::new();
        let mut deleted_indexes: BTreeSet<u64> = BTreeSet::new();
        for table in self.reftables()? {
            for record in table.logs {
                if record.refname != name {
                    continue;
                }
                match record.value {
                    ReftableLogValue::Deletion => {
                        deleted_indexes.insert(record.update_index);
                        live_indexes.remove(&record.update_index);
                    }
                    ReftableLogValue::Update(_) => {
                        live_indexes.insert(record.update_index);
                        deleted_indexes.remove(&record.update_index);
                    }
                }
            }
        }

        let table_names = self.reftable_table_names()?;
        let base = self.next_reftable_update_index(&table_names)?;
        let mut logs: Vec<ReftableLogRecord> = Vec::new();
        // Tombstone the old entries at their original update indexes.
        for index in &live_indexes {
            logs.push(ReftableLogRecord {
                refname: name.to_string(),
                update_index: *index,
                value: ReftableLogValue::Deletion,
            });
        }
        // Re-add the survivors at fresh, monotonically increasing indexes so
        // their chronological order is preserved on the next read.
        for (offset, entry) in entries.iter().enumerate() {
            let update_index = base
                .checked_add(offset as u64)
                .ok_or_else(|| GitError::InvalidFormat("reftable update index overflow".into()))?;
            logs.push(ReftableLogRecord {
                refname: name.to_string(),
                update_index,
                value: ReftableLogValue::Update(self.reftable_update_from_reflog(entry)?),
            });
        }
        if entries.is_empty() && preserve_empty {
            let null = ObjectId::null(self.format);
            logs.push(ReftableLogRecord {
                refname: name.to_string(),
                update_index: base,
                value: ReftableLogValue::Update(ReftableLogUpdate {
                    old_oid: null,
                    new_oid: null,
                    name: String::new(),
                    email: String::new(),
                    time: 0,
                    tz_offset: 0,
                    message: String::new(),
                }),
            });
        }
        if logs.is_empty() {
            return Ok(());
        }
        let leave_empty_rewrite_tombstones_separate = entries.is_empty();
        if leave_empty_rewrite_tombstones_separate
            && !reftable_autocompaction_disabled()
            && self.reftable_table_names()?.len() > 1
        {
            self.compact_reftable_stack()?;
        }
        self.append_reftable_table_spanning(Vec::new(), logs)?;
        if !leave_empty_rewrite_tombstones_separate {
            self.auto_compact_reftable_stack()?;
        }
        Ok(())
    }

    /// Like [`Self::append_reftable_table`] but the caller has already assigned
    /// each record's `update_index`; the new table's header `[min, max]` is set
    /// to span them (plus the freshly allocated index for any ref records). Used
    /// by reflog rewrites that mix old-index tombstones with new-index entries.
    fn append_reftable_table_spanning(
        &self,
        mut refs: Vec<ReftableRefRecord>,
        logs: Vec<ReftableLogRecord>,
    ) -> Result<u64> {
        let reftable_dir = self.storage_dir.join("reftable");
        fs::create_dir_all(&reftable_dir)?;
        let list_path = reftable_dir.join("tables.list");
        let list_lock = self.acquire_reftable_list_lock(list_path.clone())?;
        let mut table_names = self.reftable_table_names_from(&list_path)?;
        let alloc_index = self.next_reftable_update_index(&table_names)?;
        for record in &mut refs {
            record.update_index = alloc_index;
        }
        // Log tombstones may target update indices from older tables, but the
        // new table's range describes the update(s) allocated by this write.
        // Git therefore keeps the lower bound at `alloc_index`; including old
        // tombstone indices would overlap the preceding table's range and make
        // the stack invalid to native reftable readers.
        let min_index = alloc_index;
        let mut max_index = alloc_index;
        for record in &logs {
            max_index = max_index.max(record.update_index);
        }
        for record in &refs {
            max_index = max_index.max(record.update_index);
        }
        let table_name = reftable_table_name(min_index, max_index);
        let bytes = Reftable::write_with_options(
            self.format,
            min_index,
            max_index,
            &refs,
            &logs,
            self.reftable_write_options,
        )?;
        let table_path = reftable_dir.join(&table_name);
        write_locked(
            &table_path,
            &bytes,
            self.reference_fsync.method_if_enabled(),
        )?;
        self.apply_reftable_shared_file_mode(&table_path)?;
        table_names.push(table_name);
        let mut list = Vec::new();
        for name in &table_names {
            list.extend_from_slice(name.as_bytes());
            list.push(b'\n');
        }
        list_lock.commit(&list, self.reference_fsync.method_if_enabled())?;
        self.apply_reftable_shared_file_mode(&list_path)?;
        Ok(max_index)
    }

    fn ref_path(&self, name: &str) -> PathBuf {
        self.ref_base_dir(name).join(name)
    }

    fn reflog_path(&self, name: &str) -> PathBuf {
        self.ref_base_dir(name).join("logs").join(name)
    }

    fn ref_base_dir(&self, name: &str) -> PathBuf {
        if current_worktree_ref(name) {
            self.current_worktree_storage_dir()
        } else {
            self.storage_dir.clone()
        }
    }

    fn check_ref_directory_conflict_targeted(&self, name: &str) -> Result<()> {
        match self.refname_directory_conflict(name)? {
            Some(conflict) => Err(ref_directory_conflict_error(name, &conflict)),
            None => Ok(()),
        }
    }

    /// Return the existing ref that would directory/file-conflict with creating
    /// `name` (an ancestor occupying a needed file, or a descendant occupying a
    /// needed directory), or `None` when `name` is creatable. Checks loose and
    /// packed refs. Exposed so `update-ref --stdin --batch-updates` can reject a
    /// single conflicting update without aborting the rest of the batch.
    pub fn refname_directory_conflict(&self, name: &str) -> Result<Option<String>> {
        let components = name.split('/').collect::<Vec<_>>();
        let mut ancestors = Vec::new();
        for index in 1..components.len() {
            let ancestor = components[..index].join("/");
            if self.loose_ref_file_exists_for_conflict(&ancestor)? {
                return Ok(Some(ancestor));
            }
            ancestors.push(ancestor);
        }
        let child_prefix = format!("{name}/");
        if let Some(existing) = self.first_loose_ref_name_with_prefix(&child_prefix)? {
            return Ok(Some(existing));
        }
        if let Some(existing) =
            self.first_packed_ref_directory_conflict(&ancestors, &child_prefix)?
        {
            return Ok(Some(existing));
        }
        Ok(None)
    }

    fn loose_ref_file_exists_for_conflict(&self, name: &str) -> Result<bool> {
        let path = self.ref_path(name);
        match fs::symlink_metadata(path) {
            Ok(meta) if meta.is_dir() => Ok(false),
            Ok(_) => {
                let path = self.ref_path(name);
                if let Ok(bytes) = fs::read(path)
                    && loose_ref_bytes_are_reftable_sentinel(name, &bytes)
                {
                    return Ok(false);
                }
                Ok(true)
            }
            Err(err)
                if err.kind() == std::io::ErrorKind::NotFound
                    || err.kind() == std::io::ErrorKind::NotADirectory =>
            {
                Ok(false)
            }
            Err(err) => Err(err.into()),
        }
    }

    fn first_packed_ref_directory_conflict(
        &self,
        ancestors: &[String],
        child_prefix: &str,
    ) -> Result<Option<String>> {
        let packed_path = self.storage_dir.join("packed-refs");
        let text = match fs::read_to_string(packed_path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        for raw_line in text.lines() {
            let line = raw_line.trim_end();
            if line.is_empty() || line.starts_with('#') || line.starts_with('^') {
                continue;
            }
            let (_, name) = line
                .split_once(' ')
                .ok_or_else(|| packed_refs_unexpected_line(line))?;
            if ancestors.iter().any(|ancestor| ancestor == name) || name.starts_with(child_prefix) {
                return Ok(Some(name.to_owned()));
            }
        }
        Ok(None)
    }

    fn first_loose_ref_name_with_prefix(&self, prefix: &str) -> Result<Option<String>> {
        if !prefix.starts_with("refs/") || !prefix.ends_with('/') {
            return Ok(None);
        }
        let trimmed = prefix.trim_end_matches('/');
        let dir = self.ref_base_dir(trimmed).join(trimmed);
        self.first_loose_ref_name_in_dir(&dir, trimmed)
    }

    fn first_loose_ref_name_in_dir(&self, dir: &Path, prefix: &str) -> Result<Option<String>> {
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(err)
                if err.kind() == std::io::ErrorKind::NotFound
                    || err.kind() == std::io::ErrorKind::NotADirectory =>
            {
                return Ok(None);
            }
            Err(err) => return Err(err.into()),
        };
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            let name = format!("{prefix}/{}", entry.file_name().to_string_lossy());
            if path.is_dir() {
                if let Some(found) = self.first_loose_ref_name_in_dir(&path, &name)? {
                    return Ok(Some(found));
                }
            } else if !name.ends_with(".lock") {
                return Ok(Some(name));
            }
        }
        Ok(None)
    }
}

fn reftable_ref_target(value: ReftableRefValue) -> Result<Option<RefTarget>> {
    match value {
        ReftableRefValue::Deletion => Ok(None),
        ReftableRefValue::Direct(oid) | ReftableRefValue::Peeled { target: oid, .. } => {
            Ok(Some(RefTarget::Direct(oid)))
        }
        ReftableRefValue::Symbolic(target) => Ok(Some(RefTarget::Symbolic(target))),
    }
}

fn reftable_value_from_ref_target(target: &RefTarget) -> ReftableRefValue {
    match target {
        RefTarget::Direct(oid) => ReftableRefValue::Direct(*oid),
        RefTarget::Symbolic(target) => ReftableRefValue::Symbolic(target.clone()),
    }
}

/// Reconstruct a `ReflogEntry`'s flat committer line from a reftable log update.
///
/// git's reftable backend stores the identity split into `name`/`email`/`time`/
/// `tz_offset` (refs/reftable-backend.c::fill_reftable_log_record) and rebuilds
/// `Name <email> time tz` on read. The loose reflog committer field is exactly
/// that string, so the entry round-trips byte-for-byte with the loose backend.
fn reflog_entry_from_reftable(update: ReftableLogUpdate) -> ReflogEntry {
    let committer = format!(
        "{} <{}> {} {}",
        update.name,
        update.email,
        update.time,
        format_reflog_tz(update.tz_offset),
    );
    // git stores reflog messages with a trailing newline in the reftable record
    // (refs/reftable-backend.c passes `u->msg`, which carries the `\n`). A loose
    // `ReflogEntry.message` is the newline-free form, so strip the single
    // trailing `\n` we add on write.
    let mut message = update.message.into_bytes();
    if message.last() == Some(&b'\n') {
        message.pop();
    }
    ReflogEntry {
        old_oid: update.old_oid,
        new_oid: update.new_oid,
        committer: committer.into_bytes(),
        message,
    }
}

fn reftable_log_update_is_empty_marker(update: &ReftableLogUpdate, null: ObjectId) -> bool {
    update.old_oid == null && update.new_oid == null
}

/// Split a flat reflog committer line (`Name <email> <seconds> <±HHMM>`) plus the
/// entry's oids/message into the reftable log update fields.
fn reftable_update_from_reflog(entry: &ReflogEntry) -> Result<ReftableLogUpdate> {
    let committer = std::str::from_utf8(&entry.committer)
        .map_err(|err| GitError::InvalidFormat(err.to_string()))?;
    let (name, email, time, tz_offset) = split_committer_ident(committer)?;
    let mut message = std::str::from_utf8(&entry.message)
        .map_err(|err| GitError::InvalidFormat(err.to_string()))?
        .to_string();
    // git stores reflog messages with a trailing newline in reftable records;
    // `%gs` and the loose backend strip it. Add it back so git renders the
    // message identically across backends.
    if !message.ends_with('\n') {
        message.push('\n');
    }
    Ok(ReftableLogUpdate {
        old_oid: entry.old_oid,
        new_oid: entry.new_oid,
        name,
        email,
        time,
        tz_offset,
        message,
    })
}

/// Parse `Name <email> <seconds> <±HHMM>` into the reftable log fields. Mirrors
/// git's `split_ident_line` semantics for the committer line: the display name
/// is everything before ` <`, the email is between the angle brackets, then the
/// unix timestamp and the signed `HHMM` timezone follow.
fn split_committer_ident(committer: &str) -> Result<(String, String, u64, i16)> {
    let open = committer.find(" <").ok_or_else(|| {
        GitError::InvalidFormat("reflog committer is missing email opener".into())
    })?;
    let name = committer[..open].to_string();
    let after_open = open + 2;
    let close = committer[after_open..].find('>').ok_or_else(|| {
        GitError::InvalidFormat("reflog committer is missing email closer".into())
    })?;
    let email = committer[after_open..after_open + close].to_string();
    let rest = committer[after_open + close + 1..].trim();
    let (time_str, tz_str) = rest.split_once(' ').ok_or_else(|| {
        GitError::InvalidFormat("reflog committer is missing timestamp/timezone".into())
    })?;
    let time = time_str
        .trim()
        .parse::<u64>()
        .map_err(|err| GitError::InvalidFormat(err.to_string()))?;
    let tz_offset = parse_reflog_tz(tz_str.trim())?;
    Ok((name, email, time, tz_offset))
}

/// Format a reftable `tz_offset` (a signed `HHMM` value) as git renders it in a
/// committer line, e.g. `120 -> "+0200"`, `-300 -> "-0500"`.
fn format_reflog_tz(tz_offset: i16) -> String {
    let sign = if tz_offset < 0 { '-' } else { '+' };
    let magnitude = tz_offset.unsigned_abs();
    format!("{sign}{magnitude:04}")
}

/// Parse a `±HHMM` timezone token into the raw signed `HHMM` value git stores in
/// a reftable log record (refs/reftable-backend.c parses it the same way).
fn parse_reflog_tz(tz: &str) -> Result<i16> {
    let (sign, digits) = match tz.strip_prefix('-') {
        Some(rest) => (-1i16, rest),
        None => (1i16, tz.strip_prefix('+').unwrap_or(tz)),
    };
    let magnitude = digits
        .parse::<i16>()
        .map_err(|err| GitError::InvalidFormat(err.to_string()))?;
    Ok(sign * magnitude)
}

/// Build a reftable file name in git's exact `0x%012x-0x%012x-%08x.ref` shape
/// (reftable/stack.c::format_name). git's `table_has_valid_name` (reftable/fsck.c)
/// parses all three dash-separated components as hex, so the suffix MUST be a
/// pure 8-hex-digit token — a non-hex disambiguator like `-sley-<nanos>` makes
/// `git fsck` reject the table with `badReftableTableName`.
fn reftable_table_name(min_update_index: u64, max_update_index: u64) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    // Mix the process id in so concurrent writers in the same nanosecond still
    // pick distinct names; truncate to 32 bits to match git's `%08x`.
    let salt = (nanos as u64) ^ (u64::from(std::process::id()) << 16);
    format!(
        "0x{min_update_index:012x}-0x{max_update_index:012x}-{:08x}.ref",
        salt as u32
    )
}

fn reftable_autocompaction_disabled() -> bool {
    std::env::var("GIT_TEST_REFTABLE_AUTOCOMPACTION")
        .is_ok_and(|value| value.eq_ignore_ascii_case("false"))
}

/// Select the oldest-first, end-exclusive table range that must be merged to
/// restore the geometric size progression used by Git's reftable stack.
fn reftable_compaction_segment(sizes: &[u64], factor: u64) -> Option<(usize, usize)> {
    if sizes.len() <= 1 {
        return None;
    }
    let factor = if factor == 0 { 2 } else { factor };

    let mut pivot = None;
    let mut end = 0;
    for index in (1..sizes.len()).rev() {
        if sizes[index - 1] < sizes[index].saturating_mul(factor) {
            pivot = Some(index);
            end = index + 1;
            break;
        }
    }
    let mut index = pivot?;
    let mut bytes = sizes[index];
    let mut start = index;
    while index > 0 {
        let current = bytes;
        bytes = bytes.saturating_add(sizes[index - 1]);
        if sizes[index - 1] < current.saturating_mul(factor) {
            start = index - 1;
        }
        index -= 1;
    }
    (start < end).then_some((start, end))
}

/// Whether `name` parses as a valid reftable file name the way git's
/// `table_has_valid_name` (reftable/fsck.c) does: three hex tokens separated by
/// `-`, ending in `.ref` (or `.log`). Used to keep sley's generated names from
/// regressing into a shape `git fsck` would reject.
#[cfg(test)]
fn reftable_table_name_is_valid(name: &str) -> bool {
    fn hex_prefix(s: &str) -> Option<&str> {
        // strtoull(base 16) skips an optional leading 0x and consumes hex digits.
        let body = s
            .strip_prefix("0x")
            .or_else(|| s.strip_prefix("0X"))
            .unwrap_or(s);
        let consumed = body
            .find(|c: char| !c.is_ascii_hexdigit())
            .unwrap_or(body.len());
        if consumed == 0 {
            return None;
        }
        Some(&body[consumed..])
    }
    let Some(rest) = hex_prefix(name) else {
        return false;
    };
    let Some(rest) = rest.strip_prefix('-') else {
        return false;
    };
    let Some(rest) = hex_prefix(rest) else {
        return false;
    };
    let Some(rest) = rest.strip_prefix('-') else {
        return false;
    };
    let Some(rest) = hex_prefix(rest) else {
        return false;
    };
    rest == ".ref" || rest == ".log"
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

fn log_all_ref_updates_matches(name: &str, value: &str) -> bool {
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

/// The phase a [`ReferenceTransactionHook`] is invoked for, mirroring the
/// `state` argument git passes to the `reference-transaction` hook
/// (`refs.c:run_transaction_hook`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RefTransactionPhase {
    /// Before references are locked. A nonzero hook exit aborts the
    /// transaction with `in 'preparing' phase, update aborted ...`.
    Preparing,
    /// After references are locked but before they are written. A nonzero
    /// hook exit aborts with `in 'prepared' phase, update aborted ...`.
    Prepared,
    /// After every ref change has landed. The hook's exit status is ignored.
    Committed,
    /// When a prepared transaction is rolled back. The exit status is ignored.
    Aborted,
}

impl RefTransactionPhase {
    /// The literal `state` string git feeds as `argv[1]` to the hook.
    pub fn as_str(self) -> &'static str {
        match self {
            RefTransactionPhase::Preparing => "preparing",
            RefTransactionPhase::Prepared => "prepared",
            RefTransactionPhase::Committed => "committed",
            RefTransactionPhase::Aborted => "aborted",
        }
    }
}

/// One queued ref change as the `reference-transaction` hook sees it: the
/// `<old-value> SP <new-value> SP <refname>` triple git writes to the hook's
/// stdin (`refs.c:transaction_hook_feed_stdin`). `old_value`/`new_value` are
/// already rendered the way git renders them — a 40/64-hex OID, the string
/// `ref:<target>` for a symref, or the all-zeros OID when the side is absent.
#[derive(Clone, Debug)]
pub struct RefTransactionHookUpdate {
    pub old_value: String,
    pub new_value: String,
    pub refname: String,
}

/// A handler the file backend invokes at each phase of a ref transaction so the
/// CLI layer can run the project's `reference-transaction` hook. Implemented in
/// `sley-cli`; the backend stays oblivious to how (or whether) a hook script is
/// found and executed.
///
/// `run` returns `Ok(true)` to mean "the hook ran and requested an abort"
/// (nonzero exit in a `preparing`/`prepared` phase), `Ok(false)` to mean
/// "proceed" (hook absent, succeeded, or a non-abortable phase), and `Err` only
/// for an I/O failure spawning the hook. The backend turns an abort request in
/// the prepare phases into the git-shaped `in '<phase>' phase, update aborted by
/// the reference-transaction hook` failure.
pub trait ReferenceTransactionHook {
    fn run(&self, phase: RefTransactionPhase, updates: &[RefTransactionHookUpdate])
    -> Result<bool>;
}

pub struct FileRefTransaction<'a> {
    store: &'a FileRefStore,
    changes: Vec<QueuedRefChange>,
    hook: Option<&'a dyn ReferenceTransactionHook>,
}

/// One queued update inside a [`FileRefTransaction`], carrying the
/// compare-and-swap precondition to enforce under lock.
struct QueuedUpdate {
    name: String,
    precondition: RefPrecondition,
    new: RefTarget,
    reflog: Option<ReflogEntry>,
}

struct QueuedDelete {
    name: String,
    precondition: RefDeletePrecondition,
}

enum QueuedRefChange {
    Update(QueuedUpdate),
    Delete(QueuedDelete),
}

/// The compare-and-delete precondition checked for a queued ref delete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefDeletePrecondition {
    /// Any existing direct or symbolic ref may be deleted.
    Any,
    /// The ref's immediate target must match exactly.
    Immediate(RefTarget),
    /// The ref must be direct. When an object id is supplied, it must match.
    Direct(Option<ObjectId>),
    /// The ref may be symbolic, but its peeled direct target must match.
    Peeled(ObjectId),
}

impl<'a> FileRefTransaction<'a> {
    /// Attach the `reference-transaction` hook handler this transaction fires at
    /// each phase. Without one the transaction behaves exactly as before (no
    /// hook is run). This is the single point through which every ref-write path
    /// — `update-ref`, `symbolic-ref`, `update-ref --stdin`, push — gets hook
    /// coverage, so a new write site cannot silently skip the hook.
    pub fn with_hook(mut self, hook: &'a dyn ReferenceTransactionHook) -> Self {
        self.hook = Some(hook);
        self
    }

    /// Queue a ref update whose precondition comes from [`RefUpdate::expected`]
    /// (`None` = no check; `Some(target)` = the ref must currently match
    /// `target`). For create-only or match-or-create semantics use
    /// [`update_to`](FileRefTransaction::update_to).
    pub fn update(&mut self, update: RefUpdate) {
        self.changes.push(QueuedRefChange::Update(QueuedUpdate {
            name: update.name,
            precondition: RefPrecondition::from_expected(update.expected),
            new: update.new,
            reflog: update.reflog,
        }));
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
        self.changes.push(QueuedRefChange::Update(QueuedUpdate {
            name: name.into(),
            precondition,
            new,
            reflog,
        }));
    }

    /// Queue a direct ref delete using the historical checked-delete shape.
    ///
    /// `expected_old = None` means "delete any direct ref"; `Some(oid)` means
    /// the direct ref must currently point at that object id.
    pub fn delete(&mut self, delete: DeleteRef) {
        self.delete_with_precondition(
            delete.name,
            RefDeletePrecondition::Direct(delete.expected_old),
            delete.reflog,
        );
    }

    /// Queue a ref delete with an explicit direct/symbolic precondition.
    ///
    /// `_reflog` is accepted for API compatibility but ignored: git unlinks the
    /// reflog on delete rather than writing a deletion entry, so a
    /// caller-supplied deletion message has no on-disk effect.
    pub fn delete_with_precondition(
        &mut self,
        name: impl Into<String>,
        precondition: RefDeletePrecondition,
        _reflog: Option<DeleteRefReflog>,
    ) {
        self.changes.push(QueuedRefChange::Delete(QueuedDelete {
            name: name.into(),
            precondition,
        }));
    }

    /// Commit all queued updates and deletes atomically and durably.
    ///
    /// All ref changes succeed together or none take effect. For the loose-ref
    /// backend the sequence is:
    ///
    /// 1. Preserve the historical update-only coalescing behavior. Mixed
    ///    transactions reject duplicate ref names so a delete and write cannot
    ///    target the same ref ambiguously.
    /// 2. Take an exclusive `<ref>.lock` file for every ref up front, and lock
    ///    `packed-refs` before checked deletes can inspect or rewrite it.
    /// 3. Re-verify every precondition *while holding the locks*, closing
    ///    the check-then-write race that a pre-lock verification would leave open.
    /// 4. Stage every write, delete marker, and packed-refs rewrite.
    /// 5. Rename/remove staged paths, rolling back already-applied paths if a
    ///    later step fails.
    ///
    /// If any step fails, every path already changed in this commit is restored
    /// to the exact bytes it held beforehand (or removed if it did not exist),
    /// and all outstanding lock files are deleted. Reflog entries are appended
    /// only after every ref change has landed.
    pub fn commit(self) -> Result<()> {
        if env::var_os("GIT_QUARANTINE_PATH").is_some() {
            return Err(GitError::Transaction(
                "ref updates forbidden inside quarantine environment".into(),
            ));
        }
        let FileRefTransaction {
            store,
            changes,
            hook,
        } = self;
        let changes = coalesce_ref_changes(changes)?;
        // Derive the `<old> <new> <refname>` lines the reference-transaction
        // hook sees, in the same coalesced order the writes apply. This is the
        // single place the hook is fed, so loose, packed, and symref updates all
        // flow through one firing path (git's run_transaction_hook).
        let hook_updates = hook.map(|_| hook_updates_for_changes(store.format, &changes));
        if let (Some(hook), Some(updates)) = (hook, hook_updates.as_ref())
            && hook.run(RefTransactionPhase::Preparing, updates)?
        {
            return Err(ref_transaction_hook_abort(RefTransactionPhase::Preparing));
        }
        if store.uses_reftable()? {
            store.commit_reftable(changes)
        } else {
            store.commit_loose_hooked(changes, hook, hook_updates.as_deref())
        }
    }
}

/// The git-shaped fatal raised when the `reference-transaction` hook requests an
/// abort in the `preparing`/`prepared` phase (`refs.c:abort_by_ref_transaction_hook`).
fn ref_transaction_hook_abort(phase: RefTransactionPhase) -> GitError {
    GitError::Transaction(format!(
        "in '{}' phase, update aborted by the reference-transaction hook",
        phase.as_str()
    ))
}

/// Render the per-update hook lines for a coalesced change set, matching git's
/// `transaction_hook_feed_stdin`: the old side is `null_oid` when no old value
/// was required, `ref:<target>` for a symref precondition, else the expected
/// OID; the new side is `null_oid` for a delete, `ref:<target>` for a new
/// symref, else the new OID.
fn hook_updates_for_changes(
    format: ObjectFormat,
    changes: &[CoalescedRefChange],
) -> Vec<RefTransactionHookUpdate> {
    let zero = ObjectId::null(format).to_string();
    changes
        .iter()
        .map(|change| match change {
            CoalescedRefChange::Update(update) => RefTransactionHookUpdate {
                old_value: hook_old_value(&zero, &update.precondition),
                new_value: hook_target_value(&zero, Some(&update.new)),
                refname: update.name.clone(),
            },
            CoalescedRefChange::Delete(delete) => RefTransactionHookUpdate {
                old_value: hook_delete_old_value(&zero, &delete.precondition),
                new_value: zero.clone(),
                refname: delete.name.clone(),
            },
        })
        .collect()
}

/// The hook's `<old-value>` for an update: git prints `null_oid` unless the
/// caller supplied an old value (`REF_HAVE_OLD`), in which case it is the
/// expected target (a `ref:` for a symref expectation, else the OID).
fn hook_old_value(zero: &str, precondition: &RefPrecondition) -> String {
    match precondition {
        RefPrecondition::Any | RefPrecondition::MustExist => zero.to_string(),
        RefPrecondition::MustNotExist => zero.to_string(),
        RefPrecondition::MustExistAndMatch(target) | RefPrecondition::ExistingMustMatch(target) => {
            hook_target_value(zero, Some(target))
        }
    }
}

/// The hook's `<old-value>` for a delete: git renders the supplied old OID, or
/// `null_oid` when none was required.
fn hook_delete_old_value(zero: &str, precondition: &RefDeletePrecondition) -> String {
    match precondition {
        RefDeletePrecondition::Any => zero.to_string(),
        RefDeletePrecondition::Immediate(target) => hook_target_value(zero, Some(target)),
        RefDeletePrecondition::Direct(Some(oid)) | RefDeletePrecondition::Peeled(oid) => {
            oid.to_string()
        }
        RefDeletePrecondition::Direct(None) => zero.to_string(),
    }
}

/// Render a [`RefTarget`] the way git renders a hook value: `ref:<target>` for a
/// symref, the bare OID for a direct ref, or `null_oid` when absent.
fn hook_target_value(zero: &str, target: Option<&RefTarget>) -> String {
    match target {
        None => zero.to_string(),
        Some(RefTarget::Direct(oid)) => oid.to_string(),
        Some(RefTarget::Symbolic(name)) => format!("ref:{name}"),
    }
}

impl FileRefStore {
    fn commit_reftable(&self, changes: Vec<CoalescedRefChange>) -> Result<()> {
        let routed = self.route_reftable_changes(changes)?;
        if routed.len() != 1 || routed[0].0.storage_dir != self.storage_dir {
            for (store, changes) in routed {
                store.commit_reftable_local(changes)?;
            }
            return Ok(());
        }
        let mut routed = routed;
        if let Some((_, changes)) = routed.pop() {
            self.commit_reftable_local(changes)
        } else {
            Ok(())
        }
    }

    fn commit_reftable_local(&self, changes: Vec<CoalescedRefChange>) -> Result<()> {
        // Capture HEAD's symref target only when there is a reflog to mirror.
        let has_reflogs = changes.iter().any(|change| {
            matches!(change, CoalescedRefChange::Update(update) if !update.reflog.is_empty())
        });
        let head_branch = if has_reflogs {
            self.head_symref_target()
        } else {
            None
        };
        if changes
            .iter()
            .any(|change| matches!(change, CoalescedRefChange::Update(_)))
        {
            let mut names = self
                .list_refs()?
                .into_iter()
                .map(|reference| reference.name)
                .collect::<BTreeSet<_>>();
            for change in &changes {
                if let CoalescedRefChange::Update(update) = change {
                    names.insert(update.name.clone());
                }
            }
            for change in &changes {
                if let CoalescedRefChange::Update(update) = change {
                    check_ref_directory_conflict_in_names(&update.name, &names)?;
                }
            }
        }
        let mut records = Vec::with_capacity(changes.len());
        let mut reflogs = Vec::new();
        let mut delete_names = Vec::new();
        for change in changes {
            match change {
                CoalescedRefChange::Update(update) => {
                    if !matches!(update.precondition, RefPrecondition::Any) {
                        let current = self.read_ref(&update.name)?;
                        if !update.precondition.is_satisfied_by(current.as_ref()) {
                            return Err(GitError::Transaction(
                                update.precondition.describe(&update.name),
                            ));
                        }
                    }
                    records.push(ReftableRefRecord {
                        name: update.name.clone(),
                        update_index: 0,
                        value: reftable_value_from_ref_target(&update.new),
                    });
                    for entry in update.reflog {
                        reflogs.push((update.name.clone(), entry));
                    }
                }
                CoalescedRefChange::Delete(delete) => {
                    let current = self.read_ref(&delete.name)?;
                    // Enforce the precondition; git unlinks logs/refs/<name> on
                    // delete rather than appending a deletion reflog entry, so the
                    // returned OID is unused.
                    verify_delete_precondition(
                        self,
                        &delete.name,
                        current.as_ref(),
                        &delete.precondition,
                    )?;
                    records.push(ReftableRefRecord {
                        name: delete.name.clone(),
                        update_index: 0,
                        value: ReftableRefValue::Deletion,
                    });
                    delete_names.push(delete.name.clone());
                }
            }
        }
        if self.combine_reftable_logs || self.git_dir != self.common_dir {
            let head_mirror = Self::head_reflog_mirror(head_branch.as_deref(), &reflogs);
            reflogs.extend(head_mirror);
            let log_records = reflogs
                .into_iter()
                .map(|(name, entry)| {
                    Ok(ReftableLogRecord {
                        refname: name,
                        update_index: 0,
                        value: ReftableLogValue::Update(self.reftable_update_from_reflog(&entry)?),
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            self.append_reftable_table_with_compaction(records, log_records, false)?;
            for name in &delete_names {
                self.remove_reflog_file(name);
            }
            // Shared stacks use the native combined ref+log transaction shape
            // for update-ref and should auto-compact at the transaction
            // boundary. Per-worktree stacks currently retain their append-only
            // shape: compacting a two-table worktree stack here would collapse
            // a geometric sequence Git leaves intact.
            if self.storage_dir == self.shared_reftable_storage_dir() {
                self.auto_compact_reftable_stack()?;
            }
            return Ok(());
        }
        self.append_reftable_records(records)?;
        // Git unlinks logs/refs/<name> (pruning now-empty dirs) on delete; do
        // this before appending update reflogs so a delete+recreate does not race
        // the new ref's reflog file.
        for name in &delete_names {
            self.remove_reflog_file(name);
        }
        let head_mirror = Self::head_reflog_mirror(head_branch.as_deref(), &reflogs);
        reflogs.extend(head_mirror);
        for (name, entry) in reflogs {
            self.append_reflog(&name, &entry)?;
        }
        Ok(())
    }

    fn route_reftable_changes(
        &self,
        changes: Vec<CoalescedRefChange>,
    ) -> Result<Vec<(FileRefStore, Vec<CoalescedRefChange>)>> {
        let mut grouped = BTreeMap::<PathBuf, (FileRefStore, Vec<CoalescedRefChange>)>::new();
        for change in changes {
            let name = match &change {
                CoalescedRefChange::Update(update) => update.name.as_str(),
                CoalescedRefChange::Delete(delete) => delete.name.as_str(),
            };
            let (store, rewritten) = self.reftable_store_for_ref(name)?;
            let rewritten_change = match change {
                CoalescedRefChange::Update(mut update) => {
                    update.name = rewritten;
                    CoalescedRefChange::Update(update)
                }
                CoalescedRefChange::Delete(mut delete) => {
                    delete.name = rewritten;
                    CoalescedRefChange::Delete(delete)
                }
            };
            grouped
                .entry(store.storage_dir.clone())
                .or_insert_with(|| (store, Vec::new()))
                .1
                .push(rewritten_change);
        }
        Ok(grouped.into_values().collect())
    }

    /// Atomic, all-or-nothing commit for the loose-ref backend. See
    /// [`FileRefTransaction::commit`] for the full ordering and rollback rules.
    #[allow(dead_code)]
    fn commit_loose(&self, changes: Vec<CoalescedRefChange>) -> Result<()> {
        self.commit_loose_hooked(changes, None, None)
    }

    /// As [`commit_loose`](Self::commit_loose) but firing the
    /// `reference-transaction` hook at `prepared` (after every ref is locked and
    /// staged, before any rename) and `committed` (after every change lands).
    /// A nonzero hook exit in the `prepared` phase rolls the staged changes back
    /// and surfaces the git-shaped abort error.
    fn commit_loose_hooked(
        &self,
        changes: Vec<CoalescedRefChange>,
        hook: Option<&dyn ReferenceTransactionHook>,
        hook_updates: Option<&[RefTransactionHookUpdate]>,
    ) -> Result<()> {
        // Capture HEAD's symref target only when there is a reflog to mirror.
        let has_reflogs = changes.iter().any(|change| {
            matches!(change, CoalescedRefChange::Update(update) if !update.reflog.is_empty())
        });
        let head_branch = if has_reflogs {
            self.head_symref_target()
        } else {
            None
        };
        let has_delete = changes
            .iter()
            .any(|change| matches!(change, CoalescedRefChange::Delete(_)));
        let update_count = changes
            .iter()
            .filter(|change| matches!(change, CoalescedRefChange::Update(_)))
            .count();
        let targeted_conflict_check = update_count == 1 && !has_delete;
        let conflict_names = if update_count > 0 && !targeted_conflict_check {
            let mut names = self
                .list_refs()?
                .into_iter()
                .map(|reference| reference.name)
                .collect::<BTreeSet<_>>();
            for change in &changes {
                if let CoalescedRefChange::Update(update) = change {
                    names.insert(update.name.clone());
                }
            }
            Some(names)
        } else {
            None
        };
        let mut pending = Vec::with_capacity(changes.len() + usize::from(has_delete));
        let mut prepared_parents = BTreeSet::new();
        // Acquire every lock first; bail (releasing what we hold) on any failure.
        for change in &changes {
            let name = change.name();
            if matches!(change, CoalescedRefChange::Update(_)) {
                let conflict_result = if targeted_conflict_check {
                    self.check_ref_directory_conflict_targeted(name)
                } else if let Some(conflict_names) = conflict_names.as_ref() {
                    check_ref_directory_conflict_in_names(name, conflict_names)
                } else {
                    Ok(())
                };
                if let Err(err) = conflict_result {
                    release_pending_locks(&pending);
                    return Err(err);
                }
            }
            let path = self.ref_path(name);
            let parent = path
                .parent()
                .ok_or_else(|| GitError::InvalidPath("ref path has no parent".into()))?;
            if prepared_parents.insert(parent.to_path_buf())
                && let Err(err) = fs::create_dir_all(parent)
            {
                release_pending_locks(&pending);
                if err.kind() == std::io::ErrorKind::NotADirectory {
                    return Err(ref_directory_conflict_error(
                        name,
                        &parent_to_ref_name(&self.ref_base_dir(name), parent),
                    ));
                }
                return Err(GitError::Io(err.to_string()));
            }
            let lock_path = match lock_path_for(&path) {
                Ok(lock_path) => lock_path,
                Err(err) => {
                    release_pending_locks(&pending);
                    return Err(err);
                }
            };
            let action = match change {
                CoalescedRefChange::Update(update) => PendingPathAction::Write {
                    contents: write_loose_ref(&Ref {
                        name: update.name.clone(),
                        target: update.new.clone(),
                    }),
                },
                CoalescedRefChange::Delete(_) => PendingPathAction::Delete,
            };
            let mut lock_file = match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&lock_path)
            {
                Ok(file) => file,
                Err(err) => {
                    release_pending_locks(&pending);
                    return Err(GitError::Io(format!("could not lock ref {name}: {err}")));
                }
            };
            // The lock file is private until it is renamed into place, so it
            // is safe to stage its bytes as soon as the lock is acquired. This
            // keeps the all-locks-before-apply and rollback guarantees while
            // avoiding a second open/truncate pass over large transactions.
            let stage_result = match &action {
                PendingPathAction::Write { contents } => lock_file.write_all(contents),
                PendingPathAction::Delete => lock_file.write_all(b"delete\n"),
                PendingPathAction::ReleaseLock => Ok(()),
            };
            drop(lock_file);
            if let Err(err) = stage_result {
                let _ = fs::remove_file(&lock_path);
                release_pending_locks(&pending);
                return Err(err.into());
            }
            pending.push(PendingPathChange {
                name: name.to_string(),
                path,
                lock_path,
                original: None,
                action,
                staged: true,
            });
        }

        let packed_path = self.storage_dir.join("packed-refs");
        let mut packed_refs = Vec::new();
        let mut use_packed_snapshot = false;
        if has_delete {
            let packed_lock_path = match lock_path_for(&packed_path) {
                Ok(lock_path) => lock_path,
                Err(err) => {
                    release_pending_locks(&pending);
                    return Err(err);
                }
            };
            if let Err(err) = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&packed_lock_path)
            {
                release_pending_locks(&pending);
                return Err(GitError::Io(format!("could not lock packed-refs: {err}")));
            }
            let packed_original = match fs::read(&packed_path) {
                Ok(bytes) => Some(bytes),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
                Err(err) => {
                    release_pending_locks(&pending);
                    let _ = fs::remove_file(&packed_lock_path);
                    return Err(GitError::Io(err.to_string()));
                }
            };
            packed_refs = match &packed_original {
                Some(bytes) => match parse_packed_refs(self.format, bytes) {
                    Ok(refs) => refs,
                    Err(err) => {
                        release_pending_locks(&pending);
                        let _ = fs::remove_file(&packed_lock_path);
                        return Err(err);
                    }
                },
                None => Vec::new(),
            };
            use_packed_snapshot = true;
            pending.push(PendingPathChange {
                name: "packed-refs".into(),
                path: packed_path.clone(),
                lock_path: packed_lock_path,
                original: packed_original,
                action: PendingPathAction::ReleaseLock,
                staged: false,
            });
        } else if packed_path.exists() {
            packed_refs = parse_packed_refs(self.format, &fs::read(&packed_path)?)?;
            use_packed_snapshot = true;
        }
        let packed_ref_targets = packed_refs
            .iter()
            .map(|reference| {
                (
                    reference.reference.name.clone(),
                    reference.reference.target.clone(),
                )
            })
            .collect::<HashMap<_, _>>();

        // Verify expectations under lock, then capture prior on-disk state for
        // rollback. Mixed transactions read packed refs from the snapshot held
        // behind packed-refs.lock so deletes cannot race a packed rewrite.
        let mut reflogs = Vec::new();
        let mut delete_names = BTreeSet::new();
        for index in 0..changes.len() {
            match &changes[index] {
                CoalescedRefChange::Update(update) => {
                    let (current, has_loose) = if use_packed_snapshot {
                        match self.read_locked_ref_state(&update.name, &packed_ref_targets) {
                            Ok(state) => (state.current, state.has_loose),
                            Err(err) => {
                                release_pending_locks(&pending);
                                return Err(err);
                            }
                        }
                    } else {
                        match self.read_loose_ref(&update.name) {
                            Ok(loose) => {
                                let has_loose = loose.is_some();
                                (loose.map(|reference| reference.target), has_loose)
                            }
                            Err(err) => {
                                release_pending_locks(&pending);
                                return Err(err);
                            }
                        }
                    };
                    if !matches!(update.precondition, RefPrecondition::Any)
                        && !update.precondition.is_satisfied_by(current.as_ref())
                    {
                        release_pending_locks(&pending);
                        return Err(GitError::Transaction(
                            update.precondition.describe(&update.name),
                        ));
                    }
                    pending[index].original = if has_loose {
                        match read_optional_file(&pending[index].path) {
                            Ok(original) => original,
                            Err(err) => {
                                release_pending_locks(&pending);
                                return Err(err);
                            }
                        }
                    } else {
                        None
                    };
                    if pending[index].original.is_none() && current.as_ref() == Some(&update.new) {
                        pending[index].action = PendingPathAction::ReleaseLock;
                    }
                    for entry in &update.reflog {
                        reflogs.push((update.name.clone(), entry.clone()));
                    }
                }
                CoalescedRefChange::Delete(delete) => {
                    let state = match self.read_locked_ref_state(&delete.name, &packed_ref_targets)
                    {
                        Ok(state) => state,
                        Err(err) => {
                            release_pending_locks(&pending);
                            return Err(err);
                        }
                    };
                    // Enforce the delete precondition under lock; the returned
                    // OID is unused because git unlinks logs/refs/<name> on
                    // delete rather than appending a deletion reflog entry.
                    if let Err(err) = verify_delete_precondition(
                        self,
                        &delete.name,
                        state.current.as_ref(),
                        &delete.precondition,
                    ) {
                        release_pending_locks(&pending);
                        return Err(err);
                    }
                    pending[index].original = if state.has_loose {
                        match read_optional_file(&pending[index].path) {
                            Ok(original) => original,
                            Err(err) => {
                                release_pending_locks(&pending);
                                return Err(err);
                            }
                        }
                    } else {
                        None
                    };
                    delete_names.insert(delete.name.clone());
                }
            }
        }

        if has_delete {
            let old_len = packed_refs.len();
            packed_refs.retain(|reference| !delete_names.contains(&reference.reference.name));
            if packed_refs.len() != old_len {
                let packed_bytes = match write_packed_refs(&packed_refs) {
                    Ok(bytes) => bytes,
                    Err(err) => {
                        release_pending_locks(&pending);
                        return Err(err);
                    }
                };
                if let Some(packed) = pending.last_mut() {
                    packed.action = PendingPathAction::Write {
                        contents: packed_bytes,
                    };
                }
            }
        }

        // Stage every new value or delete marker into its lock file. Nothing has
        // been renamed or removed yet, so on failure we only drop lock files.
        for change in &mut pending {
            if let Err(err) = stage_pending_change(change) {
                release_pending_locks(&pending);
                return Err(err);
            }
        }

        // git fires the `prepared` hook once every ref is locked, before any
        // value is renamed into place. A nonzero exit drops the staged lock
        // files (no on-disk change happened yet) and aborts.
        if let (Some(hook), Some(updates)) = (hook, hook_updates)
            && hook.run(RefTransactionPhase::Prepared, updates)?
        {
            release_pending_locks(&pending);
            return Err(ref_transaction_hook_abort(RefTransactionPhase::Prepared));
        }

        // Apply each staged path change; on failure restore paths already
        // changed and drop the remaining lock files.
        for index in 0..pending.len() {
            if let Err(err) = maybe_fail_loose_commit_action(index) {
                rollback_after_apply(&pending, index);
                return Err(err);
            }
            if let Err(err) = apply_pending_change(&pending[index]) {
                rollback_after_apply(&pending, index + 1);
                return Err(err);
            }
            if matches!(pending[index].action, PendingPathAction::Delete) {
                self.prune_empty_ref_dirs(&pending[index].name);
            }
        }

        // Git unlinks logs/refs/<name> (and prunes now-empty log dirs) on delete;
        // do this before appending update reflogs so a delete+recreate in the
        // same direction does not race the new ref's reflog file.
        for name in &delete_names {
            self.remove_reflog_file(name);
        }
        // git's `files_log_ref_write` mirrors a checked-out branch's reflog
        // entry into logs/HEAD; `head_branch` was captured before any mutation.
        let head_mirror = Self::head_reflog_mirror(head_branch.as_deref(), &reflogs);
        reflogs.extend(head_mirror);
        // All refs are durable; append reflogs last, matching git's ordering.
        self.append_loose_reflogs(reflogs)?;
        // git fires the `committed` hook once every ref change has landed. Its
        // exit status is ignored — the transaction has already succeeded.
        if let (Some(hook), Some(updates)) = (hook, hook_updates) {
            hook.run(RefTransactionPhase::Committed, updates)?;
        }
        Ok(())
    }

    fn read_locked_ref_state(
        &self,
        name: &str,
        packed_refs: &HashMap<String, RefTarget>,
    ) -> Result<LockedRefState> {
        let loose = self.read_loose_ref(name)?;
        let current = if let Some(reference) = loose.as_ref() {
            Some(reference.target.clone())
        } else {
            packed_refs.get(name).cloned()
        };
        Ok(LockedRefState {
            current,
            has_loose: loose.is_some(),
        })
    }
}

struct LockedRefState {
    current: Option<RefTarget>,
    has_loose: bool,
}

enum CoalescedRefChange {
    Update(CoalescedRefUpdate),
    Delete(CoalescedRefDelete),
}

impl CoalescedRefChange {
    fn name(&self) -> &str {
        match self {
            Self::Update(update) => &update.name,
            Self::Delete(delete) => &delete.name,
        }
    }
}

/// A ref update with all writes that targeted the same name folded together.
struct CoalescedRefUpdate {
    name: String,
    precondition: RefPrecondition,
    new: RefTarget,
    reflog: Vec<ReflogEntry>,
}

struct CoalescedRefDelete {
    name: String,
    precondition: RefDeletePrecondition,
}

fn coalesce_ref_changes(changes: Vec<QueuedRefChange>) -> Result<Vec<CoalescedRefChange>> {
    let has_delete = changes
        .iter()
        .any(|change| matches!(change, QueuedRefChange::Delete(_)));
    if !has_delete {
        let updates = changes
            .into_iter()
            .map(|change| match change {
                QueuedRefChange::Update(update) => update,
                QueuedRefChange::Delete(_) => unreachable!("has_delete was false"),
            })
            .collect::<Vec<_>>();
        return coalesce_ref_updates(updates).map(|updates| {
            updates
                .into_iter()
                .map(CoalescedRefChange::Update)
                .collect()
        });
    }

    let mut seen = BTreeSet::new();
    let mut coalesced = Vec::with_capacity(changes.len());
    for change in changes {
        let name = match &change {
            QueuedRefChange::Update(update) => &update.name,
            QueuedRefChange::Delete(delete) => &delete.name,
        };
        match &change {
            QueuedRefChange::Update(update) => validate_ref_name_for_update(&update.name)?,
            QueuedRefChange::Delete(delete) => validate_ref_name_for_read(&delete.name)?,
        }
        if !seen.insert(name.clone()) {
            return Err(GitError::Transaction(format!(
                "ref {name} appears more than once in transaction"
            )));
        }
        coalesced.push(match change {
            QueuedRefChange::Update(update) => CoalescedRefChange::Update(CoalescedRefUpdate {
                name: update.name,
                precondition: update.precondition,
                new: update.new,
                reflog: update.reflog.into_iter().collect(),
            }),
            QueuedRefChange::Delete(delete) => CoalescedRefChange::Delete(CoalescedRefDelete {
                name: delete.name,
                precondition: delete.precondition,
            }),
        });
    }
    Ok(coalesced)
}

/// Fold repeated updates to the same ref into one, preserving first-seen order.
/// The last queued value wins, reflog entries accumulate in order, and the
/// precondition is taken from the first update (the state the caller
/// asserted before any change in this transaction).
fn coalesce_ref_updates(updates: Vec<QueuedUpdate>) -> Result<Vec<CoalescedRefUpdate>> {
    let mut order: Vec<String> = Vec::new();
    let mut by_name: HashMap<String, CoalescedRefUpdate> = HashMap::new();
    for update in updates {
        validate_ref_name_for_update(&update.name)?;
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

/// A staged path change: the target path, its lock file, and original bytes for
/// rollback.
struct PendingPathChange {
    name: String,
    path: PathBuf,
    lock_path: PathBuf,
    original: Option<Vec<u8>>,
    action: PendingPathAction,
    staged: bool,
}

enum PendingPathAction {
    Write { contents: Vec<u8> },
    Delete,
    ReleaseLock,
}

struct RefDirPruneGuard<'a> {
    store: &'a FileRefStore,
    name: String,
}

impl Drop for RefDirPruneGuard<'_> {
    fn drop(&mut self) {
        self.store.prune_empty_ref_dirs(&self.name);
    }
}

struct DeleteLock {
    path: PathBuf,
    file: Option<fs::File>,
    active: bool,
}

impl DeleteLock {
    fn acquire(path: PathBuf) -> std::result::Result<Self, RefDeleteError> {
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => Ok(Self {
                path,
                file: Some(file),
                active: true,
            }),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                Err(RefDeleteError::Locked)
            }
            Err(err) => Err(RefDeleteError::Io(err)),
        }
    }

    fn write_all(
        &mut self,
        bytes: &[u8],
        fsync_method: Option<ReferenceFsyncMethod>,
    ) -> std::result::Result<(), RefDeleteError> {
        let Some(file) = self.file.as_mut() else {
            return Err(RefDeleteError::Io(std::io::Error::other(
                "lock file is already closed",
            )));
        };
        file.set_len(0)?;
        file.write_all(bytes)?;
        if let Some(method) = fsync_method {
            sync_reference_file(file, method)?;
        }
        Ok(())
    }

    fn close(mut self) -> PathBuf {
        self.active = false;
        let _ = self.file.take();
        self.path.clone()
    }

    fn remove(mut self) {
        self.active = false;
        let _ = self.file.take();
        let _ = fs::remove_file(&self.path);
    }
}

impl Drop for DeleteLock {
    fn drop(&mut self) {
        if self.active {
            let _ = self.file.take();
            let _ = fs::remove_file(&self.path);
        }
    }
}

struct ReftableListLock {
    list_path: PathBuf,
    lock_path: PathBuf,
    file: Option<fs::File>,
    active: bool,
}

impl ReftableListLock {
    fn acquire(list_path: PathBuf, lock_path: PathBuf, timeout_millis: u64) -> Result<Self> {
        let start = SystemTime::now();
        loop {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&lock_path)
            {
                Ok(file) => {
                    return Ok(Self {
                        list_path,
                        lock_path,
                        file: Some(file),
                        active: true,
                    });
                }
                Err(err)
                    if err.kind() == std::io::ErrorKind::AlreadyExists && timeout_millis > 0 =>
                {
                    let elapsed = start
                        .elapsed()
                        .unwrap_or_else(|_| Duration::from_millis(timeout_millis + 1));
                    if elapsed.as_millis() as u64 >= timeout_millis {
                        return Err(GitError::Io(format!(
                            "cannot lock references: {}: File exists",
                            lock_path.display()
                        )));
                    }
                    thread::sleep(Duration::from_millis(50));
                }
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                    return Err(GitError::Io(format!(
                        "cannot lock references: {}: File exists",
                        lock_path.display()
                    )));
                }
                Err(err) => return Err(err.into()),
            }
        }
    }

    fn commit(mut self, bytes: &[u8], fsync_method: Option<ReferenceFsyncMethod>) -> Result<()> {
        let Some(mut file) = self.file.take() else {
            return Err(GitError::Io("reftable list lock is already closed".into()));
        };
        file.set_len(0)?;
        file.write_all(bytes)?;
        if let Some(method) = fsync_method {
            sync_reference_file(&file, method)?;
        }
        drop(file);
        fs::rename(&self.lock_path, &self.list_path)
            .map_err(|err| GitError::Io(err.to_string()))?;
        self.active = false;
        Ok(())
    }
}

impl Drop for ReftableListLock {
    fn drop(&mut self) {
        if self.active {
            let _ = self.file.take();
            let _ = fs::remove_file(&self.lock_path);
        }
    }
}

fn checked_delete_oid(
    expected: Option<ObjectId>,
    current: Option<RefTarget>,
) -> std::result::Result<ObjectId, RefDeleteError> {
    let Some(current) = current else {
        return Err(RefDeleteError::NotFound);
    };
    let RefTarget::Direct(actual) = current else {
        return Err(RefDeleteError::ExpectedMismatch {
            expected,
            actual: None,
        });
    };
    if let Some(expected_oid) = expected
        && expected_oid != actual
    {
        return Err(RefDeleteError::ExpectedMismatch {
            expected: Some(expected_oid),
            actual: Some(actual),
        });
    }
    Ok(actual)
}

/// Verify a queued/checked delete may proceed, dying on a precondition
/// mismatch. Git unlinks the reflog on delete (it never writes a deletion
/// entry), so this validates only — the peeled OID is no longer plumbed out.
/// `peeled_oid_for_delete` is still invoked where the precondition requires the
/// peeled value, so a broken/unpeelable ref is still reported.
fn verify_delete_precondition(
    store: &FileRefStore,
    name: &str,
    current: Option<&RefTarget>,
    precondition: &RefDeletePrecondition,
) -> Result<()> {
    let Some(current) = current else {
        return Err(GitError::Transaction(format!("ref {name} not found")));
    };
    match precondition {
        RefDeletePrecondition::Any => {
            peeled_oid_for_delete(store, current)?;
            Ok(())
        }
        RefDeletePrecondition::Immediate(expected) if current == expected => {
            peeled_oid_for_delete(store, current)?;
            Ok(())
        }
        RefDeletePrecondition::Immediate(_) => Err(delete_precondition_mismatch(name)),
        RefDeletePrecondition::Direct(expected) => {
            let RefTarget::Direct(actual) = current else {
                return Err(delete_precondition_mismatch(name));
            };
            if let Some(expected) = expected
                && expected != actual
            {
                return Err(delete_precondition_mismatch(name));
            }
            Ok(())
        }
        RefDeletePrecondition::Peeled(expected) => {
            let actual = peeled_oid_for_delete(store, current)?;
            if actual == Some(*expected) {
                Ok(())
            } else {
                Err(delete_precondition_mismatch(name))
            }
        }
    }
}

fn peeled_oid_for_delete(store: &FileRefStore, target: &RefTarget) -> Result<Option<ObjectId>> {
    match target {
        RefTarget::Direct(oid) => Ok(Some(*oid)),
        RefTarget::Symbolic(name) => resolve_ref_peeled(store, name),
    }
}

fn delete_precondition_mismatch(name: &str) -> GitError {
    GitError::Transaction(format!("expected ref {name} to match"))
}

fn ref_delete_error_from_git(err: GitError) -> RefDeleteError {
    match err {
        GitError::InvalidPath(_) => RefDeleteError::InvalidName,
        GitError::NotFound(_) => RefDeleteError::NotFound,
        GitError::Io(message) if message.contains("File exists") => RefDeleteError::Locked,
        GitError::Io(message) if message.contains("could not lock") => RefDeleteError::Locked,
        GitError::Transaction(message) if message.contains("could not lock") => {
            RefDeleteError::Locked
        }
        other => RefDeleteError::Io(std::io::Error::other(other.to_string())),
    }
}

fn read_optional_file(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) if err.kind() == std::io::ErrorKind::IsADirectory => Ok(None),
        Err(err) => Err(GitError::Io(err.to_string())),
    }
}

/// Recursively remove an empty directory tree at `path` (git's
/// `remove_empty_directories`). A no-op when `path` is absent or is a file; an
/// error if any directory in the tree contains a non-directory entry.
fn remove_empty_dir_tree(path: &Path) -> std::io::Result<()> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    if !meta.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            remove_empty_dir_tree(&entry.path())?;
        } else {
            return Err(std::io::Error::other("directory not empty"));
        }
    }
    fs::remove_dir(path)
}

fn stage_lock_file(lock_path: &Path, contents: &[u8]) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(lock_path)?;
    file.write_all(contents)?;
    Ok(())
}

fn stage_pending_change(change: &mut PendingPathChange) -> Result<()> {
    if change.staged {
        return Ok(());
    }
    match &change.action {
        PendingPathAction::Write { contents } => stage_lock_file(&change.lock_path, contents)?,
        PendingPathAction::Delete => stage_lock_file(&change.lock_path, b"delete\n")?,
        PendingPathAction::ReleaseLock => {}
    }
    change.staged = true;
    Ok(())
}

fn apply_pending_change(change: &PendingPathChange) -> Result<()> {
    match &change.action {
        PendingPathAction::Write { .. } => {
            match fs::rename(&change.lock_path, &change.path) {
                Ok(()) => Ok(()),
                Err(first_err) => {
                    // Usually the destination does not exist, so avoid a stat
                    // for every update. On the rare empty-directory conflict,
                    // preserve git's remove_empty_directories behavior and
                    // retry the rename after clearing the tree.
                    match fs::symlink_metadata(&change.path) {
                        Ok(meta) if meta.is_dir() => {
                            remove_empty_dir_tree(&change.path)
                                .map_err(|err| GitError::Io(err.to_string()))?;
                            fs::rename(&change.lock_path, &change.path)
                                .map_err(|err| GitError::Io(err.to_string()))
                        }
                        _ => Err(GitError::Io(first_err.to_string())),
                    }
                }
            }
        }
        PendingPathAction::Delete => {
            if change.original.is_some() {
                fs::remove_file(&change.path).map_err(|err| GitError::Io(err.to_string()))?;
            }
            fs::remove_file(&change.lock_path).map_err(|err| GitError::Io(err.to_string()))
        }
        PendingPathAction::ReleaseLock => {
            fs::remove_file(&change.lock_path).map_err(|err| GitError::Io(err.to_string()))
        }
    }
}

/// Delete every still-held lock file. Used when a transaction aborts before any
/// path change, so nothing on disk has changed yet.
fn release_pending_locks(pending: &[PendingPathChange]) {
    for change in pending {
        let _ = fs::remove_file(&change.lock_path);
    }
}

/// Roll back after `applied` path changes have already landed: restore each to
/// its captured bytes (or remove it if it did not previously exist), then drop
/// the lock files that have not yet been applied.
fn rollback_after_apply(pending: &[PendingPathChange], applied: usize) {
    for change in pending.iter().take(applied) {
        if matches!(change.action, PendingPathAction::ReleaseLock) {
            let _ = fs::remove_file(&change.lock_path);
            continue;
        }
        match &change.original {
            Some(bytes) => {
                let _ = restore_file_atomically(&change.path, bytes);
            }
            None => {
                let _ = fs::remove_file(&change.path);
            }
        }
        let _ = fs::remove_file(&change.lock_path);
    }
    for change in pending.iter().skip(applied) {
        let _ = fs::remove_file(&change.lock_path);
    }
}

#[cfg(test)]
thread_local! {
    static FAIL_LOOSE_COMMIT_ACTION: std::cell::Cell<Option<usize>> =
        const { std::cell::Cell::new(None) };
    static REFERENCE_FSYNC_CALLS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static FAIL_REFERENCE_FSYNC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
fn set_fail_loose_commit_action_for_test(index: Option<usize>) {
    FAIL_LOOSE_COMMIT_ACTION.with(|cell| cell.set(index));
}

#[cfg(test)]
fn maybe_fail_loose_commit_action(index: usize) -> Result<()> {
    let should_fail = FAIL_LOOSE_COMMIT_ACTION.with(|cell| cell.get() == Some(index));
    if should_fail {
        FAIL_LOOSE_COMMIT_ACTION.with(|cell| cell.set(None));
        return Err(GitError::Io(format!(
            "injected loose ref transaction failure at action {index}"
        )));
    }
    Ok(())
}

#[cfg(not(test))]
fn maybe_fail_loose_commit_action(_index: usize) -> Result<()> {
    Ok(())
}

/// Best-effort atomic restore of `path` to `bytes` during rollback, reusing the
/// write-to-temp-then-rename dance so a crash mid-rollback cannot truncate a ref.
fn restore_file_atomically(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    write_locked(path, bytes, None)
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
        // Mirror git's check_tag_ref(): reject a leading '-' or the literal
        // "HEAD", then validate refs/tags/<tag> with check_refname_format().
        if tag.starts_with('-') || tag == "HEAD" {
            return Err(GitError::InvalidPath(format!("invalid tag name {tag}")));
        }
        Self::from_tag_name_unrestricted(tag)
    }

    /// Build `refs/tags/<tag>` validating only the refname format, without the
    /// creation-only restrictions (leading `-`, literal `HEAD`). Git's delete
    /// path does not run check_tag_ref(), so a tag literally named `HEAD` can
    /// still be removed.
    pub fn from_tag_name_unrestricted(tag: &str) -> Result<Self> {
        let name = format!("{}{}", TagRefName::PREFIX, tag);
        check_refname_format(&name, false)?;
        Ok(Self { name })
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

pub fn branch_ref_name_for_read(branch: &str) -> Result<String> {
    let name = format!("{}{}", BranchRefName::PREFIX, branch);
    if validate_ref_name(&name).is_err() {
        if name.contains("..") {
            validate_ref_path_safe_for_read(&name)?;
        } else {
            return Err(GitError::InvalidPath(format!("invalid ref name {name}")));
        }
    }
    Ok(name)
}

pub fn branch_ref_name_for_source(branch: &str) -> Result<String> {
    if branch.starts_with("--") {
        return Err(GitError::InvalidPath(format!(
            "invalid branch name {branch}"
        )));
    }
    let name = format!("{}{}", BranchRefName::PREFIX, branch);
    check_refname_format(&name, false)?;
    Ok(name)
}

pub fn tag_ref_name(tag: &str) -> Result<String> {
    TagRefNameBuf::from_tag_name(tag).map(TagRefNameBuf::into_string)
}

fn sync_reference_file(file: &fs::File, method: ReferenceFsyncMethod) -> std::io::Result<()> {
    #[cfg(test)]
    {
        REFERENCE_FSYNC_CALLS.with(|calls| calls.set(calls.get() + 1));
        let fail = FAIL_REFERENCE_FSYNC.with(|fail| fail.replace(false));
        if fail {
            return Err(std::io::Error::other("injected reference fsync failure"));
        }
    }
    match method {
        ReferenceFsyncMethod::WriteoutOnly => file.sync_data(),
        ReferenceFsyncMethod::Fsync | ReferenceFsyncMethod::Batch => file.sync_all(),
    }
}

fn write_locked(
    path: &Path,
    bytes: &[u8],
    fsync_method: Option<ReferenceFsyncMethod>,
) -> Result<()> {
    write_locked_with_timeout(path, bytes, 0, fsync_method)
}

fn write_locked_with_timeout(
    path: &Path,
    bytes: &[u8],
    timeout_millis: u64,
    fsync_method: Option<ReferenceFsyncMethod>,
) -> Result<()> {
    let lock_path = lock_path_for(path)?;
    let start = SystemTime::now();
    loop {
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
        {
            Ok(mut file) => {
                let write_result = file.write_all(bytes).and_then(|()| match fsync_method {
                    Some(method) => sync_reference_file(&file, method),
                    None => Ok(()),
                });
                if let Err(err) = write_result {
                    drop(file);
                    let _ = fs::remove_file(&lock_path);
                    return Err(err.into());
                }
                break;
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists && timeout_millis > 0 => {
                let elapsed = start
                    .elapsed()
                    .unwrap_or_else(|_| Duration::from_millis(timeout_millis + 1));
                if elapsed.as_millis() as u64 >= timeout_millis {
                    return Err(GitError::Io(format!(
                        "could not lock {}: File exists",
                        path.display()
                    )));
                }
                thread::sleep(Duration::from_millis(50));
            }
            Err(err) => return Err(err.into()),
        }
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

/// Validate a ref name using git's `check_refname_format` rules.
pub fn check_refname_format(name: &str, allow_onelevel: bool) -> Result<()> {
    if name.is_empty()
        || name == "@"
        || name.starts_with('/')
        || name.ends_with('/')
        || name.ends_with('.')
        || name.contains("..")
        || name.contains("//")
        || name.contains("@{")
        || (!allow_onelevel && !name.contains('/'))
    {
        return Err(GitError::InvalidPath(format!("invalid ref name {name}")));
    }
    for component in name.split('/') {
        if component.is_empty() || component.starts_with('.') || component.ends_with(".lock") {
            return Err(GitError::InvalidPath(format!("invalid ref name {name}")));
        }
        for (idx, byte) in component.bytes().enumerate() {
            if byte <= b' '
                || byte == 0x7f
                || matches!(byte, b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\')
            {
                return Err(GitError::InvalidPath(format!("invalid ref name {name}")));
            }
            if byte == b'.' && component.as_bytes().get(idx + 1) == Some(&b'.') {
                return Err(GitError::InvalidPath(format!("invalid ref name {name}")));
            }
            if byte == b'@' && component.as_bytes().get(idx + 1) == Some(&b'{') {
                return Err(GitError::InvalidPath(format!("invalid ref name {name}")));
            }
        }
    }
    Ok(())
}

/// Validate a symbolic ref name (HEAD, one-level pseudo-refs, or `refs/...`).
pub fn validate_symref_name(name: &str) -> Result<()> {
    if name == "HEAD" {
        return Ok(());
    }
    check_refname_format(name, true)
}

/// Validate a symbolic ref target (one-level pseudo-refs or `refs/...`).
pub fn validate_symref_target(name: &str) -> Result<()> {
    check_refname_format(name, true)
}

/// Follow symbolic ref chains until a direct OID is reached.
/// Remove empty directories starting at `start` and walking up toward
/// `boundary`, stopping at the first non-empty directory or when `boundary` is
/// reached (exclusive). `boundary` itself is never removed.
fn prune_empty_dirs_up_to(start: &Path, boundary: &Path) {
    let mut dir = start.to_path_buf();
    while dir.starts_with(boundary) && dir != *boundary {
        if fs::remove_dir(&dir).is_err() {
            break;
        }
        dir = match dir.parent() {
            Some(parent) => parent.to_path_buf(),
            None => break,
        };
    }
}

fn packable_loose_ref_name(name: &str) -> bool {
    name.starts_with("refs/")
        && !name.starts_with("refs/bisect/")
        && !name.starts_with("refs/worktree/")
        && !name.starts_with("refs/rewritten/")
}

fn pack_refs_auto_required_for(packed_path: &Path, loose_count: usize) -> Result<bool> {
    let packed_size = match fs::metadata(packed_path) {
        Ok(meta) => meta.len() as usize,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => 0,
        Err(err) => return Err(err.into()),
    };
    let estimated_packed_refs = packed_size / 100;
    let log2 = if estimated_packed_refs == 0 {
        0
    } else {
        usize::BITS as usize - estimated_packed_refs.leading_zeros() as usize - 1
    };
    let limit = (log2 * 5).max(16);
    Ok(loose_count >= limit)
}

pub fn resolve_ref_peeled(store: &FileRefStore, name: &str) -> Result<Option<ObjectId>> {
    let mut current = name.to_string();
    for _ in 0..16 {
        match store.read_ref(&current)? {
            Some(RefTarget::Direct(oid)) => return Ok(Some(oid)),
            Some(RefTarget::Symbolic(next)) => {
                if validate_ref_name_for_read(&next).is_err() {
                    return Ok(None);
                }
                current = next;
            }
            None => return Ok(None),
        }
    }
    Ok(None)
}

pub fn validate_ref_name_for_read(name: &str) -> Result<()> {
    if validate_ref_name(name).is_ok() {
        return Ok(());
    }
    if is_root_ref_syntax(name) {
        return Ok(());
    }
    if check_refname_format(name, true).is_ok() {
        return Ok(());
    }
    validate_ref_path_safe_for_read(name)
}

pub fn validate_ref_name_for_update(name: &str) -> Result<()> {
    if validate_ref_name(name).is_ok() {
        return Ok(());
    }
    if is_root_ref_syntax(name) {
        return Ok(());
    }
    check_refname_format(name, true)
}

/// git's `refname_is_safe` (refs.c): the gate applied when *deleting* a ref
/// (`transaction_refname_valid` with a null new-oid). It is stricter than the
/// create-time `check_refname_format(_, REFNAME_ALLOW_ONELEVEL)`:
///   - a name under `refs/` is safe when the remainder is non-empty, has no
///     leading/trailing `/`, and does not escape `refs/` (`..`, absolute,
///     backslash component);
///   - any other (one-level) name is safe only when every byte is an uppercase
///     ASCII letter or `_` — the pseudo-ref shape (`HEAD`, `ORIG_HEAD`).
///
/// So a one-level name like `my-private-file` is *creatable* (`update-ref
/// my-private-file <oid>`) yet refused for deletion (`update-ref -d
/// my-private-file` → "refusing to update ref with bad name"), which is what
/// keeps `update-ref -d` from unlinking arbitrary files inside `.git`.
pub fn refname_is_safe(refname: &str) -> bool {
    if let Some(rest) = refname.strip_prefix("refs/") {
        if rest.is_empty() || rest.starts_with('/') || rest.ends_with('/') || rest.contains('\\') {
            return false;
        }
        let path = Path::new(rest);
        !path.is_absolute()
            && !path.components().any(|component| {
                matches!(
                    component,
                    std::path::Component::ParentDir
                        | std::path::Component::Prefix(_)
                        | std::path::Component::RootDir
                )
            })
    } else {
        !refname.is_empty() && refname.bytes().all(|b| b.is_ascii_uppercase() || b == b'_')
    }
}

/// git's is_root_ref_syntax (refs.c): a ref name made only of uppercase ASCII,
/// `-`, and `_` (e.g. HEAD, FETCH_HEAD, MERGE_HEAD). Such names live in the
/// per-worktree gitdir rather than the common refs/ tree. An empty name is not
/// root-ref syntax.
fn is_root_ref_syntax(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b == b'-' || b == b'_')
}

const CURRENT_WORKTREE_REF_PREFIXES: &[&str] =
    &["refs/bisect/", "refs/worktree/", "refs/rewritten/"];

fn current_worktree_ref(name: &str) -> bool {
    is_root_ref_syntax(name)
        || CURRENT_WORKTREE_REF_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix))
}

fn ref_prefixes_overlap(prefix: &str, namespace: &str) -> bool {
    let namespace = namespace.trim_end_matches('/');
    prefix == "refs/"
        || prefix == namespace
        || prefix.starts_with(&format!("{namespace}/"))
        || namespace.starts_with(prefix.trim_end_matches('/'))
}

fn reftable_current_worktree_ref(name: &str) -> bool {
    current_worktree_ref(name)
}

fn reftable_other_worktree_ref(name: &str) -> Option<(&str, &str)> {
    let rest = name.strip_prefix("worktrees/")?;
    let (worktree, rewritten) = rest.split_once('/')?;
    if worktree.is_empty() || rewritten.is_empty() {
        return None;
    }
    Some((worktree, rewritten))
}

pub fn validate_ref_name(name: &str) -> Result<()> {
    if name == "HEAD" {
        return Ok(());
    }
    if !name.starts_with("refs/") || check_refname_format(name, false).is_err() {
        return Err(GitError::InvalidPath(format!("invalid ref name {name}")));
    }
    Ok(())
}

fn validate_ref_path_safe_for_read(name: &str) -> Result<()> {
    let path = Path::new(name);
    if !name.starts_with("refs/")
        || name.starts_with('/')
        || name.contains('\\')
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

fn safe_ref_prefix_for_directory_scan(prefix: &str) -> bool {
    let path = Path::new(prefix);
    prefix.starts_with("refs/")
        && !prefix.starts_with('/')
        && !prefix.contains('\\')
        && !path.is_absolute()
        && !path.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir | std::path::Component::Prefix(_)
            )
        })
}

fn warn_broken_ref_name(name: &str) {
    eprintln!("warning: ignoring ref with broken name {name}");
}

fn warn_broken_ref(name: &str) {
    eprintln!("warning: ignoring broken ref {name}");
}

/// A direct ref resolving to the null OID is broken (git's `REF_ISBROKEN`).
fn ref_target_is_broken(target: &RefTarget) -> bool {
    matches!(target, RefTarget::Direct(oid) if oid.is_null())
}

fn ref_directory_conflict_error(new_ref: &str, existing_ref: &str) -> GitError {
    GitError::Transaction(format!(
        "cannot lock ref '{new_ref}': '{existing_ref}' exists; cannot create '{new_ref}'"
    ))
}

fn check_ref_directory_conflict_in_names(name: &str, names: &BTreeSet<String>) -> Result<()> {
    let components = name.split('/').collect::<Vec<_>>();
    for index in 1..components.len() {
        let ancestor = components[..index].join("/");
        if names.contains(&ancestor) {
            return Err(ref_directory_conflict_error(name, &ancestor));
        }
    }
    let child_prefix = format!("{name}/");
    if let Some(existing) = names.range(child_prefix.clone()..).next()
        && existing.starts_with(&child_prefix)
    {
        return Err(ref_directory_conflict_error(name, existing));
    }
    Ok(())
}

fn parent_to_ref_name(base: &Path, parent: &Path) -> String {
    match parent.strip_prefix(base) {
        Ok(suffix) => suffix.to_string_lossy().replace('\\', "/"),
        Err(_) => parent.to_string_lossy().into_owned(),
    }
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
    fn core_fsync_reference_component_matches_git_groups_and_negation() {
        assert!(!core_fsync_includes_reference("none"));
        assert!(!core_fsync_includes_reference("objects,index"));
        assert!(!core_fsync_includes_reference("-reference"));
        assert!(core_fsync_includes_reference("reference"));
        assert!(core_fsync_includes_reference("ref"));
        assert!(core_fsync_includes_reference("committed"));
        assert!(core_fsync_includes_reference("added"));
        assert!(core_fsync_includes_reference("all"));
        // Git accumulates positive and negative masks, then applies positives
        // last, so an explicitly included component wins in either order.
        assert!(core_fsync_includes_reference("reference,-reference"));
        assert!(core_fsync_includes_reference("-reference,reference"));
    }

    #[test]
    fn reference_fsync_policy_controls_reference_barriers() {
        let disabled_dir = temp_git_dir();
        REFERENCE_FSYNC_CALLS.with(|calls| calls.set(0));
        FileRefStore::new(&disabled_dir, ObjectFormat::Sha1)
            .with_reference_fsync_config(Some("none"), Some("fsync"))
            .write_packed_refs(&[])
            .expect("disabled reference write");
        assert_eq!(REFERENCE_FSYNC_CALLS.with(std::cell::Cell::get), 0);
        fs::remove_dir_all(disabled_dir).expect("remove disabled repository");

        let enabled_dir = temp_git_dir();
        REFERENCE_FSYNC_CALLS.with(|calls| calls.set(0));
        FileRefStore::new(&enabled_dir, ObjectFormat::Sha1)
            .with_reference_fsync_config(Some("reference"), Some("fsync"))
            .write_packed_refs(&[])
            .expect("enabled reference write");
        assert_eq!(REFERENCE_FSYNC_CALLS.with(std::cell::Cell::get), 1);
        fs::remove_dir_all(enabled_dir).expect("remove enabled repository");
    }

    #[test]
    fn reference_fsync_failure_aborts_locked_write_and_cleans_lock() {
        let git_dir = temp_git_dir();
        REFERENCE_FSYNC_CALLS.with(|calls| calls.set(0));
        FAIL_REFERENCE_FSYNC.with(|fail| fail.set(true));
        let err = FileRefStore::new(&git_dir, ObjectFormat::Sha1)
            .with_reference_fsync_config(Some("reference"), Some("fsync"))
            .write_packed_refs(&[])
            .expect_err("injected fsync failure must abort");
        assert!(err.to_string().contains("injected reference fsync failure"));
        assert!(!git_dir.join("packed-refs").exists());
        assert!(!git_dir.join("packed-refs.lock").exists());
        assert_eq!(REFERENCE_FSYNC_CALLS.with(std::cell::Cell::get), 1);
        fs::remove_dir_all(git_dir).expect("remove failed repository");
    }

    #[test]
    fn loose_ref_round_trips_direct() {
        let oid = "ce013625030ba8dba906f756967f9e9ca394464a";
        let reference = parse_loose_ref(ObjectFormat::Sha1, "refs/heads/main", oid.as_bytes())
            .expect("test operation should succeed");
        assert_eq!(write_loose_ref(&reference), format!("{oid}\n").into_bytes());
    }

    #[test]
    fn loose_fetch_head_reads_first_object_id() {
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .expect("test operation should succeed");
        let bytes = b"ce013625030ba8dba906f756967f9e9ca394464a\t\tbranch 'main' of ../sub\n";
        let reference = parse_loose_ref(ObjectFormat::Sha1, "FETCH_HEAD", bytes)
            .expect("test operation should succeed");
        assert_eq!(reference.target, RefTarget::Direct(oid));
    }

    #[test]
    fn symref_names_allow_onelevel_pseudo_refs() {
        for name in ["NOTHEAD", "FOO", "ORIG_HEAD", "TEST_SYMREF"] {
            validate_symref_name(name).expect("symref name should be valid");
        }
        assert!(validate_ref_name("NOTHEAD").is_err());
        assert!(validate_symref_target("refs/heads/foo").is_ok());
        assert!(validate_symref_target("ORIG_HEAD").is_ok());
        assert!(validate_symref_target("foo..bar").is_err());
    }

    #[test]
    fn resolve_ref_peeled_follows_symref_chains() {
        let git_dir = temp_git_dir();
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .expect("test operation should succeed");
        let mut tx = store.transaction();
        tx.update(RefUpdate {
            name: "refs/heads/target".into(),
            expected: None,
            new: RefTarget::Direct(oid),
            reflog: None,
        });
        tx.commit().expect("seed target ref");
        let mut tx = store.transaction();
        tx.update(RefUpdate {
            name: "refs/heads/alias".into(),
            expected: None,
            new: RefTarget::Symbolic("refs/heads/target".into()),
            reflog: None,
        });
        tx.commit().expect("seed alias ref");
        let mut tx = store.transaction();
        tx.update(RefUpdate {
            name: "ORIG_HEAD".into(),
            expected: None,
            new: RefTarget::Symbolic("refs/heads/alias".into()),
            reflog: None,
        });
        tx.commit().expect("seed ORIG_HEAD symref");
        assert_eq!(
            resolve_ref_peeled(&store, "ORIG_HEAD").expect("resolve ORIG_HEAD"),
            Some(oid)
        );
        let _ = fs::remove_dir_all(git_dir);
    }

    #[test]
    fn symref_directory_conflict_is_reported_gracefully() {
        let git_dir = temp_git_dir();
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .expect("test operation should succeed");
        let mut tx = store.transaction();
        tx.update(RefUpdate {
            name: "refs/heads/df".into(),
            expected: None,
            new: RefTarget::Direct(oid),
            reflog: None,
        });
        tx.commit().expect("seed branch ref");

        let mut tx = store.transaction();
        tx.update(RefUpdate {
            name: "refs/heads/df/conflict".into(),
            expected: None,
            new: RefTarget::Symbolic("refs/heads/df".into()),
            reflog: None,
        });
        let err = tx.commit().expect_err("child ref should conflict");
        assert!(
            matches!(err, GitError::Transaction(message) if message.contains(
            "cannot lock ref 'refs/heads/df/conflict'"
        ) && message.contains("refs/heads/df"))
        );
        let _ = fs::remove_dir_all(git_dir);
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
            new: RefTarget::Direct(oid),
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
                    target: RefTarget::Direct(tag_oid),
                },
                peeled: Some(peeled_oid),
            },
            PackedRef {
                reference: Ref {
                    name: "refs/heads/main".into(),
                    target: RefTarget::Direct(head_oid),
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
            new: RefTarget::Direct(oid),
            reflog: Some(ReflogEntry {
                old_oid: zero_oid(ObjectFormat::Sha1).expect("test operation should succeed"),
                new_oid: oid,
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
                        oid: tag_oid,
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
                    new_oid: tag_oid,
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
            Some(RefTarget::Direct(tag_oid))
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
                    oid,
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
                oid,
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
                oid,
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
            new: RefTarget::Direct(oid),
            reflog: Some(ReflogEntry {
                old_oid: zero_oid(ObjectFormat::Sha1).expect("test operation should succeed"),
                new_oid: oid,
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
    fn file_ref_store_delete_prunes_below_namespace_only() {
        let git_dir = temp_git_dir();
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .expect("test operation should succeed");
        for name in ["refs/heads/l/m", "refs/foo/bar"] {
            let mut tx = store.transaction();
            tx.update(RefUpdate {
                name: name.into(),
                expected: None,
                new: RefTarget::Direct(oid),
                reflog: None,
            });
            tx.commit().expect("test operation should succeed");
            store
                .delete_ref(name)
                .expect("test operation should succeed");
        }
        assert!(!git_dir.join("refs").join("heads").join("l").exists());
        assert!(git_dir.join("refs").join("heads").exists());
        assert!(!git_dir.join("refs").join("foo").join("bar").exists());
        assert!(git_dir.join("refs").join("foo").exists());
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn file_ref_store_delete_ref_checked_removes_reflog() {
        let git_dir = temp_git_dir();
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .expect("test operation should succeed");
        // Create the ref *with* a reflog entry so logs/refs/heads/main exists on
        // disk; git unlinks that file on delete rather than appending a deletion
        // entry, so the checked delete must remove it (mirroring delete_ref).
        let mut tx = store.transaction();
        tx.update(RefUpdate {
            name: "refs/heads/main".into(),
            expected: None,
            new: RefTarget::Direct(oid),
            reflog: Some(ReflogEntry {
                old_oid: zero_oid(ObjectFormat::Sha1).expect("test operation should succeed"),
                new_oid: oid,
                committer: b"Git Rs <sley@example.invalid> 0 +0000".to_vec(),
                message: b"create main".to_vec(),
            }),
        });
        tx.commit().expect("test operation should succeed");
        assert!(
            git_dir
                .join("logs")
                .join("refs")
                .join("heads")
                .join("main")
                .exists(),
            "reflog file should exist before the checked delete"
        );

        let deleted = store
            .delete_ref_checked(DeleteRef {
                name: "refs/heads/main".into(),
                expected_old: Some(oid),
                reflog: Some(DeleteRefReflog {
                    committer: b"Git Rs <sley@example.invalid> 123 +0000".to_vec(),
                    message: b"delete main".to_vec(),
                }),
            })
            .expect("test operation should succeed");

        assert_eq!(deleted.name, "refs/heads/main");
        assert_eq!(deleted.oid, oid);
        assert_eq!(
            store
                .read_ref("refs/heads/main")
                .expect("test operation should succeed"),
            None
        );
        // Git unlinks the reflog on delete: the file is gone and there is no
        // lingering deletion entry to read back.
        assert!(
            !git_dir
                .join("logs")
                .join("refs")
                .join("heads")
                .join("main")
                .exists(),
            "reflog file should be removed by the checked delete"
        );
        assert!(
            store
                .read_reflog("refs/heads/main")
                .expect("test operation should succeed")
                .is_empty()
        );
        assert!(
            !git_dir
                .join("refs")
                .join("heads")
                .join("main.lock")
                .exists()
        );
        assert!(!git_dir.join("packed-refs.lock").exists());
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn file_ref_store_delete_ref_checked_stale_expected_leaves_ref_untouched() {
        let git_dir = temp_git_dir();
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let actual = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .expect("test operation should succeed");
        let expected = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391",
        )
        .expect("test operation should succeed");
        let mut tx = store.transaction();
        tx.update(RefUpdate {
            name: "refs/heads/main".into(),
            expected: None,
            new: RefTarget::Direct(actual),
            reflog: None,
        });
        tx.commit().expect("test operation should succeed");

        let err = store
            .delete_ref_checked(DeleteRef {
                name: "refs/heads/main".into(),
                expected_old: Some(expected),
                reflog: None,
            })
            .expect_err("stale expected must fail");

        assert!(matches!(
            err,
            RefDeleteError::ExpectedMismatch {
                expected: Some(got_expected),
                actual: Some(got_actual),
            } if got_expected == expected && got_actual == actual
        ));
        assert_eq!(
            store
                .read_ref("refs/heads/main")
                .expect("test operation should succeed"),
            Some(RefTarget::Direct(actual))
        );
        assert!(
            !git_dir
                .join("refs")
                .join("heads")
                .join("main.lock")
                .exists()
        );
        assert!(!git_dir.join("packed-refs.lock").exists());
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn file_ref_store_delete_ref_checked_missing_returns_not_found() {
        let git_dir = temp_git_dir();
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);

        let err = store
            .delete_ref_checked(DeleteRef {
                name: "refs/heads/missing".into(),
                expected_old: None,
                reflog: None,
            })
            .expect_err("missing ref must fail");

        assert!(matches!(err, RefDeleteError::NotFound));
        assert!(
            !git_dir
                .join("refs")
                .join("heads")
                .join("missing.lock")
                .exists()
        );
        assert!(!git_dir.join("packed-refs.lock").exists());
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn file_ref_store_delete_ref_checked_removes_packed_ref() {
        let git_dir = temp_git_dir();
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .expect("test operation should succeed");
        let other = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391",
        )
        .expect("test operation should succeed");
        store
            .write_packed_refs(&[
                PackedRef {
                    reference: Ref {
                        name: "refs/heads/main".into(),
                        target: RefTarget::Direct(oid),
                    },
                    peeled: None,
                },
                PackedRef {
                    reference: Ref {
                        name: "refs/heads/other".into(),
                        target: RefTarget::Direct(other),
                    },
                    peeled: None,
                },
            ])
            .expect("test operation should succeed");

        store
            .delete_ref_checked(DeleteRef {
                name: "refs/heads/main".into(),
                expected_old: Some(oid),
                reflog: None,
            })
            .expect("test operation should succeed");

        assert_eq!(
            store
                .read_ref("refs/heads/main")
                .expect("test operation should succeed"),
            None
        );
        assert_eq!(
            store
                .read_ref("refs/heads/other")
                .expect("test operation should succeed"),
            Some(RefTarget::Direct(other))
        );
        let packed =
            fs::read_to_string(git_dir.join("packed-refs")).expect("test operation should succeed");
        assert!(!packed.contains("refs/heads/main"));
        assert!(packed.contains("refs/heads/other"));
        assert!(
            !git_dir
                .join("refs")
                .join("heads")
                .join("main.lock")
                .exists()
        );
        assert!(!git_dir.join("packed-refs.lock").exists());
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn file_ref_store_delete_ref_checked_lock_conflict_returns_locked() {
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
            new: RefTarget::Direct(oid),
            reflog: None,
        });
        tx.commit().expect("test operation should succeed");
        fs::write(
            git_dir.join("refs").join("heads").join("main.lock"),
            b"held\n",
        )
        .expect("test operation should succeed");

        let err = store
            .delete_ref_checked(DeleteRef {
                name: "refs/heads/main".into(),
                expected_old: Some(oid),
                reflog: None,
            })
            .expect_err("held lock must fail");

        assert!(matches!(err, RefDeleteError::Locked));
        assert_eq!(
            store
                .read_ref("refs/heads/main")
                .expect("test operation should succeed"),
            Some(RefTarget::Direct(oid))
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
            .create_tag("v1.0", oid)
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
            .create_tag("v1.0", oid)
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
            new: RefTarget::Direct(oid),
            reflog: None,
        });
        tx.commit().expect("test operation should succeed");
        let refs = store.list_refs().expect("test operation should succeed");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].target, RefTarget::Direct(oid));
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn file_ref_store_lists_refs_with_prefix_and_preserves_loose_shadowing() {
        let git_dir = temp_git_dir();
        fs::write(
            git_dir.join("packed-refs"),
            b"e69de29bb2d1d6434b8b29ae775ad8c2e48c5391 refs/heads/main\n\
              18f002b4484b838b205a48b1e9e6763ba5e3a607 refs/heads/topic\n\
              ce013625030ba8dba906f756967f9e9ca394464a refs/tags/v1.0\n",
        )
        .expect("test operation should succeed");
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let loose_main = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .expect("test operation should succeed");
        let packed_topic = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "18f002b4484b838b205a48b1e9e6763ba5e3a607",
        )
        .expect("test operation should succeed");
        let mut tx = store.transaction();
        tx.update(RefUpdate {
            name: "refs/heads/main".into(),
            expected: None,
            new: RefTarget::Direct(loose_main),
            reflog: None,
        });
        tx.commit().expect("test operation should succeed");

        assert_eq!(
            store
                .list_refs_with_prefix("refs/heads/")
                .expect("test operation should succeed"),
            vec![
                Ref {
                    name: "refs/heads/main".into(),
                    target: RefTarget::Direct(loose_main),
                },
                Ref {
                    name: "refs/heads/topic".into(),
                    target: RefTarget::Direct(packed_topic),
                },
            ]
        );
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
                    target: RefTarget::Direct(oid),
                },
                peeled: None,
            }])
            .expect("test operation should succeed");
        assert_eq!(
            store
                .read_ref("refs/heads/main")
                .expect("test operation should succeed"),
            Some(RefTarget::Direct(oid))
        );
        let refs = store.list_refs().expect("test operation should succeed");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].target, RefTarget::Direct(oid));
        assert!(git_dir.join("packed-refs").exists());
        assert!(!git_dir.join("packed-refs.lock").exists());
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn file_ref_store_checks_ref_prefix_in_packed_refs() {
        let git_dir = temp_git_dir();
        fs::write(
            git_dir.join("packed-refs"),
            b"e69de29bb2d1d6434b8b29ae775ad8c2e48c5391 refs/heads/main\n\
              ce013625030ba8dba906f756967f9e9ca394464a refs/replace/e69de29bb2d1d6434b8b29ae775ad8c2e48c5391\n",
        )
        .expect("test operation should succeed");
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        assert!(
            store
                .has_refs_with_prefix("refs/replace/")
                .expect("test operation should succeed")
        );
        assert!(
            !store
                .has_refs_with_prefix("refs/notes/")
                .expect("test operation should succeed")
        );
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn file_ref_store_checks_ref_prefix_in_loose_refs() {
        let git_dir = temp_git_dir();
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .expect("test operation should succeed");
        let mut tx = store.transaction();
        tx.update(RefUpdate {
            name: "refs/replace/e69de29bb2d1d6434b8b29ae775ad8c2e48c5391".into(),
            expected: None,
            new: RefTarget::Direct(oid),
            reflog: None,
        });
        tx.commit().expect("test operation should succeed");
        assert!(
            store
                .has_refs_with_prefix("refs/replace/")
                .expect("test operation should succeed")
        );
        assert!(
            !store
                .has_refs_with_prefix("refs/notes/")
                .expect("test operation should succeed")
        );
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
                "0x000000000001-0x000000000001-00000000.ref",
                vec![
                    sley_formats::ReftableRefRecord {
                        name: "HEAD".into(),
                        update_index: 1,
                        value: ReftableRefValue::Symbolic("refs/heads/main".into()),
                    },
                    sley_formats::ReftableRefRecord {
                        name: "refs/heads/main".into(),
                        update_index: 1,
                        value: ReftableRefValue::Direct(head_oid),
                    },
                    sley_formats::ReftableRefRecord {
                        name: "refs/tags/v1.0".into(),
                        update_index: 1,
                        value: ReftableRefValue::Peeled {
                            target: tag_oid,
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
            Some(RefTarget::Direct(head_oid))
        );
        assert_eq!(
            store
                .read_ref("refs/tags/v1.0")
                .expect("test operation should succeed"),
            Some(RefTarget::Direct(tag_oid))
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
        assert_eq!(
            store
                .list_refs_with_prefix("refs/tags/")
                .expect("test operation should succeed"),
            vec![Ref {
                name: "refs/tags/v1.0".into(),
                target: RefTarget::Direct(tag_oid),
            }]
        );

        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn root_ref_listing_is_backend_independent() {
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .expect("test operation should succeed");
        let expected = vec![
            Ref {
                name: "AUTO_MERGE".into(),
                target: RefTarget::Direct(oid),
            },
            Ref {
                name: "FOO_HEAD".into(),
                target: RefTarget::Direct(oid),
            },
            Ref {
                name: "HEAD".into(),
                target: RefTarget::Symbolic("refs/heads/main".into()),
            },
        ];

        let files_dir = temp_git_dir();
        fs::write(files_dir.join("AUTO_MERGE"), format!("{oid}\n")).expect("write files root ref");
        fs::write(files_dir.join("FOO_HEAD"), format!("{oid}\n")).expect("write files root ref");
        fs::write(files_dir.join("HEAD"), b"ref: refs/heads/main\n").expect("write files root ref");
        fs::write(files_dir.join("FETCH_HEAD"), format!("{oid}\n"))
            .expect("write working-state file");
        fs::write(files_dir.join("MERGE_HEAD"), format!("{oid}\n"))
            .expect("write working-state file");
        let files = FileRefStore::new(&files_dir, ObjectFormat::Sha1)
            .list_root_refs()
            .expect("list files root refs");

        let reftable_dir = temp_git_dir();
        write_reftable_config(&reftable_dir);
        write_reftable_stack(
            &reftable_dir,
            &[(
                "0x000000000001-0x000000000001-00000000.ref",
                vec![
                    ReftableRefRecord {
                        name: "AUTO_MERGE".into(),
                        update_index: 1,
                        value: ReftableRefValue::Direct(oid),
                    },
                    ReftableRefRecord {
                        name: "FETCH_HEAD".into(),
                        update_index: 1,
                        value: ReftableRefValue::Direct(oid),
                    },
                    ReftableRefRecord {
                        name: "FOO_HEAD".into(),
                        update_index: 1,
                        value: ReftableRefValue::Direct(oid),
                    },
                    ReftableRefRecord {
                        name: "HEAD".into(),
                        update_index: 1,
                        value: ReftableRefValue::Symbolic("refs/heads/main".into()),
                    },
                    ReftableRefRecord {
                        name: "MERGE_HEAD".into(),
                        update_index: 1,
                        value: ReftableRefValue::Direct(oid),
                    },
                    ReftableRefRecord {
                        name: "refs/heads/main".into(),
                        update_index: 1,
                        value: ReftableRefValue::Direct(oid),
                    },
                ],
            )],
        );
        let reftable = FileRefStore::new(&reftable_dir, ObjectFormat::Sha1)
            .list_root_refs()
            .expect("list reftable root refs");

        assert_eq!(files, expected);
        assert_eq!(reftable, expected);
        fs::remove_dir_all(files_dir).expect("remove test files repo");
        fs::remove_dir_all(reftable_dir).expect("remove test reftable repo");
    }

    #[test]
    fn file_ref_store_reads_loose_fetch_head_in_reftable_repo() {
        let git_dir = temp_git_dir();
        write_reftable_config(&git_dir);
        fs::write(git_dir.join("HEAD"), b"ref: refs/heads/.invalid\n")
            .expect("test operation should succeed");
        fs::create_dir_all(git_dir.join("reftable")).expect("test operation should succeed");
        fs::write(git_dir.join("reftable").join("tables.list"), b"")
            .expect("test operation should succeed");
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .expect("test operation should succeed");
        fs::write(
            git_dir.join("FETCH_HEAD"),
            b"ce013625030ba8dba906f756967f9e9ca394464a\t\tbranch 'main' of ../sub\n",
        )
        .expect("test operation should succeed");

        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        assert_eq!(
            store
                .read_ref("FETCH_HEAD")
                .expect("test operation should succeed"),
            Some(RefTarget::Direct(oid))
        );
        assert!(
            store
                .raw_ref_exists("FETCH_HEAD")
                .expect("test operation should succeed")
        );

        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn file_ref_store_empty_reftable_reflog_rewrite_keeps_marker() {
        let git_dir = temp_git_dir();
        write_reftable_config(&git_dir);
        fs::create_dir_all(git_dir.join("reftable")).expect("test operation should succeed");
        fs::write(git_dir.join("reftable").join("tables.list"), b"")
            .expect("test operation should succeed");
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let null = ObjectId::null(ObjectFormat::Sha1);
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .expect("test operation should succeed");

        store
            .write_reflog(
                "refs/heads/main",
                &[ReflogEntry {
                    old_oid: null,
                    new_oid: oid,
                    committer: b"A U Thor <author@example.com> 1 +0000".to_vec(),
                    message: b"commit: initial".to_vec(),
                }],
            )
            .expect("test operation should succeed");

        store
            .write_reflog("refs/heads/main", &[])
            .expect("test operation should succeed");

        assert!(
            store
                .read_reflog("refs/heads/main")
                .expect("test operation should succeed")
                .is_empty()
        );
        assert!(
            store
                .reflog_exists("refs/heads/main")
                .expect("test operation should succeed")
        );
        let tables = store.reftables().expect("test operation should succeed");
        assert!(
            tables
                .windows(2)
                .all(|pair| pair[0].header.max_update_index < pair[1].header.min_update_index)
        );
        let marker = tables
            .iter()
            .flat_map(|table| table.logs.iter())
            .find(|record| {
                record.refname == "refs/heads/main"
                    && matches!(
                        &record.value,
                        ReftableLogValue::Update(update)
                            if update.old_oid == null && update.new_oid == null
                    )
            })
            .expect("empty reflog marker should exist");
        let ReftableLogValue::Update(update) = &marker.value else {
            panic!("empty reflog marker should be an update");
        };
        assert_eq!(update.old_oid, null);
        assert_eq!(update.new_oid, null);

        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn file_ref_store_limits_reftable_reflog_messages_to_half_a_block() {
        let git_dir = temp_git_dir();
        write_reftable_config(&git_dir);
        fs::create_dir_all(git_dir.join("reftable")).expect("test operation should succeed");
        fs::write(git_dir.join("reftable").join("tables.list"), b"")
            .expect("test operation should succeed");
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1).with_reftable_write_options(
            ReftableWriteOptions {
                block_size: 0,
                ..ReftableWriteOptions::default()
            },
        );
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .expect("test operation should succeed");
        let mut tx = store.transaction();
        tx.update(RefUpdate {
            name: "refs/heads/main".into(),
            expected: None,
            new: RefTarget::Direct(oid),
            reflog: Some(ReflogEntry {
                old_oid: ObjectId::null(ObjectFormat::Sha1),
                new_oid: oid,
                committer: b"A U Thor <author@example.com> 1 +0000".to_vec(),
                message: vec![b'x'; 50_000],
            }),
        });
        tx.commit()
            .expect("long reflog message should be truncated");

        let message = store
            .reftables()
            .expect("test operation should succeed")
            .into_iter()
            .flat_map(|table| table.logs)
            .find_map(|record| match record.value {
                ReftableLogValue::Update(update) => Some(update.message),
                ReftableLogValue::Deletion => None,
            })
            .expect("reflog update should exist");
        assert_eq!(message.len(), 2048);

        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn file_ref_store_lists_only_visible_reftable_reflog_names() {
        let git_dir = temp_git_dir();
        write_reftable_config(&git_dir);
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .expect("test operation should succeed");

        store
            .write_reflog("refs/heads/empty", &[])
            .expect("test operation should succeed");
        store
            .append_reflog("refs/heads/main", &reflog_entry(&oid, 1, "create main"))
            .expect("test operation should succeed");

        assert_eq!(
            store
                .list_reflog_names()
                .expect("test operation should succeed"),
            vec!["refs/heads/main".to_string()]
        );

        store
            .write_reflog("refs/heads/main", &[])
            .expect("test operation should succeed");
        assert!(
            store
                .list_reflog_names()
                .expect("test operation should succeed")
                .is_empty()
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
                    "0x000000000001-0x000000000001-00000000.ref",
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
                "0x000000000001-0x000000000001-00000000.ref",
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
        let last = tables
            .lines()
            .last()
            .expect("test operation should succeed");
        // The rust-written table name follows git's `0x%012x-0x%012x-%08x.ref`
        // shape (reftable/stack.c::format_name) so `git fsck` accepts it; the
        // earlier `-sley-<nanos>` token tripped `badReftableTableName`.
        assert!(
            last.starts_with("0x") && last.ends_with(".ref"),
            "expected git-format reftable name in tables.list, got {tables}"
        );
        assert!(
            reftable_table_name_is_valid(last),
            "rust-written reftable name must parse as git's hex format, got {last}"
        );

        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn reftable_auto_compaction_uses_git_geometric_segments() {
        assert_eq!(
            reftable_compaction_segment(&[512, 64, 17, 16, 9, 9, 9, 16, 2, 16], 2),
            Some((1, 10))
        );
        assert_eq!(reftable_compaction_segment(&[64, 32, 16, 8, 4, 2], 2), None);
    }

    #[test]
    fn file_ref_store_geometric_compaction_keeps_a_stack() {
        let git_dir = temp_git_dir();
        write_reftable_config(&git_dir);
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .expect("test operation should succeed");

        for index in 1..=20 {
            let mut tx = store.transaction();
            tx.update(RefUpdate {
                name: format!("refs/heads/branch-{index}"),
                expected: None,
                new: RefTarget::Direct(oid),
                reflog: None,
            });
            tx.commit().expect("test operation should succeed");
        }

        let table_count = store
            .reftable_table_count()
            .expect("test operation should succeed");
        assert!(table_count > 1, "geometric stack collapsed to one table");
        assert!(table_count < 20, "geometric stack was never compacted");

        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn combined_reftable_transactions_auto_compact_geometrically() {
        let git_dir = temp_git_dir();
        write_reftable_config(&git_dir);
        let store =
            FileRefStore::new(&git_dir, ObjectFormat::Sha1).with_reftable_combined_logs(true);
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .expect("test operation should succeed");

        for index in 1..=20 {
            let mut tx = store.transaction();
            tx.update(RefUpdate {
                name: format!("refs/heads/branch-{index}"),
                expected: None,
                new: RefTarget::Direct(oid),
                reflog: Some(ReflogEntry {
                    old_oid: ObjectId::null(ObjectFormat::Sha1),
                    new_oid: oid,
                    committer: format!("A <a@example.invalid> {index} +0000").into_bytes(),
                    message: Vec::new(),
                }),
            });
            tx.commit().expect("test operation should succeed");
        }

        let table_count = store
            .reftable_table_count()
            .expect("test operation should succeed");
        assert!(table_count > 0, "combined transaction stack disappeared");
        assert!(table_count < 20, "combined transactions never compacted");

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
                "0x000000000001-0x000000000001-00000000.ref",
                vec![
                    sley_formats::ReftableRefRecord {
                        name: "refs/heads/main".into(),
                        update_index: 1,
                        value: ReftableRefValue::Direct(oid),
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
                        target: RefTarget::Direct(branch_oid),
                    },
                    peeled: None,
                },
                PackedRef {
                    reference: Ref {
                        name: "refs/tags/v1.0".into(),
                        target: RefTarget::Direct(tag_oid),
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
                    target: RefTarget::Direct(oid),
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
            new: RefTarget::Direct(main_oid),
            reflog: None,
        });
        tx.update(RefUpdate {
            name: "refs/tags/v1.0".into(),
            expected: None,
            new: RefTarget::Direct(tag_oid),
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
    fn post_pack_prune_preserves_a_racing_loose_update() {
        let git_dir = temp_git_dir();
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let packed_oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .expect("test operation should succeed");
        let newer_oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391",
        )
        .expect("test operation should succeed");
        let mut tx = store.transaction();
        tx.update(RefUpdate {
            name: "refs/heads/main".into(),
            expected: None,
            new: RefTarget::Direct(newer_oid),
            reflog: None,
        });
        tx.commit().expect("seed racing loose update");

        assert!(
            !store
                .prune_loose_ref_after_pack("refs/heads/main", &packed_oid)
                .expect("mismatched loose value is preserved")
        );
        assert_eq!(
            store
                .read_ref("refs/heads/main")
                .expect("read preserved loose ref"),
            Some(RefTarget::Direct(newer_oid))
        );
        assert!(!git_dir.join("refs/heads/main.lock").exists());

        assert!(
            store
                .prune_loose_ref_after_pack("refs/heads/main", &newer_oid)
                .expect("matching loose value is pruned")
        );
        assert!(!git_dir.join("refs/heads/main").exists());
        assert!(!git_dir.join("refs/heads/main.lock").exists());
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn file_ref_store_imports_files_snapshot_in_final_packed_form() {
        let git_dir = temp_git_dir();
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .expect("test operation should succeed");
        let refs = vec![
            Ref {
                name: "HEAD".into(),
                target: RefTarget::Symbolic("refs/heads/main".into()),
            },
            Ref {
                name: "ORIG_HEAD".into(),
                target: RefTarget::Direct(oid),
            },
            Ref {
                name: "refs/heads/main".into(),
                target: RefTarget::Direct(oid),
            },
        ];
        let reflogs = vec![(
            "refs/heads/main".into(),
            vec![reflog_entry(&oid, 1, "import")],
        )];

        store
            .import_snapshot(&refs, &reflogs, true)
            .expect("import files snapshot");

        assert!(git_dir.join("packed-refs").exists());
        assert!(!git_dir.join("refs/heads/main").exists());
        assert!(git_dir.join("HEAD").exists());
        assert!(git_dir.join("ORIG_HEAD").exists());
        assert_eq!(
            store.read_ref("refs/heads/main").expect("read packed ref"),
            Some(RefTarget::Direct(oid))
        );
        assert_eq!(
            store
                .read_reflog("refs/heads/main")
                .expect("read reflog")
                .len(),
            1
        );
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn reftable_snapshot_export_batches_refs_and_reflogs() {
        let git_dir = temp_git_dir();
        write_reftable_config(&git_dir);
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .expect("test operation should succeed");
        let refs = vec![
            Ref {
                name: "HEAD".into(),
                target: RefTarget::Symbolic("refs/heads/main".into()),
            },
            Ref {
                name: "ORIG_HEAD".into(),
                target: RefTarget::Direct(oid),
            },
            Ref {
                name: "refs/heads/main".into(),
                target: RefTarget::Direct(oid),
            },
        ];
        let reflogs = vec![
            ("HEAD".into(), vec![reflog_entry(&oid, 1, "head")]),
            (
                "refs/heads/main".into(),
                vec![
                    reflog_entry(&oid, 2, "first"),
                    reflog_entry(&oid, 3, "second"),
                ],
            ),
        ];
        store
            .import_snapshot(&refs, &reflogs, false)
            .expect("import reftable snapshot");

        assert_eq!(
            store.export_snapshot(true).expect("export with reflogs"),
            RefSnapshot {
                refs: refs.clone(),
                reflogs,
            }
        );
        assert_eq!(
            store
                .export_snapshot(false)
                .expect("export without reflogs"),
            RefSnapshot {
                refs,
                reflogs: Vec::new(),
            }
        );
        fs::remove_dir_all(git_dir).expect("remove test reftable repo");
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
            new: RefTarget::Direct(oid),
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
            new: RefTarget::Direct(tag_oid),
            reflog: None,
        });
        tx.commit().expect("test operation should succeed");

        let packed = store
            .pack_refs_with_peeler(true, |name, oid| {
                if name == "refs/tags/v1.0" && oid == &tag_oid {
                    Ok(Some(peeled_oid))
                } else {
                    Ok(None)
                }
            })
            .expect("test operation should succeed");
        assert_eq!(packed.len(), 1);
        assert_eq!(packed[0].peeled, Some(peeled_oid));
        let bytes =
            fs::read_to_string(git_dir.join("packed-refs")).expect("test operation should succeed");
        assert!(bytes.contains(&format!("^{peeled_oid}\n")));
        assert!(!git_dir.join("refs").join("tags").join("v1.0").exists());
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    fn reflog_entry(new_oid: &ObjectId, timestamp: i64, message: &str) -> ReflogEntry {
        ReflogEntry {
            old_oid: zero_oid(new_oid.format()).expect("test operation should succeed"),
            new_oid: *new_oid,
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
            new: RefTarget::Direct(main_oid),
            reflog: Some(reflog_entry(&main_oid, 0, "create main")),
        });
        tx.update(RefUpdate {
            name: "refs/heads/topic".into(),
            expected: None,
            new: RefTarget::Direct(topic_oid),
            reflog: None,
        });
        tx.update(RefUpdate {
            name: "refs/tags/v1.0".into(),
            expected: None,
            new: RefTarget::Direct(tag_oid),
            reflog: None,
        });
        tx.commit().expect("test operation should succeed");

        assert_eq!(
            store
                .read_ref("refs/heads/main")
                .expect("test operation should succeed"),
            Some(RefTarget::Direct(main_oid))
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
    fn file_ref_transaction_preserves_batched_reflog_order() {
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
            new: RefTarget::Direct(first),
            reflog: Some(reflog_entry(&first, 1, "first")),
        });
        tx.update(RefUpdate {
            name: "refs/heads/topic".into(),
            expected: None,
            new: RefTarget::Direct(first),
            reflog: Some(reflog_entry(&first, 2, "topic")),
        });
        tx.update(RefUpdate {
            name: "refs/heads/main".into(),
            expected: None,
            new: RefTarget::Direct(second),
            reflog: Some(reflog_entry(&second, 3, "second")),
        });
        tx.commit().expect("test operation should succeed");

        let main_log = store
            .read_reflog("refs/heads/main")
            .expect("test operation should succeed");
        assert_eq!(
            main_log
                .iter()
                .map(|entry| entry.message.as_slice())
                .collect::<Vec<_>>(),
            vec![b"first".as_slice(), b"second".as_slice()]
        );
        assert_eq!(
            store
                .read_reflog("refs/heads/topic")
                .expect("test operation should succeed")[0]
                .message,
            b"topic"
        );
        assert_eq!(
            store
                .read_ref("refs/heads/main")
                .expect("test operation should succeed"),
            Some(RefTarget::Direct(second))
        );
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn file_ref_transaction_replaces_empty_directory_at_ref_path() {
        let git_dir = temp_git_dir();
        let blocked_ref = git_dir.join("refs").join("heads").join("topic");
        fs::create_dir_all(blocked_ref.join("empty").join("nested"))
            .expect("test operation should succeed");
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
            new: RefTarget::Direct(oid),
            reflog: None,
        });
        tx.commit().expect("test operation should succeed");

        assert!(blocked_ref.is_file());
        assert_eq!(
            store
                .read_ref("refs/heads/topic")
                .expect("test operation should succeed"),
            Some(RefTarget::Direct(oid))
        );
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

    #[test]
    fn file_ref_transaction_mixes_update_and_delete() {
        let git_dir = temp_git_dir();
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let old_main = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .expect("test operation should succeed");
        let new_topic = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391",
        )
        .expect("test operation should succeed");
        let mut seed = store.transaction();
        seed.update(RefUpdate {
            name: "refs/heads/main".into(),
            expected: None,
            new: RefTarget::Direct(old_main),
            reflog: None,
        });
        seed.commit().expect("test operation should succeed");

        let mut tx = store.transaction();
        tx.update(RefUpdate {
            name: "refs/heads/topic".into(),
            expected: None,
            new: RefTarget::Direct(new_topic),
            reflog: None,
        });
        tx.delete_with_precondition(
            "refs/heads/main",
            RefDeletePrecondition::Direct(Some(old_main)),
            None,
        );
        tx.commit().expect("test operation should succeed");

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
            Some(RefTarget::Direct(new_topic))
        );
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn file_ref_transaction_rejects_deleted_descendant_parent_create() {
        let git_dir = temp_git_dir();
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let old_conflict = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .expect("test operation should succeed");
        let new_parent = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391",
        )
        .expect("test operation should succeed");
        let mut seed = store.transaction();
        seed.update(RefUpdate {
            name: "refs/heads/branch/conflict".into(),
            expected: None,
            new: RefTarget::Direct(old_conflict),
            reflog: None,
        });
        seed.commit().expect("test operation should succeed");

        let mut tx = store.transaction();
        tx.delete_with_precondition(
            "refs/heads/branch/conflict",
            RefDeletePrecondition::Direct(Some(old_conflict)),
            None,
        );
        tx.update(RefUpdate {
            name: "refs/heads/branch".into(),
            expected: None,
            new: RefTarget::Direct(new_parent),
            reflog: None,
        });
        let err = tx
            .commit()
            .expect_err("D/F-conflicting delete plus create must fail");
        assert_eq!(
            err.to_string(),
            "transaction failed: cannot lock ref 'refs/heads/branch': 'refs/heads/branch/conflict' exists; cannot create 'refs/heads/branch'"
        );

        assert_eq!(
            store
                .read_ref("refs/heads/branch/conflict")
                .expect("test operation should succeed"),
            Some(RefTarget::Direct(old_conflict))
        );
        assert_eq!(
            store
                .read_ref("refs/heads/branch")
                .expect("test operation should succeed"),
            None
        );
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn file_ref_transaction_stale_delete_rolls_back_update() {
        let git_dir = temp_git_dir();
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let old_oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .expect("test operation should succeed");
        let new_oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391",
        )
        .expect("test operation should succeed");
        let mut seed = store.transaction();
        for name in ["refs/heads/main", "refs/heads/topic"] {
            seed.update(RefUpdate {
                name: name.into(),
                expected: None,
                new: RefTarget::Direct(old_oid),
                reflog: None,
            });
        }
        seed.commit().expect("test operation should succeed");

        let mut tx = store.transaction();
        tx.update(RefUpdate {
            name: "refs/heads/topic".into(),
            expected: None,
            new: RefTarget::Direct(new_oid),
            reflog: None,
        });
        tx.delete_with_precondition(
            "refs/heads/main",
            RefDeletePrecondition::Direct(Some(new_oid)),
            None,
        );
        let err = tx.commit().expect_err("stale delete must abort");
        assert!(err.to_string().contains("expected ref refs/heads/main"));

        assert_eq!(
            store
                .read_ref("refs/heads/main")
                .expect("test operation should succeed"),
            Some(RefTarget::Direct(old_oid))
        );
        assert_eq!(
            store
                .read_ref("refs/heads/topic")
                .expect("test operation should succeed"),
            Some(RefTarget::Direct(old_oid))
        );
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn file_ref_transaction_rejects_duplicate_mixed_ref() {
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
            new: RefTarget::Direct(oid),
            reflog: None,
        });
        tx.delete_with_precondition("refs/heads/main", RefDeletePrecondition::Any, None);

        let err = tx.commit().expect_err("duplicate ref must fail");
        assert!(err.to_string().contains("refs/heads/main"));
        assert_eq!(
            store
                .read_ref("refs/heads/main")
                .expect("test operation should succeed"),
            None
        );
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn file_ref_transaction_deletes_symbolic_ref_with_immediate_expectation() {
        let git_dir = temp_git_dir();
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .expect("test operation should succeed");
        let mut seed = store.transaction();
        seed.update(RefUpdate {
            name: "refs/heads/main".into(),
            expected: None,
            new: RefTarget::Direct(oid),
            reflog: None,
        });
        seed.update(RefUpdate {
            name: "refs/aliases/main".into(),
            expected: None,
            new: RefTarget::Symbolic("refs/heads/main".into()),
            reflog: None,
        });
        seed.commit().expect("test operation should succeed");

        let mut tx = store.transaction();
        tx.delete_with_precondition(
            "refs/aliases/main",
            RefDeletePrecondition::Immediate(RefTarget::Symbolic("refs/heads/main".into())),
            None,
        );
        tx.commit().expect("test operation should succeed");

        assert_eq!(
            store
                .read_ref("refs/aliases/main")
                .expect("test operation should succeed"),
            None
        );
        assert_eq!(
            store
                .read_ref("refs/heads/main")
                .expect("test operation should succeed"),
            Some(RefTarget::Direct(oid))
        );
        fs::remove_dir_all(git_dir).expect("test operation should succeed");
    }

    #[test]
    fn file_ref_transaction_rolls_back_delete_after_late_write_failure() {
        let git_dir = temp_git_dir();
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let old_oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .expect("test operation should succeed");
        let new_oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391",
        )
        .expect("test operation should succeed");
        let mut seed = store.transaction();
        for name in ["refs/heads/main", "refs/heads/topic"] {
            seed.update(RefUpdate {
                name: name.into(),
                expected: None,
                new: RefTarget::Direct(old_oid),
                reflog: None,
            });
        }
        seed.commit().expect("test operation should succeed");

        set_fail_loose_commit_action_for_test(Some(1));
        let mut tx = store.transaction();
        tx.delete_with_precondition(
            "refs/heads/main",
            RefDeletePrecondition::Direct(Some(old_oid)),
            None,
        );
        tx.update(RefUpdate {
            name: "refs/heads/topic".into(),
            expected: None,
            new: RefTarget::Direct(new_oid),
            reflog: None,
        });
        let err = tx.commit().expect_err("injected failure must abort");
        assert!(
            err.to_string()
                .contains("injected loose ref transaction failure")
        );

        assert_eq!(
            store
                .read_ref("refs/heads/main")
                .expect("test operation should succeed"),
            Some(RefTarget::Direct(old_oid))
        );
        assert_eq!(
            store
                .read_ref("refs/heads/topic")
                .expect("test operation should succeed"),
            Some(RefTarget::Direct(old_oid))
        );
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
