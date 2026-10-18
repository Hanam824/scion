//! Commit-graph queries over generation numbers and skip lists.

use crate::{tree, Graph, HistRoot, Repo};
use anyhow::Result;
use scion_core::{Changeset, ObjectId};
use std::collections::{BinaryHeap, HashMap, HashSet};

fn generation(repo: &Repo, id: &ObjectId) -> Result<u64> {
    Ok(repo.derive::<Graph>(id)?.generation)
}

/// The changeset `n` first-parent steps behind `id`, in O(log n) hops.
pub fn first_parent_ancestor(
    repo: &Repo,
    mut id: ObjectId,
    mut n: u64,
) -> Result<Option<ObjectId>> {
    while n > 0 {
        let info = repo.derive::<Graph>(&id)?;
        if info.skips.is_empty() {
            return Ok(None);
        }
        let k = ((63 - n.leading_zeros()) as usize).min(info.skips.len() - 1);
        id = info.skips[k];
        n -= 1 << k;
    }
    Ok(Some(id))
}

/// A best common ancestor of `a` and `b`. Visits nodes in decreasing
/// generation order: a node's reachability flags are final when popped
/// (all its children have higher generations), so the first node reachable
/// from both sides has the highest generation of any common ancestor.
pub fn merge_base(repo: &Repo, a: ObjectId, b: ObjectId) -> Result<Option<ObjectId>> {
    if a == b {
        return Ok(Some(a));
    }
    const A: u8 = 1;
    const B: u8 = 2;
    let mut flags: HashMap<ObjectId, u8> = HashMap::from([(a, A), (b, B)]);
    let mut heap = BinaryHeap::from([(generation(repo, &a)?, a), (generation(repo, &b)?, b)]);
    let mut seen = HashSet::new();
    while let Some((_, id)) = heap.pop() {
        if !seen.insert(id) {
            continue;
        }
        let f = flags[&id];
        if f == A | B {
            return Ok(Some(id));
        }
        for p in repo.get_changeset(&id)?.parents {
            let e = flags.entry(p).or_insert(0);
            let fresh = *e == 0;
            *e |= f;
            if fresh {
                heap.push((generation(repo, &p)?, p));
            }
        }
    }
    Ok(None)
}

