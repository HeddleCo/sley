//! Object read helpers and zero-copy parse views.

use std::sync::Arc;

use sley_object::{CommitRef, EncodedObject, ObjectType};
use sley_odb::{FileObjectDatabase, ObjectReader};

use crate::{GitError, ObjectFormat, ObjectId, Repository, Result};

/// A loaded object whose body bytes can be parsed without copying.
#[derive(Debug, Clone)]
pub struct LoadedObject {
    object: Arc<EncodedObject>,
}

impl LoadedObject {
    /// The object's type and uncompressed body size.
    pub fn header(&self) -> (ObjectType, u64) {
        (self.object.object_type, self.object.body.len() as u64)
    }

    /// Borrowed commit parse-view over the loaded body.
    pub fn commit_ref(&self, format: ObjectFormat) -> Result<CommitRef<'_>> {
        if self.object.object_type != ObjectType::Commit {
            return Err(GitError::InvalidObject(format!(
                "object is a {}, not a commit",
                self.object.object_type.as_str()
            )));
        }
        CommitRef::parse(format, &self.object.body)
    }

    /// Raw encoded object (shared via `Arc`).
    pub fn encoded(&self) -> &Arc<EncodedObject> {
        &self.object
    }
}

impl Repository {
    /// Session-scoped object database handle (shared across clones of this repo).
    pub fn objects(&self) -> Arc<FileObjectDatabase> {
        Arc::clone(&self.objects)
    }

    /// Writable object-store view sharing this session's read caches.
    pub fn objects_mut(&self) -> FileObjectDatabase {
        self.objects.as_ref().clone()
    }

    /// Invalidate pack/decoded read caches after `fetch`, `push`, or pack install.
    pub fn refresh_objects(&self) {
        self.objects.refresh_read_cache();
    }

    /// Object type and size without decoding the body (`git cat-file --batch-check`).
    pub fn read_object_header(&self, oid: &ObjectId) -> Result<Option<(ObjectType, u64)>> {
        self.objects.read_object_header(oid)
    }

    /// Load an object for zero-copy parsing via [`LoadedObject`].
    ///
    /// Keep the returned value alive while using [`LoadedObject::commit_ref`].
    pub fn load_object(&self, oid: &ObjectId) -> Result<LoadedObject> {
        Ok(LoadedObject {
            object: ObjectReader::read_object(self.objects.as_ref(), oid)?,
        })
    }
}