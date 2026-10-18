//! Server-side three-way merge, rebase, revert, and lock-free landing onto refs.

use crate::graph::{is_ancestor, merge_base};
use crate::tree::{apply_changes, diff_trees, FileDiff};
use crate::{empty_tree_id, invalid, Repo, RootTree};
use anyhow::{bail, Result};
use scion_core::text::as_text;
use scion_core::{
    merge_text, Change, Changeset, FileMode, FileOp, MergeOutcome, ObjectId, PathFilter,
};
use serde::Serialize;
use std::collections::HashMap;

/// How many times landing retries after losing a ref compare-and-swap race.
const MAX_LAND_ATTEMPTS: u32 = 32;

pub type FileVersion = Option<(ObjectId, FileMode)>;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Conflict {
    pub path: String,
    pub kind: String,
    /// For content conflicts, the whole file with conflict markers.
    pub detail: String,
    /// The file on the target side (`None` = deleted there).
    pub target: FileVersion,
    /// The file on the source side (`None` = deleted there).
    pub source: FileVersion,
}

/// A merge that could not complete. `resolved` holds the changes (relative
/// to `target`) for every file that did merge cleanly, so a client can
/// reconstruct the merge in a working copy and finish it by hand.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conflicted {
    pub target: ObjectId,
    pub conflicts: Vec<Conflict>,
    pub resolved: Vec<Change>,
}

pub type Outcome = std::result::Result<ObjectId, Conflicted>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Strategy {
    /// Replay the source's changes as one new changeset on top of the target.
    Rebase,
    /// Create a two-parent merge changeset.
    Merge,
}

#[derive(Debug)]
pub enum LandResult {
    UpToDate(ObjectId),
    Landed {
        id: ObjectId,
        fast_forward: bool,
        attempts: u32,
    },
    Conflicts(Conflicted),
}

/// Result of a three-way tree merge.
pub struct Merged {
    pub changes: Vec<Change>,
    pub conflicts: Vec<Conflict>,
}

fn to_change(path: &str, new: FileVersion) -> Change {
    let op = match new {
        Some((blob, mode)) => FileOp::Upsert { blob, mode },
        None => FileOp::Delete,
    };
    Change {
        path: path.to_string(),
        op,
    }
}

/// Merges root trees: the changes to apply on top of `ours` to incorporate
/// everything `theirs` changed relative to `base`, plus any conflicts.
pub fn three_way(repo: &Repo, base: ObjectId, ours: ObjectId, theirs: ObjectId) -> Result<Merged> {
    let all = PathFilter::all();
    let ours_diff: HashMap<String, FileDiff> = diff_trees(repo, base, ours, &all)?
        .into_iter()
        .map(|d| (d.path.clone(), d))
        .collect();
    let mut changes = Vec::new();
    let mut conflicts = Vec::new();
    for t in diff_trees(repo, base, theirs, &all)? {
        let Some(o) = ours_diff.get(&t.path) else {
            changes.push(to_change(&t.path, t.new));
            continue;
        };
        if o.new == t.new {
            continue;
        }
        let conflict = |kind: &str, detail: String| Conflict {
            path: t.path.clone(),
            kind: kind.to_string(),
            detail,
            target: o.new,
            source: t.new,
        };
        match (o.new, t.new) {
            (Some((ob, om)), Some((tb, tm))) => {
                let mode = if Some(om) == t.old.map(|x| x.1) {
                    tm
                } else {
                    om
                };
                if ob == tb {
                    changes.push(to_change(&t.path, Some((ob, mode))));
                    continue;
                }
                let base_bytes = match t.old {
                    Some((bb, _)) => repo.get_blob(&bb)?,
                    None => Vec::new(),
                };
                let (ours_bytes, theirs_bytes) = (repo.get_blob(&ob)?, repo.get_blob(&tb)?);
                match (
                    as_text(&base_bytes),
                    as_text(&ours_bytes),
                    as_text(&theirs_bytes),
                ) {
                    (Some(b), Some(o2), Some(t2)) => {
                        match merge_text(b, o2, t2, ("target", "source")) {
                            MergeOutcome::Clean(text) => {
                                let id = repo.put_blob(text.as_bytes())?;
                                if id != ob || mode != om {
                                    changes.push(to_change(&t.path, Some((id, mode))));
                                }
                            }
                            MergeOutcome::Conflict(marked) => {
                                conflicts.push(conflict("content", marked))
                            }
                        }
                    }
                    _ => conflicts.push(conflict(
                        "binary",
                        "binary file changed on both sides".into(),
                    )),
                }
            }
            (None, Some(_)) => conflicts.push(conflict(
                "delete/modify",
                "deleted on target, modified on source".into(),
            )),
            (Some(_), None) => conflicts.push(conflict(
                "modify/delete",
                "modified on target, deleted on source".into(),
            )),
            (None, None) => {}
        }
    }
    if conflicts.is_empty() {
        if let Err(e) = apply_changes(repo, ours, &changes) {
            conflicts.push(Conflict {
                path: String::new(),
                kind: "file/directory".into(),
                detail: e.to_string(),
                target: None,
                source: None,
            });
        }
    }
    Ok(Merged { changes, conflicts })
}

