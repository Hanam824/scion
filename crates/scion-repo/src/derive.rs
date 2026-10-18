//! Generic derived data. Each kind is defined recursively from the values of
//! a changeset's parents and memoized in the `derived` table. Eager kinds
//! are computed when a changeset is created; lazy ones on first query.

use crate::{empty_tree_id, tree, Repo};
use anyhow::{anyhow, Result};
use scion_core::{Changeset, ObjectId};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::collections::HashMap;

pub trait Derivation {
    const KIND: &'static str;
    type Value: Serialize + DeserializeOwned + Clone;

    /// Computes the value for `cs` given its parents' values (same order as
    /// `cs.parents`).
    fn derive(
        repo: &Repo,
        id: &ObjectId,
        cs: &Changeset,
        parents: &[Self::Value],
    ) -> Result<Self::Value>;
}

impl Repo {
    pub fn load_derived<D: Derivation>(&self, id: &ObjectId) -> Result<Option<D::Value>> {
        match self.meta().get_derived(D::KIND, &id.to_hex())? {
            Some(b) => Ok(Some(serde_json::from_slice(&b)?)),
            None => Ok(None),
        }
    }

    pub fn store_derived<D: Derivation>(&self, id: &ObjectId, v: &D::Value) -> Result<()> {
        self.meta()
            .put_derived(D::KIND, &id.to_hex(), &serde_json::to_vec(v)?)
    }

    /// Returns the derived value for `id`, computing any missing ancestors
    /// oldest-first. Iterative, so deep histories cannot overflow the stack.
    pub fn derive<D: Derivation>(&self, id: &ObjectId) -> Result<D::Value> {
        if let Some(v) = self.load_derived::<D>(id)? {
            return Ok(v);
        }
        let mut done: HashMap<ObjectId, D::Value> = HashMap::new();
        let mut stack: Vec<(ObjectId, Option<Changeset>)> = vec![(*id, None)];
        while let Some((cur, cs)) = stack.pop() {
            if done.contains_key(&cur) {
                continue;
            }
            match cs {
                None => {
                    let cs = self.get_changeset(&cur)?;
                    let mut missing = Vec::new();
                    for p in &cs.parents {
                        if done.contains_key(p) {
                            continue;
                        }
                        match self.load_derived::<D>(p)? {
                            Some(v) => {
                                done.insert(*p, v);
                            }
                            None => missing.push(*p),
                        }
                    }
                    stack.push((cur, Some(cs)));
                    stack.extend(missing.into_iter().map(|p| (p, None)));
                }
                Some(cs) => {
                    let parents = cs
                        .parents
                        .iter()
                        .map(|p| {
                            done.get(p)
                                .cloned()
                                .ok_or_else(|| anyhow!("derivation parent {p} missing"))
                        })
                        .collect::<Result<Vec<_>>>()?;
                    let v = D::derive(self, &cur, &cs, &parents)?;
                    self.store_derived::<D>(&cur, &v)?;
                    done.insert(cur, v);
                }
            }
        }
        done.remove(id)
            .ok_or_else(|| anyhow!("derivation of {id} produced no value"))
    }
}

/// Root `Tree` id of the full snapshot at a changeset. Eager.
pub struct RootTree;

impl Derivation for RootTree {
    const KIND: &'static str = "root_tree";
    type Value = ObjectId;

    fn derive(
        repo: &Repo,
        _id: &ObjectId,
        cs: &Changeset,
        parents: &[ObjectId],
    ) -> Result<ObjectId> {
        let base = parents.first().copied().unwrap_or_else(empty_tree_id);
        tree::apply_changes(repo, base, &cs.changes)
    }
}

/// Root `HistTree` id (last-modifying changeset per path). Eager.
pub struct HistRoot;

impl Derivation for HistRoot {
    const KIND: &'static str = "hist_tree";
    type Value = ObjectId;

    fn derive(
        repo: &Repo,
        id: &ObjectId,
        cs: &Changeset,
        parents: &[ObjectId],
    ) -> Result<ObjectId> {
        let base = parents.first().copied().unwrap_or_else(empty_tree_id);
        tree::apply_hist(repo, base, &cs.changes, *id)
    }
}

/// Generation number plus first-parent skip list: `skips[k]` is the
/// ancestor 2^k first-parent steps back. Eager.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GraphInfo {
    pub generation: u64,
    pub skips: Vec<ObjectId>,
}

pub struct Graph;

impl Derivation for Graph {
    const KIND: &'static str = "graph";
    type Value = GraphInfo;

    fn derive(
        repo: &Repo,
        _id: &ObjectId,
        cs: &Changeset,
        parents: &[GraphInfo],
    ) -> Result<GraphInfo> {
        let generation = 1 + parents.iter().map(|p| p.generation).max().unwrap_or(0);
        let mut skips = Vec::new();
        if let Some(p0) = cs.parents.first() {
            skips.push(*p0);
            // `info` is the GraphInfo of skips[k]; its own skips[k] is 2^(k+1) back.
            let mut info = parents[0].clone();
            while let Some(next) = info.skips.get(skips.len() - 1).copied() {
                skips.push(next);
                info = repo.derive::<Graph>(&next)?;
            }
        }
        Ok(GraphInfo { generation, skips })
    }
}
