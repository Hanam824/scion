//! Pluggable storage. The article's Scion uses Spanner (metadata, refs) and
//! cloud object storage (blobs); here both sit behind traits with local
//! SQLite and filesystem implementations.

mod fs;
mod sqlite;

pub use fs::FsBlobs;
pub use sqlite::SqliteMeta;

use anyhow::Result;
use scion_core::ObjectId;

/// Immutable content-addressed blob storage. Callers verify ids.
pub trait BlobStore: Send + Sync {
    fn has(&self, id: &ObjectId) -> Result<bool>;
    fn put(&self, id: &ObjectId, data: &[u8]) -> Result<()>;
    fn get(&self, id: &ObjectId) -> Result<Option<Vec<u8>>>;
}

/// Metadata storage: the changeset log, tree objects, refs and derived data.
pub trait MetaStore: Send + Sync {
    /// Appends to the changeset log; a no-op if `id` already exists.
    fn append_changeset(&self, id: &ObjectId, data: &[u8]) -> Result<()>;
    fn get_changeset(&self, id: &ObjectId) -> Result<Option<Vec<u8>>>;
    /// Changesets whose hex id starts with `prefix` (lowercase hex).
    fn find_changesets_by_prefix(&self, prefix: &str, limit: usize) -> Result<Vec<ObjectId>>;

    fn put_tree(&self, id: &ObjectId, data: &[u8]) -> Result<()>;
    fn get_tree(&self, id: &ObjectId) -> Result<Option<Vec<u8>>>;

    fn get_ref(&self, name: &str) -> Result<Option<ObjectId>>;
    /// Compare-and-swap: moves `name` from `expected` to `new` atomically.
    /// `None` as `expected` means the ref must not exist; `None` as `new`
    /// deletes it. Returns false if the current value differs.
    fn cas_ref(
        &self,
        name: &str,
        expected: Option<ObjectId>,
        new: Option<ObjectId>,
    ) -> Result<bool>;
    fn list_refs(&self) -> Result<Vec<(String, ObjectId)>>;

    fn get_derived(&self, kind: &str, key: &str) -> Result<Option<Vec<u8>>>;
    fn put_derived(&self, kind: &str, key: &str, data: &[u8]) -> Result<()>;
}