/// Three-way merges `source` into `target` (as commits); `Err` on conflicts.
fn merged_changes(
    repo: &Repo,
    target: ObjectId,
    source: ObjectId,
) -> Result<std::result::Result<Vec<Change>, Conflicted>> {
    let base_tree = match merge_base(repo, target, source)? {
        Some(b) => repo.derive::<RootTree>(&b)?,
        None => empty_tree_id(),
    };
    let m = three_way(
        repo,
        base_tree,
        repo.derive::<RootTree>(&target)?,
        repo.derive::<RootTree>(&source)?,
    )?;
    Ok(finish(target, m))
}

fn finish(target: ObjectId, m: Merged) -> std::result::Result<Vec<Change>, Conflicted> {
    if m.conflicts.is_empty() {
        Ok(m.changes)
    } else {
        Err(Conflicted {
            target,
            conflicts: m.conflicts,
            resolved: m.changes,
        })
    }
}

/// Replays `source` (everything since its merge base with `onto`) as a
/// single changeset on top of `onto`, keeping the source's metadata so the
/// result is deterministic.
pub fn rebase(repo: &Repo, source: ObjectId, onto: ObjectId) -> Result<Outcome> {
    if is_ancestor(repo, source, onto)? {
        return Ok(Ok(onto));
    }
    if is_ancestor(repo, onto, source)? {
        return Ok(Ok(source));
    }
    let changes = match merged_changes(repo, onto, source)? {
        Ok(c) => c,
        Err(c) => return Ok(Err(c)),
    };
    let src = repo.get_changeset(&source)?;
    let cs = Changeset {
        parents: vec![onto],
        author: src.author,
        message: src.message,
        timestamp: src.timestamp,
        changes,
    };
    Ok(Ok(repo.create_changeset(cs)?))
}

fn merge_commit(
    repo: &Repo,
    target: ObjectId,
    source: ObjectId,
    author: &str,
    message: &str,
    timestamp: i64,
) -> Result<Outcome> {
    let changes = match merged_changes(repo, target, source)? {
        Ok(c) => c,
        Err(c) => return Ok(Err(c)),
    };
    let cs = Changeset {
        parents: vec![target, source],
        author: author.to_string(),
        message: message.to_string(),
        timestamp,
        changes,
    };
    Ok(Ok(repo.create_changeset(cs)?))
}

/// Creates a changeset on top of `onto` that undoes what `cs` changed
/// relative to its first parent.
pub fn revert(
    repo: &Repo,
    cs: ObjectId,
    onto: ObjectId,
    author: &str,
    timestamp: i64,
) -> Result<Outcome> {
    let c = repo.get_changeset(&cs)?;
    let before = match c.parents.first() {
        Some(p) => repo.derive::<RootTree>(p)?,
        None => empty_tree_id(),
    };
    let m = three_way(
        repo,
        repo.derive::<RootTree>(&cs)?,
        repo.derive::<RootTree>(&onto)?,
        before,
    )?;
    let changes = match finish(onto, m) {
        Ok(c) => c,
        Err(c) => return Ok(Err(c)),
    };
    if changes.is_empty() {
        return Err(invalid(format!("{} is already reverted", cs.short())));
    }
    let summary = c.message.lines().next().unwrap_or_default();
    Ok(Ok(repo.create_changeset(Changeset {
        parents: vec![onto],
        author: author.to_string(),
        message: format!("Revert \"{summary}\"\n\nThis reverts changeset {cs}."),
        timestamp,
        changes,
    })?))
}

