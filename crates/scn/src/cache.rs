use anyhow::Result;
use scion_core::ObjectId;
use std::fs::{self, FileTimes};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const DEFAULT_MAX_BYTES: u64 = 1 << 30;

/// Bounded local blob cache with LRU eviction by file mtime (touched on
/// read). Size limit from `SCN_CACHE_MAX_BYTES`.
pub struct Cache {
    dir: PathBuf,
    max_bytes: u64,
}

impl Cache {
    pub fn new(root: &Path) -> Self {
        let max_bytes = std::env::var("SCN_CACHE_MAX_BYTES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_MAX_BYTES);
        Cache {
            dir: root.join(crate::state::DIR).join("cache"),
            max_bytes,
        }
    }

    fn path(&self, id: &ObjectId) -> PathBuf {
        let h = id.to_hex();
        self.dir.join(&h[..2]).join(h)
    }

    pub fn has(&self, id: &ObjectId) -> bool {
        self.path(id).exists()
    }

    pub fn get(&self, id: &ObjectId) -> Option<Vec<u8>> {
        let path = self.path(id);
        let data = fs::read(&path).ok()?;
        if ObjectId::hash(&data) != *id {
            let _ = fs::remove_file(&path);
            return None;
        }
        if let Ok(f) = fs::File::options().write(true).open(&path) {
            let _ = f.set_times(FileTimes::new().set_modified(SystemTime::now()));
        }
        Some(data)
    }

    pub fn put(&self, id: &ObjectId, data: &[u8]) -> Result<()> {
        let path = self.path(id);
        if path.exists() {
            return Ok(());
        }
        let dir = path.parent().expect("cache path has a parent");
        fs::create_dir_all(dir)?;
        let tmp = tempfile::NamedTempFile::new_in(dir)?;
        fs::write(tmp.path(), data)?;
        tmp.persist(&path)?;
        Ok(())
    }

    /// Evicts least recently used blobs until under 80% of the limit.
    pub fn evict(&self) -> Result<()> {
        let mut entries = Vec::new();
        let mut total = 0u64;
        let Ok(shards) = fs::read_dir(&self.dir) else {
            return Ok(());
        };
        for shard in shards.flatten() {
            for f in fs::read_dir(shard.path())?.flatten() {
                let m = f.metadata()?;
                total += m.len();
                entries.push((m.modified()?, m.len(), f.path()));
            }
        }
        if total <= self.max_bytes {
            return Ok(());
        }
        entries.sort();
        let target = self.max_bytes / 10 * 8;
        for (_, len, path) in entries {
            if total <= target {
                break;
            }
            fs::remove_file(path)?;
            total -= len;
        }
        Ok(())
    }
}
