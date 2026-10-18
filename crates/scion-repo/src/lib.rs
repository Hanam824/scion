//! The Scion server engine: changeset creation, derived data, and
//! server-side history operations (diff, merge, rebase, log, blame).
//! Synchronous; the gRPC layer runs it on blocking threads.

pub mod blame;
pub mod derive;
pub mod graph;
pub mod merge;
pub mod tree;

use anyhow::{Context, Result};
use scion_core::{Changeset, FileOp, Object, ObjectId, Tree};
use scion_store::{BlobStore, FsBlobs, MetaStore, SqliteMeta};
use std::path::Path;
use std::sync::Arc;

pub use derive::{Derivation, Graph, GraphInfo, HistRoot, RootTree};
pub use merge::{Conflict, LandResult, Strategy};
pub use tree::FileDiff;

#[derive(Debug, thiserror::Error)]
pub enum RepoError {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid argument: {0}")]
    Invalid(String),
}

pub fn not_found(msg: impl Into<String>) -> anyhow::Error {
    RepoError::NotFound(msg.into()).into()
}

pub fn invalid(msg: impl Into<String>) -> anyhow::Error {
    RepoError::Invalid(msg.into()).into()
}

pub fn empty_tree_id() -> ObjectId {
    Tree::default().id()
}

pub struct Repo {
    meta: Arc<dyn MetaStore>,
    blobs: Arc<dyn BlobStore>,
}

impl Repo {
    pub fn new(meta: Arc<dyn MetaStore>, blobs: Arc<dyn BlobStore>) -> Result<Self> {
        let repo = Repo { meta, blobs };
        repo.put_tree(&Tree::default())?;
        Ok(repo)
    }

    /// SQLite metadata and filesystem blobs under `dir`.
    pub fn open_local(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let meta = SqliteMeta::open(&dir.join("meta.sqlite"))?;
        let blobs = FsBlobs::new(dir.join("blobs"))?;
        Self::new(Arc::new(meta), Arc::new(blobs))
    }

    pub fn meta(&self) -> &dyn MetaStore {
        &*self.meta
    }

    pub fn has_blob(&self, id: &ObjectId) -> Result<bool> {
        self.blobs.has(id)
    }

    pub fn put_blob(&self, data: &[u8]) -> Result<ObjectId> {
        let id = ObjectId::hash(data);
        self.blobs.put(&id, data)?;
        Ok(id)
    }

    /// Stores a client-supplied blob after checking it matches its id.
    pub fn put_blob_checked(&self, id: &ObjectId, data: &[u8]) -> Result<()> {
        if ObjectId::hash(data) != *id {
            return Err(invalid(format!("blob content does not match id {id}")));
        }
        self.blobs.put(id, data)
    }

    pub fn get_blob(&self, id: &ObjectId) -> Result<Vec<u8>> {
        self.blobs
            .get(id)?
            .ok_or_else(|| not_found(format!("blob {id}")))
    }

    pub fn put_tree<T: Object>(&self, t: &T) -> Result<ObjectId> {
        let bytes = t.encode();
        let id = ObjectId::hash(&bytes);
        self.meta.put_tree(&id, &bytes)?;
        Ok(id)
    }

    pub fn get_tree<T: Object>(&self, id: &ObjectId) -> Result<T> {
        let bytes = self
            .meta
            .get_tree(id)?
            .ok_or_else(|| not_found(format!("tree {id}")))?;
        Ok(T::decode(&bytes)?)
    }

    pub fn get_changeset(&self, id: &ObjectId) -> Result<Changeset> {
        let bytes = self
            .meta
            .get_changeset(id)?
            .ok_or_else(|| not_found(format!("changeset {id}")))?;
        Ok(Changeset::decode(&bytes)?)
    }

    /// Validates and appends a changeset, then computes eager derived data.
    /// Never touches refs. Idempotent: an identical changeset returns the
    /// existing id.
    pub fn create_changeset(&self, mut cs: Changeset) -> Result<ObjectId> {
        cs.normalize().map_err(invalid)?;
        for p in &cs.parents {
            self.get_changeset(p)?;
        }
        for c in &cs.changes {
            if let FileOp::Upsert { blob, .. } = &c.op {
                if !self.has_blob(blob)? {
                    return Err(invalid(format!(
                        "blob {blob} for {} was not uploaded",
                        c.path
                    )));
                }
            }
        }
        let id = cs.id();
        if self.meta.get_changeset(&id)?.is_some() {
            return Ok(id);
        }
        // Applying the changes to the parent tree also validates them, so do
        // it before the changeset becomes visible in the log.
        let parent_root = match cs.parents.first() {
            Some(p) => self.derive::<RootTree>(p)?,
            None => empty_tree_id(),
        };
        let root = tree::apply_changes(self, parent_root, &cs.changes)?;
        self.meta.append_changeset(&id, &cs.encode())?;
        self.store_derived::<RootTree>(&id, &root)?;
        self.derive::<Graph>(&id)?;
        self.derive::<HistRoot>(&id)?;
        Ok(id)
    }