/// Lands `source` onto `target_ref` without any lock: compute the result
/// against the ref's current value, then compare-and-swap. If another land
/// won the race, recompute against the new value and retry. There is no
/// global merge queue; correctness of the combined result is left to
/// post-merge testing, as in the article.
pub fn land(
    repo: &Repo,
    target_ref: &str,
    source: ObjectId,
    strategy: Strategy,
    author: &str,
    message: &str,
    timestamp: i64,
) -> Result<LandResult> {
    repo.get_changeset(&source)?;
    for attempt in 1..=MAX_LAND_ATTEMPTS {
        let Some(cur) = repo.meta().get_ref(target_ref)? else {
            if repo.meta().cas_ref(target_ref, None, Some(source))? {
                return Ok(LandResult::Landed {
                    id: source,
                    fast_forward: true,
                    attempts: attempt,
                });
            }
            continue;
        };
        if is_ancestor(repo, source, cur)? {
            return Ok(LandResult::UpToDate(cur));
        }
        let (new, fast_forward) = if is_ancestor(repo, cur, source)? {
            (source, true)
        } else {
            let r = match strategy {
                Strategy::Rebase => rebase(repo, source, cur)?,
                Strategy::Merge => merge_commit(repo, cur, source, author, message, timestamp)?,
            };
            match r {
                Ok(id) => (id, false),
                Err(c) => return Ok(LandResult::Conflicts(c)),
            }
        };
        if repo.meta().cas_ref(target_ref, Some(cur), Some(new))? {
            return Ok(LandResult::Landed {
                id: new,
                fast_forward,
                attempts: attempt,
            });
        }
    }
    bail!("could not land onto {target_ref} after {MAX_LAND_ATTEMPTS} attempts (ref contention)")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;

    #[test]
    fn rebase_merges_disjoint_lines() {
        let (repo, _d) = repo();
        let base = commit(&repo, &[], "base", &[("f", Some("1\n2\n3\n4\n5\n"))]);
        let main = commit(&repo, &[base], "main", &[("f", Some("one\n2\n3\n4\n5\n"))]);
        let feat = commit(
            &repo,
            &[base],
            "feat",
            &[("f", Some("1\n2\n3\n4\nfive\n")), ("g", Some("new"))],
        );
        let r = rebase(&repo, feat, main).unwrap().unwrap();
        assert_eq!(read(&repo, r, "f").as_deref(), Some("one\n2\n3\n4\nfive\n"));
        assert_eq!(read(&repo, r, "g").as_deref(), Some("new"));
        let cs = repo.get_changeset(&r).unwrap();
        assert_eq!((cs.parents, cs.message.as_str()), (vec![main], "feat"));
    }

    #[test]
    fn land_fast_forward_rebase_and_conflict() {
        let (repo, _d) = repo();
        let base = commit(&repo, &[], "base", &[("f", Some("x\n"))]);
        let land_ = |src| land(&repo, "main", src, Strategy::Rebase, "t", "m", 0).unwrap();
        assert!(matches!(
            land_(base),
            LandResult::Landed {
                fast_forward: true,
                ..
            }
        ));
        let a = commit(&repo, &[base], "a", &[("a", Some("a"))]);
        assert!(matches!(land_(a), LandResult::Landed { id, fast_forward: true, .. } if id == a));
        assert!(matches!(land_(base), LandResult::UpToDate(_)));

        let b = commit(&repo, &[base], "b", &[("b", Some("b"))]);
        let LandResult::Landed {
            id,
            fast_forward: false,
            ..
        } = land_(b)
        else {
            panic!()
        };
        assert_eq!(repo.meta().get_ref("main").unwrap(), Some(id));
        assert_eq!(read(&repo, id, "a").as_deref(), Some("a"));
        assert_eq!(read(&repo, id, "b").as_deref(), Some("b"));

        let c1 = commit(&repo, &[id], "c1", &[("f", Some("y\n"))]);
        let c2 = commit(&repo, &[id], "c2", &[("f", Some("z\n"))]);
        assert!(matches!(land_(c1), LandResult::Landed { .. }));
        let LandResult::Conflicts(c) = land_(c2) else {
            panic!()
        };
        assert_eq!(c.target, c1);
        let f = &c.conflicts[0];
        assert_eq!((f.path.as_str(), f.kind.as_str()), ("f", "content"));
        assert!(f.target.is_some() && f.source.is_some());
        assert_eq!(
            repo.meta().get_ref("main").unwrap(),
            Some(c1),
            "ref unchanged on conflict"
        );
    }

    #[test]
    fn merge_strategy_creates_two_parents() {
        let (repo, _d) = repo();
        let base = commit(&repo, &[], "base", &[("f", Some("x"))]);
        repo.meta().cas_ref("main", None, Some(base)).unwrap();
        let a = commit(&repo, &[base], "a", &[("a", Some("a"))]);
        land(&repo, "main", a, Strategy::Rebase, "t", "m", 0).unwrap();
        let b = commit(&repo, &[base], "b", &[("b", Some("b"))]);
        let LandResult::Landed { id, .. } =
            land(&repo, "main", b, Strategy::Merge, "t", "merge b", 0).unwrap()
        else {
            panic!()
        };
        assert_eq!(repo.get_changeset(&id).unwrap().parents, vec![a, b]);
        assert_eq!(read(&repo, id, "a").as_deref(), Some("a"));
        assert_eq!(read(&repo, id, "b").as_deref(), Some("b"));
    }

    #[test]
    fn modify_delete_conflicts() {
        let (repo, _d) = repo();
        let base = commit(&repo, &[], "base", &[("f", Some("x"))]);
        let del = commit(&repo, &[base], "del", &[("f", None)]);
        let modi = commit(&repo, &[base], "mod", &[("f", Some("y"))]);
        let Err(c) = rebase(&repo, modi, del).unwrap() else {
            panic!()
        };
        assert_eq!(c.conflicts[0].kind, "delete/modify");
        assert_eq!(c.conflicts[0].target, None);
    }

    #[test]
    fn conflicts_carry_clean_merges() {
        let (repo, _d) = repo();
        let base = commit(
            &repo,
            &[],
            "base",
            &[("f", Some("x\n")), ("g", Some("1\n2\n3\n"))],
        );
        let main = commit(
            &repo,
            &[base],
            "main",
            &[("f", Some("main\n")), ("g", Some("one\n2\n3\n"))],
        );
        let feat = commit(
            &repo,
            &[base],
            "feat",
            &[
                ("f", Some("feat\n")),
                ("g", Some("1\n2\nthree\n")),
                ("h", Some("new")),
            ],
        );
        let Err(c) = rebase(&repo, feat, main).unwrap() else {
            panic!("expected conflict")
        };
        assert_eq!(c.conflicts.len(), 1);
        let paths: Vec<&str> = c.resolved.iter().map(|ch| ch.path.as_str()).collect();
        assert_eq!(
            paths,
            ["g", "h"],
            "clean merges are returned alongside conflicts"
        );
        let FileOp::Upsert { blob, .. } = c.resolved[0].op else {
            panic!()
        };
        assert_eq!(repo.get_blob(&blob).unwrap(), b"one\n2\nthree\n");
    }

    #[test]
    fn revert_undoes_a_changeset() {
        let (repo, _d) = repo();
        let a = commit(&repo, &[], "a", &[("f", Some("1\n2\n3\n"))]);
        let b = commit(
            &repo,
            &[a],
            "b",
            &[("f", Some("1\nTWO\n3\n")), ("g", Some("g"))],
        );
        let c = commit(&repo, &[b], "c", &[("f", Some("1\nTWO\n3\nfour\n"))]);
        let r = revert(&repo, b, c, "t", 0).unwrap().unwrap();
        assert_eq!(read(&repo, r, "f").as_deref(), Some("1\n2\n3\nfour\n"));
        assert_eq!(read(&repo, r, "g"), None);
        assert!(repo
            .get_changeset(&r)
            .unwrap()
            .message
            .starts_with("Revert \"b\""));
        assert!(revert(&repo, b, r, "t", 0).is_err(), "already reverted");
    }
}
