//! Git notes: read and write the tree-backed mapping from annotated object to
//! note blob, reachable from `refs/notes/*`.
//!
//! Notes trees may use git's fanout layout (two-hex-digit subtrees); this crate
//! reads any fanout depth and writes flat (un-fanned) trees, which git reads
//! back identically.

use sley_config::GitConfig;
use sley_core::{GitError, ObjectFormat, ObjectId, Result};
use sley_object::{
    BString, Commit, EncodedObject, ObjectType, Tree, TreeEntries, TreeEntry,
    tree_entry_object_type,
};
use sley_odb::{FileObjectDatabase, ObjectReader, ObjectWriter};
use sley_refs::{FileRefStore, RefTarget, RefUpdate, ReflogEntry};
use sley_sequencer::{CommitCreate, create_commit};
use std::collections::HashSet;
use std::path::Path;

/// Default notes ref when none is selected via `GIT_NOTES_REF` or `core.notesRef`.
pub const DEFAULT_NOTES_REF: &str = "refs/notes/commits";

/// A fully-qualified notes ref name (e.g. `refs/notes/commits`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotesRef(pub String);

impl NotesRef {
    /// Qualify a notes ref name. Names already under `refs/notes/` are kept;
    /// every other spelling is placed under `refs/notes/`.
    pub fn expand(name: &str) -> Self {
        Self(expand_notes_ref(name))
    }

    /// Borrow the underlying ref string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for NotesRef {
    fn from(value: &str) -> Self {
        Self::expand(value)
    }
}

impl From<String> for NotesRef {
    fn from(value: String) -> Self {
        Self::expand(&value)
    }
}

/// A single note: annotated object oid and the note blob oid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub annotated: ObjectId,
    pub blob: ObjectId,
}

/// Author/committer lines for the notes commit (raw git identity bytes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotesCommitIdentity {
    pub author: Vec<u8>,
    pub committer: Vec<u8>,
}

/// Result of an incremental note upsert at the repository level.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpsertNoteOutcome {
    /// A new or updated note was written and the notes ref advanced.
    Updated { notes_commit: ObjectId },
    /// The annotated object already referenced this blob; no objects or ref were written.
    Unchanged,
}

/// Result of an incremental note removal at the repository level.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoveNoteOutcome {
    /// One or more notes were removed and the notes ref advanced.
    Removed { notes_commit: ObjectId },
    /// The notes ref was absent or none of the requested annotated objects had notes.
    Unchanged,
}

/// Resolve the notes ref using git's precedence: explicit override, then
/// `GIT_NOTES_REF`, then `core.notesRef`, then [`DEFAULT_NOTES_REF`].
pub fn resolve_notes_ref(git_dir: &Path, ref_override: Option<&str>) -> Result<NotesRef> {
    resolve_notes_ref_impl(git_dir, ref_override, None)
}

/// Like [`resolve_notes_ref`], but resolves `core.notesRef` against a
/// caller-supplied effective config instead of re-reading `<git_dir>/config`
/// blindly.
///
/// Callers that have already resolved the repository config — `include`/
/// `includeIf` directives plus command-line `-c` / `GIT_CONFIG_*` overrides —
/// pass it here so the notes ref honours the same `core.notesRef` the rest of the
/// command sees. The explicit override and `GIT_NOTES_REF` still take precedence,
/// matching git.
pub fn resolve_notes_ref_with_config(
    git_dir: &Path,
    ref_override: Option<&str>,
    config: &GitConfig,
) -> Result<NotesRef> {
    resolve_notes_ref_impl(git_dir, ref_override, Some(config))
}

fn resolve_notes_ref_impl(
    git_dir: &Path,
    ref_override: Option<&str>,
    config: Option<&GitConfig>,
) -> Result<NotesRef> {
    if let Some(value) = ref_override {
        return Ok(NotesRef::expand(value));
    }
    if let Ok(value) = std::env::var("GIT_NOTES_REF")
        && !value.is_empty()
    {
        return Ok(NotesRef::expand(&value));
    }
    // Prefer the caller-resolved effective config; fall back to an include-aware
    // read of `<git_dir>/config` when none was threaded in.
    let owned_config;
    let config = match config {
        Some(config) => Some(config),
        None => match read_repo_config(git_dir) {
            Ok(config) => {
                owned_config = config;
                Some(&owned_config)
            }
            Err(_) => None,
        },
    };
    if let Some(config) = config
        && let Some(value) = config.get("core", None, "notesRef")
        && !value.is_empty()
    {
        return Ok(NotesRef::expand(value));
    }
    Ok(NotesRef::expand(DEFAULT_NOTES_REF))
}

/// Lazy iterator over notes reachable from `notes_ref`.
pub struct NotesIter {
    db: FileObjectDatabase,
    format: ObjectFormat,
    stack: Vec<(ObjectId, String)>,
    pending: Vec<Note>,
}

impl NotesIter {
    fn new(
        git_dir: &Path,
        format: ObjectFormat,
        store: &FileRefStore,
        notes_ref: &NotesRef,
    ) -> Result<Self> {
        let Some(tree_oid) = notes_tree_oid(git_dir, format, store, notes_ref)? else {
            return Ok(Self {
                db: FileObjectDatabase::from_git_dir(git_dir, format),
                format,
                stack: Vec::new(),
                pending: Vec::new(),
            });
        };
        Ok(Self {
            db: FileObjectDatabase::from_git_dir(git_dir, format),
            format,
            stack: vec![(tree_oid, String::new())],
            pending: Vec::new(),
        })
    }
}

