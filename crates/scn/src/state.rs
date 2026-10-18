use anyhow::{Context, Result};
use scion_core::{FileMode, ObjectId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;

/// Name of the per-working-copy metadata directory.
pub const DIR: &str = ".scn";

/// What the working copy believes is on disk, for cheap change detection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileState {
    pub blob: ObjectId,
    pub mode: FileMode,
    pub size: u64,
    pub mtime_ns: u64,
}

/// One side of a conflicted file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileVersion {
    pub blob: ObjectId,
    pub mode: FileMode,
}

/// A file left conflicted by `Workspace::start_resolution`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictEntry {
    /// "content", "binary", "delete/modify" or "modify/delete".
    pub kind: String,
    /// Version already landed (`None` = deleted there).
    pub target: Option<FileVersion>,
    /// Version from the changeset being merged (`None` = deleted there).
    pub source: Option<FileVersion>,
    /// Set by `resolve_conflict`; content conflicts also count as resolved
    /// once their markers are gone.
    #[serde(default)]
    pub resolved: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct State {
    pub server: String,
    /// Ref that `sync` and `land` target by default.
    pub track: String,
    /// Changeset the working copy is based on; `None` in an empty repo.
    pub base: Option<ObjectId>,
    /// Sparse directory prefixes; empty = full checkout.
    pub sparse: Vec<String>,
    pub files: BTreeMap<String, FileState>,
    /// Conflicts being resolved in the working copy, by path.
    #[serde(default)]
    pub conflicts: BTreeMap<String, ConflictEntry>,
    /// Suggested message for the commit that finishes a resolution.
    #[serde(default)]
    pub pending_message: Option<String>,
}

impl State {
    pub fn load(root: &Path) -> Result<Self> {
        let path = root.join(DIR).join("state.json");
        let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    pub fn save(&self, root: &Path) -> Result<()> {
        let dir = root.join(DIR);
        std::fs::create_dir_all(&dir)?;
        let mut tmp = tempfile::NamedTempFile::new_in(&dir)?;
        tmp.write_all(&serde_json::to_vec(self)?)?;
        tmp.persist(dir.join("state.json"))?;
        Ok(())
    }
}
