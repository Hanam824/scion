use crate::BlobStore;
use anyhow::{Context, Result};
use scion_core::ObjectId;
use std::io::Write;
use std::path::{Path, PathBuf};

/// zstd-compressed blobs under a two-level hash fan-out: `ab/cd/abcd…`.
pub struct FsBlobs {
    root: PathBuf,
}

impl FsBlobs {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;
        Ok(FsBlobs { root })
    }

    fn path(&self, id: &ObjectId) -> PathBuf {
        let h = id.to_hex();
        self.root.join(&h[..2]).join(&h[2..4]).join(h)
    }
}

impl BlobStore for FsBlobs {
    fn has(&self, id: &ObjectId) -> Result<bool> {
        Ok(self.path(id).exists())
    }

    fn put(&self, id: &ObjectId, data: &[u8]) -> Result<()> {
        let path = self.path(id);
        if path.exists() {
            return Ok(());
        }
        let dir = path.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir)?;
        let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
        tmp.write_all(&zstd::encode_all(data, 3)?)?;
        tmp.persist(&path)?;
        Ok(())
    }

    fn get(&self, id: &ObjectId) -> Result<Option<Vec<u8>>> {
        match std::fs::read(self.path(id)) {
            Ok(c) => Ok(Some(zstd::decode_all(&c[..])?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let s = FsBlobs::new(dir.path()).unwrap();
        let id = ObjectId::hash(b"data");
        assert!(!s.has(&id).unwrap());
        s.put(&id, b"data").unwrap();
        s.put(&id, b"data").unwrap();
        assert!(s.has(&id).unwrap());
        assert_eq!(s.get(&id).unwrap().unwrap(), b"data");
        assert!(s.get(&ObjectId::hash(b"other")).unwrap().is_none());
    }
}