/// How many changesets are reachable from `a` but not `b` (ahead) and from
/// `b` but not `a` (behind), counting at most `limit` of each. Walks in
/// decreasing generation order like `merge_base`, so flags are final when a
/// node is popped, and stops once only shared ancestry is left to visit.
pub fn ahead_behind(repo: &Repo, a: ObjectId, b: ObjectId, limit: u64) -> Result<AheadBehind> {
    const A: u8 = 1;
    const B: u8 = 2;
    const BOTH: u8 = A | B;
    let mut out = AheadBehind::default();
    if a == b {
        return Ok(out);
    }
    let mut flags: HashMap<ObjectId, u8> = HashMap::from([(a, A), (b, B)]);
    let mut heap = BinaryHeap::from([(generation(repo, &a)?, a), (generation(repo, &b)?, b)]);
    let mut popped = HashSet::new();
    // Queued nodes not yet known to be shared; the walk ends when this is 0.
    let mut pending = 2usize;
    while pending > 0 {
        let Some((_, id)) = heap.pop() else { break };
        if !popped.insert(id) {
            continue;
        }
        let f = flags[&id];
        if f != BOTH {
            pending -= 1;
            if f == A {
                out.ahead += 1;
            } else {
                out.behind += 1;
            }
            if out.ahead > limit || out.behind > limit {
                out.truncated = true;
                out.ahead = out.ahead.min(limit);
                out.behind = out.behind.min(limit);
                break;
            }
        }
        for p in repo.get_changeset(&id)?.parents {
            let e = flags.entry(p).or_insert(0);
            let (old, new) = (*e, *e | f);
            *e = new;
            if old == 0 {
                heap.push((generation(repo, &p)?, p));
                if new != BOTH {
                    pending += 1;
                }
            } else if old != BOTH && new == BOTH && !popped.contains(&p) {
                pending -= 1;
            }
        }
    }
    Ok(out)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AheadBehind {
    pub ahead: u64,
    pub behind: u64,
    pub truncated: bool,
}

/// Whether `a` is `b` or one of its ancestors.
pub fn is_ancestor(repo: &Repo, a: ObjectId, b: ObjectId) -> Result<bool> {
    Ok(merge_base(repo, a, b)? == Some(a))
}

/// History from `start`, newest first. With a path, follows the
/// last-modifying changeset recorded in the `HistTree` along first parents,
/// so cost is proportional to the number of results, not history length.
pub fn log(
    repo: &Repo,
    start: ObjectId,
    path: Option<&str>,
    limit: usize,
) -> Result<Vec<(ObjectId, Changeset)>> {
    let mut out = Vec::new();
    match path.filter(|p| !p.is_empty() && *p != ".") {
        Some(path) => {
            let mut cur = Some(start);
            while let Some(c) = cur {
                if out.len() >= limit {
                    break;
                }
                let hist = repo.derive::<HistRoot>(&c)?;
                let Some(last) = tree::last_change(repo, hist, path)? else {
                    break;
                };
                let cs = repo.get_changeset(&last)?;
                cur = cs.parents.first().copied();
                out.push((last, cs));
            }
        }
        None => {
            let mut heap = BinaryHeap::from([(generation(repo, &start)?, start)]);
            let mut seen = HashSet::from([start]);
            while let Some((_, id)) = heap.pop() {
                if out.len() >= limit {
                    break;
                }
                let cs = repo.get_changeset(&id)?;
                for p in &cs.parents {
                    if seen.insert(*p) {
                        heap.push((generation(repo, p)?, *p));
                    }
                }
                out.push((id, cs));
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;

    #[test]
    fn deep_linear_history() {
        let (repo, _d) = repo();
        let mut ids = vec![commit(&repo, &[], "0", &[("f", Some("0"))])];
        for i in 1..3000 {
            let c = commit(
                &repo,
                &[*ids.last().unwrap()],
                &i.to_string(),
                &[("f", Some(&i.to_string()))],
            );
            ids.push(c);
        }
        let tip = *ids.last().unwrap();
        assert_eq!(
            first_parent_ancestor(&repo, tip, 2999).unwrap(),
            Some(ids[0])
        );
        assert_eq!(
            first_parent_ancestor(&repo, tip, 1234).unwrap(),
            Some(ids[2999 - 1234])
        );
        assert_eq!(first_parent_ancestor(&repo, tip, 3000).unwrap(), None);
        assert_eq!(repo.derive::<Graph>(&tip).unwrap().generation, 3000);
        assert_eq!(log(&repo, tip, Some("f"), 5).unwrap().len(), 5);
    }

    #[test]
    fn ahead_and_behind() {
        let (repo, _d) = repo();
        let root = commit(&repo, &[], "root", &[("a", Some("1"))]);
        let m1 = commit(&repo, &[root], "m1", &[("a", Some("2"))]);
        let m2 = commit(&repo, &[m1], "m2", &[("a", Some("3"))]);
        let f1 = commit(&repo, &[root], "f1", &[("b", Some("1"))]);
        let ab = |x, y| {
            let r = ahead_behind(&repo, x, y, 100).unwrap();
            (r.ahead, r.behind)
        };
        assert_eq!(ab(f1, m2), (1, 2));
        assert_eq!(ab(m2, f1), (2, 1));
        assert_eq!(ab(m2, root), (2, 0));
        assert_eq!(ab(m2, m2), (0, 0));
        let merge = commit(&repo, &[m2, f1], "merge", &[("b", Some("1"))]);
        assert_eq!(ab(merge, m2), (2, 0), "merge commit plus the merged-in f1");
        let r = ahead_behind(&repo, m2, root, 1).unwrap();
        assert!(r.truncated && r.ahead == 1);
    }

    #[test]
    fn merge_base_and_log() {
        let (repo, _d) = repo();
        let root = commit(&repo, &[], "root", &[("a", Some("1")), ("b", Some("1"))]);
        let left = commit(&repo, &[root], "left", &[("a", Some("2"))]);
        let right = commit(&repo, &[root], "right", &[("b", Some("2"))]);
        let left2 = commit(&repo, &[left], "left2", &[("a", Some("3"))]);
        assert_eq!(merge_base(&repo, left2, right).unwrap(), Some(root));
        assert!(is_ancestor(&repo, root, left2).unwrap());
        assert!(!is_ancestor(&repo, right, left2).unwrap());

        let msgs = |p: Option<&str>| {
            log(&repo, left2, p, 10)
                .unwrap()
                .into_iter()
                .map(|(_, c)| c.message)
                .collect::<Vec<_>>()
        };
        assert_eq!(msgs(None), vec!["left2", "left", "root"]);
        assert_eq!(msgs(Some("a")), vec!["left2", "left", "root"]);
        assert_eq!(msgs(Some("b")), vec!["root"]);
    }
}
