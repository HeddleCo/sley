//! Repository-level git notes helpers.

use sley_notes::{
    Note, NotesCommitIdentity, NotesIter, NotesRef, iter_notes, list_notes,
    read_note_bytes, read_note_for, resolve_notes_ref, write_notes,
};

use crate::{ObjectId, Repository, Result};

impl Repository {
    /// Resolve the active notes ref (honours `GIT_NOTES_REF` and `core.notesRef`).
    pub fn notes_ref(&self, override_ref: Option<&str>) -> Result<NotesRef> {
        resolve_notes_ref(self.git_dir(), override_ref)
    }

    /// List every note on `notes_ref`.
    pub fn list_notes(&self, notes_ref: &NotesRef) -> Result<Vec<Note>> {
        list_notes(
            self.git_dir(),
            self.object_format(),
            &self.references(),
            notes_ref,
        )
    }

    /// Stream notes from `notes_ref` without loading the full list.
    pub fn iter_notes(&self, notes_ref: &NotesRef) -> Result<NotesIter> {
        iter_notes(
            self.git_dir(),
            self.object_format(),
            &self.references(),
            notes_ref,
        )
    }

    /// Return the note blob oid for `annotated`, if any (fanout-aware lookup).
    pub fn read_note_for(
        &self,
        notes_ref: &NotesRef,
        annotated: &ObjectId,
    ) -> Result<Option<ObjectId>> {
        read_note_for(
            self.git_dir(),
            self.object_format(),
            &self.references(),
            notes_ref,
            annotated,
        )
    }

    /// Return the note blob oid for `annotated`, if any.
    pub fn read_note(&self, notes_ref: &NotesRef, annotated: &ObjectId) -> Result<Option<ObjectId>> {
        self.read_note_for(notes_ref, annotated)
    }

    /// Return decoded note bytes for `annotated`, if a note exists.
    pub fn read_note_bytes(
        &self,
        notes_ref: &NotesRef,
        annotated: &ObjectId,
    ) -> Result<Option<Vec<u8>>> {
        read_note_bytes(
            self.git_dir(),
            self.object_format(),
            &self.references(),
            notes_ref,
            annotated,
        )
    }

    /// Rewrite the notes tree and advance `notes_ref` to a new commit.
    pub fn write_notes(
        &self,
        notes_ref: &NotesRef,
        notes: &[Note],
        message: &str,
        identity: &NotesCommitIdentity,
    ) -> Result<()> {
        write_notes(
            self.git_dir(),
            self.object_format(),
            &self.references(),
            notes_ref,
            notes,
            message,
            identity,
        )
    }
}