    /// Resolves a revision: a ref name, a full or abbreviated (>= 4 hex
    /// chars) changeset id, optionally followed by `~N` (Nth first parent).
    pub fn resolve(&self, rev: &str) -> Result<ObjectId> {
        let rev = rev.trim();
        let (base, steps) = match rev.rsplit_once('~') {
            Some((b, "")) => (b, 1),
            Some((b, n)) if n.chars().all(|c| c.is_ascii_digit()) => {
                (b, n.parse::<u64>().map_err(|e| invalid(e.to_string()))?)
            }
            _ => (rev, 0),
        };
        let id = self.resolve_base(base)?;
        if steps == 0 {
            return Ok(id);
        }
        graph::first_parent_ancestor(self, id, steps)?
            .ok_or_else(|| not_found(format!("{rev}: history is too short")))
    }

    fn resolve_base(&self, s: &str) -> Result<ObjectId> {
        if s.is_empty() {
            return Err(invalid("empty revision"));
        }
        if let Some(id) = self.meta.get_ref(s)? {
            return Ok(id);
        }
        if (4..=64).contains(&s.len()) && s.chars().all(|c| c.is_ascii_hexdigit()) {
            let s = s.to_ascii_lowercase();
            if s.len() == 64 {
                let id = ObjectId::from_hex(&s)?;
                if self.meta.get_changeset(&id)?.is_some() {
                    return Ok(id);
                }
            } else {
                let found = self.meta.find_changesets_by_prefix(&s, 2)?;
                match found.len() {
                    1 => return Ok(found[0]),
                    2 => return Err(invalid(format!("ambiguous revision {s}"))),
                    _ => {}
                }
            }
        }
        Err(not_found(format!("revision {s:?}")))
    }
}

#[cfg(test)]
pub(crate) mod testutil {
    use super::*;
    use scion_core::{Change, FileMode};

    pub fn repo() -> (Repo, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (Repo::open_local(dir.path()).unwrap(), dir)
    }

    /// Commits `files` (path, Some(content) | None for delete) on `parents`.
    pub fn commit(
        repo: &Repo,
        parents: &[ObjectId],
        msg: &str,
        files: &[(&str, Option<&str>)],
    ) -> ObjectId {
        let changes = files
            .iter()
            .map(|(p, c)| Change {
                path: p.to_string(),
                op: match c {
                    Some(c) => FileOp::Upsert {
                        blob: repo.put_blob(c.as_bytes()).unwrap(),
                        mode: FileMode::Regular,
                    },
                    None => FileOp::Delete,
                },
            })
            .collect();
        repo.create_changeset(Changeset {
            parents: parents.to_vec(),
            author: "tester".into(),
            message: msg.into(),
            timestamp: 0,
            changes,
        })
        .unwrap()
    }

    pub fn read(repo: &Repo, cs: ObjectId, path: &str) -> Option<String> {
        let root = repo.derive::<RootTree>(&cs).unwrap();
        tree::lookup_file(repo, root, path)
            .unwrap()
            .map(|(b, _)| String::from_utf8(repo.get_blob(&b).unwrap()).unwrap())
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::*;

    #[test]
    fn resolve_revisions() {
        let (repo, _d) = repo();
        let a = commit(&repo, &[], "a", &[("f", Some("1"))]);
        let b = commit(&repo, &[a], "b", &[("f", Some("2"))]);
        assert!(repo.meta().cas_ref("main", None, Some(b)).unwrap());
        assert_eq!(repo.resolve("main").unwrap(), b);
        assert_eq!(repo.resolve("main~").unwrap(), a);
        assert_eq!(repo.resolve("main~1").unwrap(), a);
        assert_eq!(repo.resolve(&b.to_hex()[..10]).unwrap(), b);
        assert!(repo.resolve("main~2").is_err());
        assert!(repo.resolve("nope").is_err());
    }

    #[test]
    fn invalid_changes_rejected() {
        let (repo, _d) = repo();
        let a = commit(&repo, &[], "a", &[("dir/f", Some("1"))]);
        let blob = repo.put_blob(b"x").unwrap();
        let bad = |path: &str, op| {
            repo.create_changeset(scion_core::Changeset {
                parents: vec![a],
                author: String::new(),
                message: String::new(),
                timestamp: 0,
                changes: vec![scion_core::Change {
                    path: path.into(),
                    op,
                }],
            })
        };
        assert!(bad("missing", scion_core::FileOp::Delete).is_err());
        assert!(bad(
            "dir",
            scion_core::FileOp::Upsert {
                blob,
                mode: Default::default()
            }
        )
        .is_err());
        assert!(bad(
            "dir/f/g",
            scion_core::FileOp::Upsert {
                blob,
                mode: Default::default()
            }
        )
        .is_err());
    }
}
