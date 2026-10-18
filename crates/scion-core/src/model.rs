use crate::{path::validate_path, ObjectId};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};

/// Content-addressed object with a canonical (deterministic) JSON encoding.
/// Struct field order is fixed and maps are `BTreeMap`s, so equal values
/// always encode to equal bytes.
pub trait Object: Serialize + DeserializeOwned + Sized {
    fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("object serialization cannot fail")
    }

    fn decode(bytes: &[u8]) -> serde_json::Result<Self> {
        serde_json::from_slice(bytes)
    }

    fn id(&self) -> ObjectId {
        ObjectId::hash(&self.encode())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileMode {
    #[default]
    Regular,
    Executable,
    Symlink,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "lowercase")]
pub enum TreeEntry {
    File { blob: ObjectId, mode: FileMode },
    Dir { tree: ObjectId },
}

/// One directory level. Trees are never stored in changesets; they are
/// derived data (see `RootTree` in scion-repo).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tree {
    pub entries: BTreeMap<String, TreeEntry>,
}

impl Object for Tree {}

/// Mirror of `Tree` recording, for every file and directory, the last
/// changeset (along first parents) that touched it. Powers `log <path>` and
/// blame without scanning history.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistTree {
    pub entries: BTreeMap<String, HistEntry>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistEntry {
    pub last: ObjectId,
    /// Subtree for directories; `None` for files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sub: Option<ObjectId>,
}

impl Object for HistTree {}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum FileOp {
    Upsert { blob: ObjectId, mode: FileMode },
    Delete,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    pub path: String,
    #[serde(flatten)]
    pub op: FileOp,
}

/// Unit of history: the files that changed relative to `parents[0]`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Changeset {
    pub parents: Vec<ObjectId>,
    pub author: String,
    pub message: String,
    pub timestamp: i64,
    pub changes: Vec<Change>,
}

impl Object for Changeset {}

impl Changeset {
    /// Sorts changes into canonical order and rejects malformed input.
    pub fn normalize(&mut self) -> Result<(), String> {
        self.changes.sort_by(|a, b| a.path.cmp(&b.path));
        for w in self.changes.windows(2) {
            if w[0].path == w[1].path {
                return Err(format!("duplicate change for {}", w[0].path));
            }
        }
        for c in &self.changes {
            validate_path(&c.path)?;
        }
        let mut seen = HashSet::new();
        if !self.parents.iter().all(|p| seen.insert(*p)) {
            return Err("duplicate parent".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changeset_id_is_canonical() {
        let blob = ObjectId::hash(b"x");
        let up = FileOp::Upsert {
            blob,
            mode: FileMode::Regular,
        };
        let mk = |paths: &[&str]| {
            let mut cs = Changeset {
                parents: vec![],
                author: "a".into(),
                message: "m".into(),
                timestamp: 1,
                changes: paths
                    .iter()
                    .map(|p| Change {
                        path: p.to_string(),
                        op: up,
                    })
                    .collect(),
            };
            cs.normalize().unwrap();
            cs
        };
        assert_eq!(mk(&["b", "a"]).id(), mk(&["a", "b"]).id());
        let cs = mk(&["a"]);
        assert_eq!(Changeset::decode(&cs.encode()).unwrap(), cs);
    }

    #[test]
    fn rejects_duplicates() {
        let mut cs = Changeset {
            parents: vec![],
            author: String::new(),
            message: String::new(),
            timestamp: 0,
            changes: vec![
                Change {
                    path: "a".into(),
                    op: FileOp::Delete,
                },
                Change {
                    path: "a".into(),
                    op: FileOp::Delete,
                },
            ],
        };
        assert!(cs.normalize().is_err());
    }
}