impl Iterator for NotesIter {
    type Item = Result<Note>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(note) = self.pending.pop() {
                return Some(Ok(note));
            }
            let (tree_oid, prefix) = self.stack.pop()?;
            let entries = match load_hex_tree_entries(&self.db, self.format, &tree_oid) {
                Ok(entries) => entries,
                Err(err) => return Some(Err(err)),
            };
            for (name, mode, oid) in entries.into_iter().rev() {
                if tree_entry_object_type(mode) == ObjectType::Tree {
                    let mut nested = prefix.clone();
                    nested.push_str(&name);
                    self.stack.push((oid, nested));
                } else {
                    let mut hex = prefix.clone();
                    hex.push_str(&name);
                    if hex.len() != self.format.hex_len() {
                        continue;
                    }
                    let Ok(annotated) = ObjectId::from_hex(self.format, &hex) else {
                        continue;
                    };
                    self.pending.push(Note {
                        annotated,
                        blob: oid,
                    });
                }
            }
        }
    }
}

/// Stream notes from `notes_ref` without materializing the full list.
pub fn iter_notes(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    notes_ref: &NotesRef,
) -> Result<NotesIter> {
    NotesIter::new(git_dir, format, store, notes_ref)
}

/// List every note reachable from `notes_ref`, sorted by annotated-object hex.
pub fn list_notes(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    notes_ref: &NotesRef,
) -> Result<Vec<Note>> {
    let mut notes = iter_notes(git_dir, format, store, notes_ref)?.collect::<Result<Vec<_>>>()?;
    notes.sort_by_key(|entry| entry.annotated.to_hex());
    Ok(notes)
}

/// Return the note blob oid for `annotated`, if any (fanout-aware, no full scan).
pub fn read_note_for(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    notes_ref: &NotesRef,
    annotated: &ObjectId,
) -> Result<Option<ObjectId>> {
    let Some(tree_oid) = notes_tree_oid(git_dir, format, store, notes_ref)? else {
        return Ok(None);
    };
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    lookup_note_for(&db, format, &tree_oid, "", &annotated.to_hex())
}

/// Return the note blob oid for `annotated`, if any.
pub fn read_note(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    notes_ref: &NotesRef,
    annotated: &ObjectId,
) -> Result<Option<ObjectId>> {
    read_note_for(git_dir, format, store, notes_ref, annotated)
}

/// Return the note body bytes for `annotated`, if a note exists.
pub fn read_note_bytes(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    notes_ref: &NotesRef,
    annotated: &ObjectId,
) -> Result<Option<Vec<u8>>> {
    let Some(blob) = read_note(git_dir, format, store, notes_ref, annotated)? else {
        return Ok(None);
    };
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let object = db.read_object(&blob)?;
    if object.object_type != ObjectType::Blob {
        return Err(GitError::InvalidFormat(format!(
            "note for {} is not a blob",
            annotated.to_hex()
        )));
    }
    Ok(Some(object.body.to_vec()))
}

/// Derive the compare-and-swap precondition used by legacy full-replace callers:
/// [`Some`](RefTarget::Direct) when the notes ref exists as a direct oid, otherwise
/// `None` (create-only).
pub fn notes_ref_expected(store: &FileRefStore, notes_ref: &NotesRef) -> Result<Option<RefTarget>> {
    Ok(match store.read_ref(notes_ref.as_str())? {
        Some(RefTarget::Direct(oid)) => Some(RefTarget::Direct(oid)),
        _ => None,
    })
}

/// Rewrite the notes tree to exactly `notes` and advance `notes_ref` to a new
/// commit. An empty set still records a commit on the empty tree.
///
/// `ref_expected` is the compare-and-swap precondition on the notes ref:
/// `None` means the ref must not exist; [`Some`](RefTarget::Direct) means it must
/// point at that oid. Use [`notes_ref_expected`] for legacy auto-detection.
#[allow(clippy::too_many_arguments)]
pub fn write_notes(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    notes_ref: &NotesRef,
    notes: &[Note],
    message: &str,
    identity: &NotesCommitIdentity,
    ref_expected: Option<RefTarget>,
) -> Result<()> {
    commit_notes_update(
        git_dir,
        format,
        store,
        notes_ref,
        notes,
        message,
        identity,
        ref_expected,
    )?;
    Ok(())
}

/// Incrementally upsert a single note, reading any fanout layout and writing a
/// flat sorted tree. Returns [`UpsertNoteOutcome::Unchanged`] when `annotated`
/// already maps to `blob`.
#[allow(clippy::too_many_arguments)]
pub fn upsert_note_for(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    notes_ref: &NotesRef,
    annotated: &ObjectId,
    blob: ObjectId,
    message: &str,
    identity: &NotesCommitIdentity,
    ref_expected: Option<RefTarget>,
) -> Result<UpsertNoteOutcome> {
    if let Some(existing) = read_note_for(git_dir, format, store, notes_ref, annotated)?
        && existing == blob
    {
        return Ok(UpsertNoteOutcome::Unchanged);
    }
    let mut notes = list_notes(git_dir, format, store, notes_ref)?;
    upsert_note(&mut notes, annotated, blob);
    let notes_commit = commit_notes_update(
        git_dir,
        format,
        store,
        notes_ref,
        &notes,
        message,
        identity,
        ref_expected,
    )?;
    Ok(UpsertNoteOutcome::Updated { notes_commit })
}

/// Write `body` as a blob, then call [`upsert_note_for`].
#[allow(clippy::too_many_arguments)]
pub fn upsert_note_bytes_for(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    notes_ref: &NotesRef,
    annotated: &ObjectId,
    body: &[u8],
    message: &str,
    identity: &NotesCommitIdentity,
    ref_expected: Option<RefTarget>,
) -> Result<UpsertNoteOutcome> {
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let blob = db.write_object(EncodedObject::new(ObjectType::Blob, body.to_vec()))?;
    upsert_note_for(
        git_dir,
        format,
        store,
        notes_ref,
        annotated,
        blob,
        message,
        identity,
        ref_expected,
    )
}

