//! Git notes: read and write the tree-backed mapping from annotated object to
//! note blob, reachable from `refs/notes/*`.
//!
//! Notes trees may use git's fanout layout (two-hex-digit subtrees); this crate
//! reads any fanout depth and writes flat (un-fanned) trees, which git reads
//! back identically.

use sley_config::GitConfig;
use sley_core::{GitError, ObjectFormat, ObjectId, Result};
use sley_object::{
    Commit, EncodedObject, ObjectType, Tree, TreeEntries, TreeEntry, tree_entry_object_type,
};
use sley_odb::{FileObjectDatabase, ObjectReader, ObjectWriter};
use sley_refs::{FileRefStore, RefTarget, RefUpdate, ReflogEntry};
use sley_sequencer::{CommitCreate, create_commit};
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

/// Resolve the notes ref using git's precedence: explicit override, then
/// `GIT_NOTES_REF`, then `core.notesRef`, then [`DEFAULT_NOTES_REF`].
pub fn resolve_notes_ref(git_dir: &Path, ref_override: Option<&str>) -> Result<NotesRef> {
    if let Some(value) = ref_override {
        return Ok(NotesRef::expand(value));
    }
    if let Ok(value) = std::env::var("GIT_NOTES_REF")
        && !value.is_empty()
    {
        return Ok(NotesRef::expand(&value));
    }
    if let Ok(config) = read_repo_config(git_dir)
        && let Some(value) = config.get("core", None, "notesRef")
        && !value.is_empty()
    {
        return Ok(NotesRef::expand(value));
    }
    Ok(NotesRef::expand(DEFAULT_NOTES_REF))
}

/// List every note reachable from `notes_ref`, sorted by annotated-object hex.
pub fn list_notes(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    notes_ref: &NotesRef,
) -> Result<Vec<Note>> {
    read_all_notes(git_dir, format, store, notes_ref)
}

/// Return the note blob oid for `annotated`, if any.
pub fn read_note(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    notes_ref: &NotesRef,
    annotated: &ObjectId,
) -> Result<Option<ObjectId>> {
    let target_hex = annotated.to_hex();
    Ok(read_all_notes(git_dir, format, store, notes_ref)?
        .into_iter()
        .find(|entry| entry.annotated.to_hex() == target_hex)
        .map(|entry| entry.blob))
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

/// Rewrite the notes tree to exactly `notes` and advance `notes_ref` to a new
/// commit. An empty set still records a commit on the empty tree.
pub fn write_notes(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    notes_ref: &NotesRef,
    notes: &[Note],
    message: &str,
    identity: &NotesCommitIdentity,
) -> Result<()> {
    let mut db = FileObjectDatabase::from_git_dir(git_dir, format);

    let parent = match store.read_ref(notes_ref.as_str())? {
        Some(RefTarget::Direct(oid)) => Some(oid),
        _ => None,
    };

    let mut entries: Vec<TreeEntry> = notes
        .iter()
        .map(|note| TreeEntry {
            mode: 0o100644,
            name: note.annotated.to_hex().into_bytes(),
            oid: note.blob.clone(),
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
        },
    )?;

    let old_oid = parent.clone().unwrap_or(zero_oid(format)?);
    let mut tx = store.transaction();
    tx.update(RefUpdate {
        name: notes_ref.as_str().to_string(),
        expected: parent.map(RefTarget::Direct),
        new: RefTarget::Direct(commit_oid.clone()),
        reflog: Some(ReflogEntry {
            old_oid,
            new_oid: commit_oid,
            committer: identity.committer.clone(),
            message: message.as_bytes().to_vec(),
        }),
    });
    tx.commit()?;
    Ok(())
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
            annotated: annotated.clone(),
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

fn read_all_notes(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    notes_ref: &NotesRef,
) -> Result<Vec<Note>> {
    let Some(tree_oid) = notes_tree_oid(git_dir, format, store, notes_ref)? else {
        return Ok(Vec::new());
    };
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let mut out = Vec::new();
    collect_notes(&db, format, &tree_oid, "", &mut out)?;
    out.sort_by_key(|entry| entry.annotated.to_hex());
    Ok(out)
}

fn collect_notes(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    prefix: &str,
    out: &mut Vec<Note>,
) -> Result<()> {
    let object = db.read_object(tree_oid)?;
    if object.object_type != ObjectType::Tree {
        return Ok(());
    }
    for entry in TreeEntries::new(format, &object.body) {
        let entry = entry?;
        let Ok(name) = std::str::from_utf8(entry.name) else {
            continue;
        };
        if !name.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            continue;
        }
        if tree_entry_object_type(entry.mode) == ObjectType::Tree {
            let mut nested = prefix.to_string();
            nested.push_str(name);
            collect_notes(db, format, &entry.oid, &nested, out)?;
        } else {
            let mut hex = prefix.to_string();
            hex.push_str(name);
            if hex.len() != format.hex_len() {
                continue;
            }
            let Ok(annotated) = ObjectId::from_hex(format, &hex) else {
                continue;
            };
            out.push(Note {
                annotated,
                blob: entry.oid,
            });
        }
    }
    Ok(())
}

fn expand_notes_ref(name: &str) -> String {
    if name.starts_with("refs/notes/") {
        name.to_string()
    } else {
        format!("refs/notes/{name}")
    }
}

fn read_repo_config(git_dir: &Path) -> Result<GitConfig> {
    GitConfig::read(git_dir.join("config"))
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
            author: format_commit_identity(NAME, EMAIL, DATE).expect("test operation should succeed"),
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
        git_env(&mut Command::new("git").current_dir(root).args(["init", "-q"]))
            .status()
            .expect("git init should succeed");
        fs::write(root.join("f.txt"), b"content\n").expect("write worktree file");
        git_env(&mut Command::new("git").current_dir(root).args(["add", "f.txt"]))
            .status()
            .expect("git add should succeed");
        git_env(
            &mut Command::new("git")
                .current_dir(root)
                .args(["commit", "-q", "-m", "c1"]),
        )
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
        assert_eq!(
            NotesRef::expand("commits").as_str(),
            "refs/notes/commits"
        );
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
        upsert_note(&mut notes, &target, blob.clone());
        write_notes(
            &git_dir,
            format,
            &store,
            &notes_ref,
            &notes,
            "Notes added by test",
            &identity,
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
                name: suffix.as_bytes().to_vec(),
                oid: blob.clone(),
            }],
        };
        let leaf_oid = db
            .write_object(EncodedObject::new(ObjectType::Tree, leaf.write()))
            .expect("test operation should succeed");
        let fanout = Tree {
            entries: vec![TreeEntry {
                mode: 0o040000,
                name: prefix.as_bytes().to_vec(),
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

            let git_add = git_env(
                &mut Command::new("git")
                    .current_dir(&dir)
                    .args(["notes", "add", "-m", "interop note", "HEAD"]),
            )
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

            let git_output = git_env(
                &mut Command::new("git")
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
}