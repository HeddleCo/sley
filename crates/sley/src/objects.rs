//! Object read helpers and zero-copy parse views.

use std::sync::Arc;

use sley_object::{CommitRef, EncodedObject, ObjectType};
use sley_odb::{FileObjectDatabase, ObjectReader};

use crate::{
    GitError, MissingObjectContext, MissingObjectKind, ObjectFormat, ObjectId, Repository, Result,
};

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

/// Options for lazy blob reads that may cross a promisor/remote boundary.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BlobFetchOptions {
    remote: Option<String>,
}

impl BlobFetchOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_remote(remote: impl Into<String>) -> Self {
        Self {
            remote: Some(remote.into()),
        }
    }

    pub fn remote(&self) -> Option<&str> {
        self.remote.as_deref()
    }
}

/// Blob read facade for embedders that need a single local/lazy boundary.
#[derive(Debug, Clone)]
pub struct BlobStore<'repo> {
    repo: &'repo Repository,
}

impl<'repo> BlobStore<'repo> {
    pub(crate) fn new(repo: &'repo Repository) -> Self {
        Self { repo }
    }

    /// Read a local blob.
    pub fn read(&self, oid: ObjectId) -> Result<Vec<u8>> {
        self.read_local_blob(oid, MissingObjectContext::Read)
    }

    /// Read a blob, returning a typed remote-boundary missing-object error when
    /// the local object is absent and a remote fetch policy was supplied.
    ///
    /// This async signature is intentionally stable for future promisor fetch
    /// support; today it completes immediately after the local object lookup.
    pub async fn read_or_fetch(&self, oid: ObjectId, options: BlobFetchOptions) -> Result<Vec<u8>> {
        self.read_or_fetch_blocking(oid, options)
    }

    /// Synchronous form of [`BlobStore::read_or_fetch`] for non-async callers.
    pub fn read_or_fetch_blocking(
        &self,
        oid: ObjectId,
        options: BlobFetchOptions,
    ) -> Result<Vec<u8>> {
        let context = if options.remote().is_some() {
            MissingObjectContext::RemoteBoundary
        } else {
            MissingObjectContext::Read
        };
        self.read_local_blob(oid, context)
    }

    fn read_local_blob(&self, oid: ObjectId, context: MissingObjectContext) -> Result<Vec<u8>> {
        let object = self
            .repo
            .read_object(&oid)
            .map_err(|err| match err.not_found_kind() {
                Some(crate::NotFoundKind::Object { .. }) => {
                    GitError::object_kind_not_found_in(oid, MissingObjectKind::Blob, context)
                }
                _ => err,
            })?;
        if object.object_type != ObjectType::Blob {
            return Err(GitError::InvalidObject(format!(
                "object {oid} is a {}, not a blob",
                object.object_type.as_str()
            )));
        }
        Ok(object.body.clone())
    }
}

impl Repository {
    /// Borrow the repository's session-scoped object database without cloning
    /// its shared handle.
    ///
    /// Engine operations which accept an [`ObjectReader`] by reference can use
    /// this view and share the repository's decoded-object and pack caches.
    pub fn object_database(&self) -> &FileObjectDatabase {
        self.objects.as_ref()
    }

    /// Session-scoped object database handle (shared across clones of this repo).
    pub fn objects(&self) -> Arc<FileObjectDatabase> {
        Arc::clone(&self.objects)
    }

    /// Writable object-store view sharing this session's read caches.
    pub fn objects_mut(&self) -> FileObjectDatabase {
        self.objects.as_ref().clone()
    }

    /// Blob reads with a single boundary for future lazy hydration support.
    pub fn blobs(&self) -> BlobStore<'_> {
        BlobStore::new(self)
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