/// Remove the note for a single annotated object, if present.
#[allow(clippy::too_many_arguments)]
pub fn remove_note_for(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    notes_ref: &NotesRef,
    annotated: &ObjectId,
    message: &str,
    identity: &NotesCommitIdentity,
    ref_expected: Option<RefTarget>,
) -> Result<RemoveNoteOutcome> {
    remove_notes_for(
        git_dir,
        format,
        store,
        notes_ref,
        std::slice::from_ref(annotated),
        message,
        identity,
        ref_expected,
    )
}

/// Remove notes for `annotated` in a single fast-forward commit when any are
/// present. Returns [`RemoveNoteOutcome::Unchanged`] when the ref is absent or
/// none of the oids have notes.
#[allow(clippy::too_many_arguments)]
pub fn remove_notes_for(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    notes_ref: &NotesRef,
    annotated: &[ObjectId],
    message: &str,
    identity: &NotesCommitIdentity,
    ref_expected: Option<RefTarget>,
) -> Result<RemoveNoteOutcome> {
    if annotated.is_empty() || notes_head_oid(store, notes_ref)?.is_none() {
        return Ok(RemoveNoteOutcome::Unchanged);
    }
    let targets: HashSet<_> = annotated.iter().collect();
    let mut notes = list_notes(git_dir, format, store, notes_ref)?;
    let before = notes.len();
    notes.retain(|note| !targets.contains(&note.annotated));
    if notes.len() == before {
        return Ok(RemoveNoteOutcome::Unchanged);
    }
    let notes_commit = commit_notes_update(
        git_dir,
        format,
        store,
        notes_ref,
        &notes,
        message,
        identity,
        ref_expected,
    )?;
    Ok(RemoveNoteOutcome::Removed { notes_commit })
}

/// Replace (or insert) the note for `annotated` inside an in-memory note list.
pub fn upsert_note(notes: &mut Vec<Note>, annotated: &ObjectId, blob: ObjectId) {
    let target_hex = annotated.to_hex();
    if let Some(existing) = notes
        .iter_mut()
        .find(|entry| entry.annotated.to_hex() == target_hex)
    {
        existing.blob = blob;
    } else {
        notes.push(Note {
            annotated: *annotated,
            blob,
        });
    }
}

/// Remove the note for `annotated` from an in-memory note list, if present.
pub fn remove_note(notes: &mut Vec<Note>, annotated: &ObjectId) {
    let target_hex = annotated.to_hex();
    notes.retain(|entry| entry.annotated.to_hex() != target_hex);
}

/// Peel `notes_ref` to its root tree oid. Returns `None` when the ref is absent.
pub fn notes_tree_oid(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    notes_ref: &NotesRef,
) -> Result<Option<ObjectId>> {
    let Some(target) = store.read_ref(notes_ref.as_str())? else {
        return Ok(None);
    };
    let commit_oid = match target {
        RefTarget::Direct(oid) => oid,
        RefTarget::Symbolic(name) => match store.read_ref(&name)? {
            Some(RefTarget::Direct(oid)) => oid,
            _ => return Ok(None),
        },
    };
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let object = db.read_object(&commit_oid)?;
    match object.object_type {
        ObjectType::Commit => Ok(Some(Commit::parse_ref(format, &object.body)?.tree)),
        ObjectType::Tree => Ok(Some(commit_oid)),
        _ => Ok(None),
    }
}

fn load_hex_tree_entries(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &ObjectId,
) -> Result<Vec<(String, u32, ObjectId)>> {
    let object = db.read_object(tree_oid)?;
    if object.object_type != ObjectType::Tree {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in TreeEntries::new(format, &object.body) {
        let entry = entry?;
        let Ok(name) = std::str::from_utf8(entry.name) else {
            continue;
        };
        if !is_hex_name(name) {
            continue;
        }
        out.push((name.to_string(), entry.mode, entry.oid));
    }
    Ok(out)
}

fn lookup_note_for(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    prefix: &str,
    target_hex: &str,
) -> Result<Option<ObjectId>> {
    for (name, mode, oid) in load_hex_tree_entries(db, format, tree_oid)? {
        let mut hex = prefix.to_string();
        hex.push_str(&name);
        if tree_entry_object_type(mode) == ObjectType::Tree {
            if !target_hex.starts_with(&hex) {
                continue;
            }
            if let Some(blob) = lookup_note_for(db, format, &oid, &hex, target_hex)? {
                return Ok(Some(blob));
            }
        } else if hex == target_hex {
            return Ok(Some(oid));
        }
    }
    Ok(None)
}

fn is_hex_name(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn expand_notes_ref(name: &str) -> String {
    if name.starts_with("refs/notes/") {
        name.to_string()
    } else {
        format!("refs/notes/{name}")
    }
}

/// Include-aware read of `<git_dir>/config` (resolves `include`/`includeIf` and
/// layers inherited `GIT_CONFIG_*` overrides), shared with the rest of the
/// library via [`sley_config::read_repo_config`]. Used as the fallback when a
/// caller did not pass an already-resolved effective config to
/// [`resolve_notes_ref_with_config`].
fn read_repo_config(git_dir: &Path) -> Result<GitConfig> {
    sley_config::read_repo_config(git_dir, None)
}

fn notes_head_oid(store: &FileRefStore, notes_ref: &NotesRef) -> Result<Option<ObjectId>> {
    Ok(match store.read_ref(notes_ref.as_str())? {
        Some(RefTarget::Direct(oid)) => Some(oid),
        _ => None,
    })
}

#[allow(clippy::too_many_arguments)]
fn commit_notes_update(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    notes_ref: &NotesRef,
    notes: &[Note],
    message: &str,
    identity: &NotesCommitIdentity,
    ref_expected: Option<RefTarget>,
) -> Result<ObjectId> {
    let mut db = FileObjectDatabase::from_git_dir(git_dir, format);
    let parent = notes_head_oid(store, notes_ref)?;

    let mut entries: Vec<TreeEntry> = notes
        .iter()
        .map(|note| TreeEntry {
            mode: 0o100644,
            name: BString::from(note.annotated.to_hex().as_bytes()),
            oid: note.blob,
        })
        .collect();
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    let tree = Tree { entries };
    let tree_oid = db.write_object(EncodedObject::new(ObjectType::Tree, tree.write()))?;

    let parents = parent.iter().cloned().collect();
    let commit_oid = create_commit(
        &mut db,
        CommitCreate {
            tree: tree_oid,
            parents,
            author: identity.author.clone(),
            committer: identity.committer.clone(),
            message: format!("{message}\n").into_bytes(),
            encoding: None,
        },
    )?;

    let old_oid = parent.unwrap_or(zero_oid(format)?);
    let mut tx = store.transaction();
    tx.update(RefUpdate {
        name: notes_ref.as_str().to_string(),
        expected: ref_expected,
        new: RefTarget::Direct(commit_oid),
        reflog: Some(ReflogEntry {
            old_oid,
            new_oid: commit_oid,
            committer: identity.committer.clone(),
            // git prefixes the notes reflog message with "notes: " (the commit
            // message itself is left unprefixed).
            message: format!("notes: {message}").into_bytes(),
        }),
    });
    tx.commit()?;
    Ok(commit_oid)
}

fn zero_oid(format: ObjectFormat) -> Result<ObjectId> {
    ObjectId::from_hex(format, &"0".repeat(format.hex_len()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sley_sequencer::format_commit_identity;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::time::{SystemTime, UNIX_EPOCH};

    const NAME: &str = "Tester";
    const EMAIL: &str = "tester@example.com";
    const DATE: &str = "@1790000000 -0500";

    fn unique_temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("sley-notes-{name}-{}-{nanos}", std::process::id()))
    }

    fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    fn test_identity() -> NotesCommitIdentity {
        NotesCommitIdentity {
            author: format_commit_identity(NAME, EMAIL, DATE)
                .expect("test operation should succeed"),
            committer: format_commit_identity(NAME, EMAIL, DATE)
                .expect("test operation should succeed"),
        }
    }

    fn git_env(command: &mut Command) -> &mut Command {
        command
            .env("GIT_AUTHOR_NAME", NAME)
            .env("GIT_AUTHOR_EMAIL", EMAIL)
            .env("GIT_AUTHOR_DATE", DATE)
            .env("GIT_COMMITTER_NAME", NAME)
            .env("GIT_COMMITTER_EMAIL", EMAIL)
            .env("GIT_COMMITTER_DATE", DATE)
    }

    fn init_repo_with_commit(root: &Path) -> (PathBuf, ObjectId) {
        let mut init = Command::new("git");
        git_env(init.current_dir(root).args(["init", "-q"]))
            .status()
            .expect("git init should succeed");
        fs::write(root.join("f.txt"), b"content\n").expect("write worktree file");
        let mut add = Command::new("git");
        git_env(add.current_dir(root).args(["add", "f.txt"]))
            .status()
            .expect("git add should succeed");
        let mut commit = Command::new("git");
        git_env(commit.current_dir(root).args(["commit", "-q", "-m", "c1"]))
            .status()
            .expect("git commit should succeed");
        let git_dir = root.join(".git");
        let format = ObjectFormat::Sha1;
        let store = FileRefStore::new(&git_dir, format);
        let head = store
            .read_ref("HEAD")
            .expect("read HEAD")
            .expect("HEAD should exist");
        let oid = match head {
            RefTarget::Direct(oid) => oid,
            RefTarget::Symbolic(name) => match store.read_ref(&name).expect("read symref") {
                Some(RefTarget::Direct(oid)) => oid,
                other => panic!("unexpected symref target: {other:?}"),
            },
        };
        (git_dir, oid)
    }

    fn write_blob(db: &mut FileObjectDatabase, bytes: &[u8]) -> Result<ObjectId> {
        db.write_object(EncodedObject::new(ObjectType::Blob, bytes.to_vec()))
    }

    #[test]
    fn notes_ref_expand_qualifies_names() {
        assert_eq!(NotesRef::expand("commits").as_str(), "refs/notes/commits");
        assert_eq!(
            NotesRef::expand("refs/notes/review").as_str(),
            "refs/notes/review"
        );
    }

    #[test]
    fn read_write_list_round_trip() {
        let dir = unique_temp_dir("round-trip");
        fs::create_dir_all(&dir).expect("create temp dir");
        let (git_dir, target) = init_repo_with_commit(&dir);
        let format = ObjectFormat::Sha1;
        let store = FileRefStore::new(&git_dir, format);
        let notes_ref = NotesRef::expand(DEFAULT_NOTES_REF);
        let identity = test_identity();
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, format);
        let blob = write_blob(&mut db, b"hello note\n").expect("test operation should succeed");

        let mut notes = Vec::new();
        upsert_note(&mut notes, &target, blob);
        write_notes(
            &git_dir,
            format,
            &store,
            &notes_ref,
            &notes,
            "Notes added by test",
            &identity,
            notes_ref_expected(&store, &notes_ref).expect("ref expected"),
        )
        .expect("test operation should succeed");

        let listed = list_notes(&git_dir, format, &store, &notes_ref)
            .expect("test operation should succeed");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].annotated, target);
        assert_eq!(listed[0].blob, blob);

        let read_back = read_note(&git_dir, format, &store, &notes_ref, &target)
            .expect("test operation should succeed");
        assert_eq!(read_back, Some(blob));

        let bytes = read_note_bytes(&git_dir, format, &store, &notes_ref, &target)
            .expect("test operation should succeed");
        assert_eq!(bytes.as_deref(), Some(b"hello note\n" as &[u8]));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn iter_notes_matches_list_notes() {
        let dir = unique_temp_dir("iter-list");
        fs::create_dir_all(&dir).expect("create temp dir");
        let (git_dir, target) = init_repo_with_commit(&dir);
        let format = ObjectFormat::Sha1;
        let store = FileRefStore::new(&git_dir, format);
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, format);
        let blob = write_blob(&mut db, b"iter note\n").expect("blob");
        let notes_ref = NotesRef::expand(DEFAULT_NOTES_REF);
        write_notes(
            &git_dir,
            format,
            &store,
            &notes_ref,
            &[Note {
                annotated: target,
                blob,
            }],
            "note",
            &test_identity(),
            notes_ref_expected(&store, &notes_ref).expect("ref expected"),
        )
        .expect("write notes");

        let listed = list_notes(&git_dir, format, &store, &notes_ref).expect("list");
        let mut iter_collected = iter_notes(&git_dir, format, &store, &notes_ref)
            .expect("iter")
            .collect::<Result<Vec<_>>>()
            .expect("collect");
        iter_collected.sort_by_key(|entry| entry.annotated.to_hex());
        assert_eq!(listed, iter_collected);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn iter_notes_yields_every_note_in_flat_tree() {
        let dir = unique_temp_dir("iter-flat-multi");
        fs::create_dir_all(&dir).expect("create temp dir");
        let (git_dir, _) = init_repo_with_commit(&dir);
        let format = ObjectFormat::Sha1;
        let store = FileRefStore::new(&git_dir, format);
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, format);
        let first =
            ObjectId::from_hex(format, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").expect("oid");
        let second =
            ObjectId::from_hex(format, "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").expect("oid");
        let blob_a = write_blob(&mut db, b"note a\n").expect("blob");
        let blob_b = write_blob(&mut db, b"note b\n").expect("blob");
        let notes_ref = NotesRef::expand(DEFAULT_NOTES_REF);
        write_notes(
            &git_dir,
            format,
            &store,
            &notes_ref,
            &[
                Note {
                    annotated: first,
                    blob: blob_a,
                },
                Note {
                    annotated: second,
                    blob: blob_b,
                },
            ],
            "notes",
            &test_identity(),
            notes_ref_expected(&store, &notes_ref).expect("ref expected"),
        )
        .expect("write notes");

        let collected = iter_notes(&git_dir, format, &store, &notes_ref)
            .expect("iter")
            .collect::<Result<Vec<_>>>()
            .expect("collect");
        assert_eq!(collected.len(), 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_note_for_skips_unrelated_fanout_branches() {
        let dir = unique_temp_dir("lookup");
        fs::create_dir_all(&dir).expect("create temp dir");
        let (git_dir, target) = init_repo_with_commit(&dir);
        let format = ObjectFormat::Sha1;
        let store = FileRefStore::new(&git_dir, format);
        let target_hex = target.to_hex();
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, format);
        let blob = write_blob(&mut db, b"lookup note\n").expect("blob");
        let prefix = &target_hex[..2];
        let suffix = &target_hex[2..];
        let leaf = Tree {
            entries: vec![TreeEntry {
                mode: 0o100644,
                name: BString::from(suffix.as_bytes()),
                oid: blob,
            }],
        };
        let leaf_oid = db
            .write_object(EncodedObject::new(ObjectType::Tree, leaf.write()))
            .expect("leaf");
        let fanout = Tree {
            entries: vec![TreeEntry {
                mode: 0o040000,
                name: BString::from(prefix.as_bytes()),
                oid: leaf_oid,
            }],
        };
        let fanout_oid = db
            .write_object(EncodedObject::new(ObjectType::Tree, fanout.write()))
            .expect("fanout");
        let identity = test_identity();
        let commit_oid = create_commit(
            &mut db,
            CommitCreate {
                tree: fanout_oid,
                parents: Vec::new(),
                author: identity.author.clone(),
                committer: identity.committer.clone(),
                message: b"fanout notes\n".to_vec(),
                encoding: None,
            },
        )
        .expect("commit");
        let mut tx = store.transaction();
        tx.update(RefUpdate {
            name: DEFAULT_NOTES_REF.to_string(),
            expected: None,
            new: RefTarget::Direct(commit_oid),
            reflog: None,
        });
        tx.commit().expect("update ref");
        let notes_ref = NotesRef::expand(DEFAULT_NOTES_REF);
        let found = read_note_for(&git_dir, format, &store, &notes_ref, &target).expect("lookup");
        assert_eq!(found, Some(blob));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn fanout_tree_is_readable() {
        let dir = unique_temp_dir("fanout");
        fs::create_dir_all(&dir).expect("create temp dir");
        let (git_dir, target) = init_repo_with_commit(&dir);
        let format = ObjectFormat::Sha1;
        let store = FileRefStore::new(&git_dir, format);
        let target_hex = target.to_hex();
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, format);
        let blob = write_blob(&mut db, b"fanout note\n").expect("test operation should succeed");

        // Build a two-level fanout tree: ab/<rest-of-hex> -> blob
        let prefix = &target_hex[..2];
        let suffix = &target_hex[2..];
        let leaf = Tree {
            entries: vec![TreeEntry {
                mode: 0o100644,
                name: BString::from(suffix.as_bytes()),
                oid: blob,
            }],
        };
        let leaf_oid = db
            .write_object(EncodedObject::new(ObjectType::Tree, leaf.write()))
            .expect("test operation should succeed");
        let fanout = Tree {
            entries: vec![TreeEntry {
                mode: 0o040000,
                name: BString::from(prefix.as_bytes()),
                oid: leaf_oid,
            }],
        };
        let fanout_oid = db
            .write_object(EncodedObject::new(ObjectType::Tree, fanout.write()))
            .expect("test operation should succeed");

        let identity = test_identity();
        let commit_oid = create_commit(
            &mut db,
            CommitCreate {
                tree: fanout_oid,
                parents: Vec::new(),
                author: identity.author.clone(),
                committer: identity.committer.clone(),
                message: b"fanout notes\n".to_vec(),
                encoding: None,
            },
        )
        .expect("test operation should succeed");
        let mut tx = store.transaction();
        tx.update(RefUpdate {
            name: DEFAULT_NOTES_REF.to_string(),
            expected: None,
            new: RefTarget::Direct(commit_oid),
            reflog: None,
        });
        tx.commit().expect("test operation should succeed");

        let notes_ref = NotesRef::expand(DEFAULT_NOTES_REF);
        let read_back = read_note(&git_dir, format, &store, &notes_ref, &target)
            .expect("test operation should succeed");
        assert_eq!(read_back, Some(blob));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn note_bytes_match_system_git() {
        if !git_available() {
            return;
        }
        let dir = unique_temp_dir("git-interop");
        fs::create_dir_all(&dir).expect("test operation should succeed");
        let result = std::panic::catch_unwind(|| {
            let (git_dir, target) = init_repo_with_commit(&dir);
            let format = ObjectFormat::Sha1;
            let store = FileRefStore::new(&git_dir, format);
            let notes_ref = NotesRef::expand(DEFAULT_NOTES_REF);

            let mut git_add_cmd = Command::new("git");
            let git_add = git_env(git_add_cmd.current_dir(&dir).args([
                "notes",
                "add",
                "-m",
                "interop note",
                "HEAD",
            ]))
            .output()
            .expect("git notes add should run");
            assert!(
                git_add.status.success(),
                "git notes add failed: {}",
                String::from_utf8_lossy(&git_add.stderr)
            );

            let sley_bytes = read_note_bytes(&git_dir, format, &store, &notes_ref, &target)
                .expect("test operation should succeed")
                .expect("note should exist");

            let mut git_show_cmd = Command::new("git");
            let git_output = git_env(
                git_show_cmd
                    .current_dir(&dir)
                    .args(["notes", "show", "HEAD"]),
            )
            .output()
            .expect("test operation should succeed");
            assert!(
                git_output.status.success(),
                "git notes show failed: {}",
                String::from_utf8_lossy(&git_output.stderr)
            );
            assert_eq!(sley_bytes, git_output.stdout);
        });
        let _ = fs::remove_dir_all(&dir);
        result.expect("note_bytes_match_system_git assertions");
    }

    fn heddle_notes_ref() -> NotesRef {
        NotesRef::expand("refs/notes/heddle")
    }

    fn read_notes_head(store: &FileRefStore, notes_ref: &NotesRef) -> Option<ObjectId> {
        match store.read_ref(notes_ref.as_str()).expect("read ref") {
            Some(RefTarget::Direct(oid)) => Some(oid),
            _ => None,
        }
    }

    fn install_fanout_note(
        git_dir: &Path,
        store: &FileRefStore,
        notes_ref: &NotesRef,
        annotated: &ObjectId,
        blob: ObjectId,
        identity: &NotesCommitIdentity,
    ) {
        let format = ObjectFormat::Sha1;
        let annotated_hex = annotated.to_hex();
        let mut db = FileObjectDatabase::from_git_dir(git_dir, format);
        let prefix = &annotated_hex[..2];
        let suffix = &annotated_hex[2..];
        let leaf = Tree {
            entries: vec![TreeEntry {
                mode: 0o100644,
                name: BString::from(suffix.as_bytes()),
                oid: blob,
            }],
        };
        let leaf_oid = db
            .write_object(EncodedObject::new(ObjectType::Tree, leaf.write()))
            .expect("leaf");
        let fanout = Tree {
            entries: vec![TreeEntry {
                mode: 0o040000,
                name: BString::from(prefix.as_bytes()),
                oid: leaf_oid,
            }],
        };
        let fanout_oid = db
            .write_object(EncodedObject::new(ObjectType::Tree, fanout.write()))
            .expect("fanout");
        let commit_oid = create_commit(
            &mut db,
            CommitCreate {
                tree: fanout_oid,
                parents: Vec::new(),
                author: identity.author.clone(),
                committer: identity.committer.clone(),
                message: b"fanout notes\n".to_vec(),
                encoding: None,
            },
        )
        .expect("commit");
        let mut tx = store.transaction();
        tx.update(RefUpdate {
            name: notes_ref.as_str().to_string(),
            expected: notes_ref_expected(store, notes_ref).expect("ref expected"),
            new: RefTarget::Direct(commit_oid),
            reflog: None,
        });
        tx.commit().expect("update ref");
    }

    #[test]
    fn upsert_note_for_unchanged_is_noop() {
        let dir = unique_temp_dir("upsert-unchanged");
        fs::create_dir_all(&dir).expect("create temp dir");
        let (git_dir, target) = init_repo_with_commit(&dir);
        let format = ObjectFormat::Sha1;
        let store = FileRefStore::new(&git_dir, format);
        let notes_ref = heddle_notes_ref();
        let identity = test_identity();
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, format);
        let blob = write_blob(&mut db, br#"{"status":"served"}"#).expect("blob");

        let first = upsert_note_for(
            &git_dir,
            format,
            &store,
            &notes_ref,
            &target,
            blob.clone(),
            "heddle: export",
            &identity,
            None,
        )
        .expect("first upsert");
        let first_head = read_notes_head(&store, &notes_ref).expect("head");
        assert!(matches!(first, UpsertNoteOutcome::Updated { .. }));

        let second = upsert_note_for(
            &git_dir,
            format,
            &store,
            &notes_ref,
            &target,
            blob,
            "heddle: export",
            &identity,
            Some(RefTarget::Direct(first_head)),
        )
        .expect("second upsert");
        assert_eq!(second, UpsertNoteOutcome::Unchanged);
        assert_eq!(read_notes_head(&store, &notes_ref), Some(first_head));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn upsert_note_for_updates_blob() {
        let dir = unique_temp_dir("upsert-update");
        fs::create_dir_all(&dir).expect("create temp dir");
        let (git_dir, target) = init_repo_with_commit(&dir);
        let format = ObjectFormat::Sha1;
        let store = FileRefStore::new(&git_dir, format);
        let notes_ref = heddle_notes_ref();
        let identity = test_identity();
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, format);
        let blob_a = write_blob(&mut db, br#"{"v":1}"#).expect("blob a");
        let blob_b = write_blob(&mut db, br#"{"v":2}"#).expect("blob b");

        let first = upsert_note_for(
            &git_dir,
            format,
            &store,
            &notes_ref,
            &target,
            blob_a,
            "heddle: export",
            &identity,
            None,
        )
        .expect("first upsert");
        let UpsertNoteOutcome::Updated {
            notes_commit: first_commit,
        } = first
        else {
            panic!("expected first upsert to update");
        };

        let second = upsert_note_for(
            &git_dir,
            format,
            &store,
            &notes_ref,
            &target,
            blob_b,
            "heddle: export",
            &identity,
            Some(RefTarget::Direct(first_commit)),
        )
        .expect("second upsert");
        let UpsertNoteOutcome::Updated {
            notes_commit: second_commit,
        } = second
        else {
            panic!("expected second upsert to update");
        };
        assert_ne!(first_commit, second_commit);
        assert_eq!(
            read_note(&git_dir, format, &store, &notes_ref, &target).expect("read"),
            Some(blob_b)
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn upsert_note_for_creates_ref() {
        let dir = unique_temp_dir("upsert-create");
        fs::create_dir_all(&dir).expect("create temp dir");
        let (git_dir, target) = init_repo_with_commit(&dir);
        let format = ObjectFormat::Sha1;
        let store = FileRefStore::new(&git_dir, format);
        let notes_ref = heddle_notes_ref();
        let identity = test_identity();
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, format);
        let blob = write_blob(&mut db, br#"{"status":"served"}"#).expect("blob");

        assert_eq!(read_notes_head(&store, &notes_ref), None);
        let outcome = upsert_note_for(
            &git_dir,
            format,
            &store,
            &notes_ref,
            &target,
            blob,
            "heddle: export",
            &identity,
            None,
        )
        .expect("upsert");
        assert!(matches!(outcome, UpsertNoteOutcome::Updated { .. }));
        assert!(read_notes_head(&store, &notes_ref).is_some());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn upsert_note_for_cas_mismatch_fails() {
        let dir = unique_temp_dir("upsert-cas");
        fs::create_dir_all(&dir).expect("create temp dir");
        let (git_dir, target) = init_repo_with_commit(&dir);
        let format = ObjectFormat::Sha1;
        let store = FileRefStore::new(&git_dir, format);
        let notes_ref = heddle_notes_ref();
        let identity = test_identity();
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, format);
        let blob_a = write_blob(&mut db, br#"{"v":1}"#).expect("blob a");
        let blob_b = write_blob(&mut db, br#"{"v":2}"#).expect("blob b");

        upsert_note_for(
            &git_dir,
            format,
            &store,
            &notes_ref,
            &target,
            blob_a,
            "heddle: export",
            &identity,
            None,
        )
        .expect("seed note");
        let head = read_notes_head(&store, &notes_ref).expect("head");
        let wrong =
            ObjectId::from_hex(format, "cccccccccccccccccccccccccccccccccccccccc").expect("oid");

        let err = upsert_note_for(
            &git_dir,
            format,
            &store,
            &notes_ref,
            &target,
            blob_b,
            "heddle: export",
            &identity,
            Some(RefTarget::Direct(wrong)),
        )
        .expect_err("cas mismatch");
        assert!(matches!(err, GitError::Transaction(_)));
        assert_eq!(read_notes_head(&store, &notes_ref), Some(head));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_notes_for_partial_hit() {
        let dir = unique_temp_dir("remove-partial");
        fs::create_dir_all(&dir).expect("create temp dir");
        let (git_dir, target) = init_repo_with_commit(&dir);
        let format = ObjectFormat::Sha1;
        let store = FileRefStore::new(&git_dir, format);
        let notes_ref = heddle_notes_ref();
        let identity = test_identity();
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, format);
        let other =
            ObjectId::from_hex(format, "dddddddddddddddddddddddddddddddddddddddd").expect("oid");
        let blob_a = write_blob(&mut db, br#"{"a":1}"#).expect("blob a");
        let blob_b = write_blob(&mut db, br#"{"b":2}"#).expect("blob b");

        write_notes(
            &git_dir,
            format,
            &store,
            &notes_ref,
            &[
                Note {
                    annotated: target,
                    blob: blob_a,
                },
                Note {
                    annotated: other,
                    blob: blob_b,
                },
            ],
            "seed",
            &identity,
            None,
        )
        .expect("seed notes");
        let head = read_notes_head(&store, &notes_ref).expect("head");

        let outcome = remove_notes_for(
            &git_dir,
            format,
            &store,
            &notes_ref,
            &[target],
            "heddle: retract",
            &identity,
            Some(RefTarget::Direct(head)),
        )
        .expect("remove");
        assert!(matches!(outcome, RemoveNoteOutcome::Removed { .. }));
        assert_eq!(
            read_note(&git_dir, format, &store, &notes_ref, &target).expect("read"),
            None
        );
        assert_eq!(
            read_note(&git_dir, format, &store, &notes_ref, &other).expect("read"),
            Some(blob_b)
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_notes_for_noop_when_missing() {
        let dir = unique_temp_dir("remove-noop");
        fs::create_dir_all(&dir).expect("create temp dir");
        let (git_dir, target) = init_repo_with_commit(&dir);
        let format = ObjectFormat::Sha1;
        let store = FileRefStore::new(&git_dir, format);
        let notes_ref = heddle_notes_ref();
        let identity = test_identity();
        let missing =
            ObjectId::from_hex(format, "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee").expect("oid");

        let absent = remove_notes_for(
            &git_dir,
            format,
            &store,
            &notes_ref,
            &[target],
            "heddle: retract",
            &identity,
            None,
        )
        .expect("remove absent ref");
        assert_eq!(absent, RemoveNoteOutcome::Unchanged);

        let mut db = FileObjectDatabase::from_git_dir(&git_dir, format);
        let blob = write_blob(&mut db, br#"{"x":1}"#).expect("blob");
        upsert_note_for(
            &git_dir,
            format,
            &store,
            &notes_ref,
            &target,
            blob,
            "heddle: export",
            &identity,
            None,
        )
        .expect("seed");
        let head = read_notes_head(&store, &notes_ref).expect("head");

        let noop = remove_notes_for(
            &git_dir,
            format,
            &store,
            &notes_ref,
            &[missing],
            "heddle: retract",
            &identity,
            Some(RefTarget::Direct(head)),
        )
        .expect("remove missing oid");
        assert_eq!(noop, RemoveNoteOutcome::Unchanged);
        assert_eq!(read_notes_head(&store, &notes_ref), Some(head));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_notes_for_batch_single_commit() {
        let dir = unique_temp_dir("remove-batch");
        fs::create_dir_all(&dir).expect("create temp dir");
        let (git_dir, _) = init_repo_with_commit(&dir);
        let format = ObjectFormat::Sha1;
        let store = FileRefStore::new(&git_dir, format);
        let notes_ref = heddle_notes_ref();
        let identity = test_identity();
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, format);
        let first =
            ObjectId::from_hex(format, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").expect("oid");
        let second =
            ObjectId::from_hex(format, "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").expect("oid");
        let third =
            ObjectId::from_hex(format, "cccccccccccccccccccccccccccccccccccccccc").expect("oid");
        let blob_a = write_blob(&mut db, b"a\n").expect("blob");
        let blob_b = write_blob(&mut db, b"b\n").expect("blob");
        let blob_c = write_blob(&mut db, b"c\n").expect("blob");

        write_notes(
            &git_dir,
            format,
            &store,
            &notes_ref,
            &[
                Note {
                    annotated: first,
                    blob: blob_a,
                },
                Note {
                    annotated: second,
                    blob: blob_b,
                },
                Note {
                    annotated: third,
                    blob: blob_c,
                },
            ],
            "seed",
            &identity,
            None,
        )
        .expect("seed");
        let head = read_notes_head(&store, &notes_ref).expect("head");

        let RemoveNoteOutcome::Removed { notes_commit } = remove_notes_for(
            &git_dir,
            format,
            &store,
            &notes_ref,
            &[first, second],
            "heddle: retract",
            &identity,
            Some(RefTarget::Direct(head)),
        )
        .expect("batch remove") else {
            panic!("expected removal");
        };

        let db = FileObjectDatabase::from_git_dir(&git_dir, format);
        let commit = db.read_object(&notes_commit).expect("read commit");
        let commit = Commit::parse_ref(format, &commit.body).expect("parse");
        assert_eq!(commit.parents.len(), 1);
        assert_eq!(commit.parents[0], head);
        assert_eq!(
            list_notes(&git_dir, format, &store, &notes_ref)
                .expect("list")
                .len(),
            1
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn incremental_ops_read_fanout_legacy() {
        let dir = unique_temp_dir("incremental-fanout");
        fs::create_dir_all(&dir).expect("create temp dir");
        let (git_dir, target) = init_repo_with_commit(&dir);
        let format = ObjectFormat::Sha1;
        let store = FileRefStore::new(&git_dir, format);
        let notes_ref = heddle_notes_ref();
        let identity = test_identity();
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, format);
        let blob = write_blob(&mut db, br#"{"legacy":true}"#).expect("blob");

        install_fanout_note(&git_dir, &store, &notes_ref, &target, blob, &identity);
        let head = read_notes_head(&store, &notes_ref).expect("head");
        let new_blob = write_blob(&mut db, br#"{"legacy":false}"#).expect("new blob");

        upsert_note_for(
            &git_dir,
            format,
            &store,
            &notes_ref,
            &target,
            new_blob,
            "heddle: export",
            &identity,
            Some(RefTarget::Direct(head)),
        )
        .expect("upsert fanout");

        assert_eq!(
            read_note_for(&git_dir, format, &store, &notes_ref, &target).expect("read"),
            Some(new_blob)
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn incremental_ops_ff_chain() {
        let dir = unique_temp_dir("incremental-ff");
        fs::create_dir_all(&dir).expect("create temp dir");
        let (git_dir, target) = init_repo_with_commit(&dir);
        let format = ObjectFormat::Sha1;
        let store = FileRefStore::new(&git_dir, format);
        let notes_ref = heddle_notes_ref();
        let identity = test_identity();
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, format);
        let other =
            ObjectId::from_hex(format, "ffffffffffffffffffffffffffffffffffffffff").expect("oid");
        let blob_a = write_blob(&mut db, br#"{"first":true}"#).expect("blob a");
        let blob_b = write_blob(&mut db, br#"{"second":true}"#).expect("blob b");

        let UpsertNoteOutcome::Updated {
            notes_commit: first_commit,
        } = upsert_note_for(
            &git_dir,
            format,
            &store,
            &notes_ref,
            &target,
            blob_a,
            "heddle: export",
            &identity,
            None,
        )
        .expect("first upsert")
        else {
            panic!("expected update");
        };

        let UpsertNoteOutcome::Updated {
            notes_commit: second_commit,
        } = upsert_note_for(
            &git_dir,
            format,
            &store,
            &notes_ref,
            &other,
            blob_b,
            "heddle: export",
            &identity,
            Some(RefTarget::Direct(first_commit)),
        )
        .expect("second upsert")
        else {
            panic!("expected update");
        };

        let db = FileObjectDatabase::from_git_dir(&git_dir, format);
        let object = db.read_object(&second_commit).expect("read commit");
        let commit = Commit::parse_ref(format, &object.body).expect("parse");
        assert_eq!(commit.parents, vec![first_commit]);
        let _ = fs::remove_dir_all(&dir);
    }
}
