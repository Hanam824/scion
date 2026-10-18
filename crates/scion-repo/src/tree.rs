//! Tree application and diffing. Both only visit directories on the paths
//! that changed, so cost scales with the size of a change, not the repo.

use crate::{empty_tree_id, invalid, Repo};
use anyhow::Result;
use scion_core::{
    join, Change, FileMode, FileOp, HistEntry, HistTree, ObjectId, PathFilter, Tree, TreeEntry,
};
use std::collections::{BTreeMap, BTreeSet};

type Edit<'a> = (Vec<&'a str>, &'a FileOp);

/// Groups edits by first path component into (leaf edit, deeper edits).
fn group(edits: Vec<Edit<'_>>) -> BTreeMap<&str, (Option<&FileOp>, Vec<Edit<'_>>)> {
    let mut g: BTreeMap<&str, (Option<&FileOp>, Vec<Edit<'_>>)> = BTreeMap::new();
    for (comps, op) in edits {
        let Some((first, rest)) = comps.split_first() else {
            continue;
        };
        let e = g.entry(*first).or_default();
        if rest.is_empty() {
            e.0 = Some(op);
        } else {
            e.1.push((rest.to_vec(), op));
        }
    }
    g
}

fn edits(changes: &[Change]) -> Vec<Edit<'_>> {
    changes
        .iter()
        .map(|c| (c.path.split('/').collect(), &c.op))
        .collect()
}

/// Applies changes to the tree `root`, returning the new root tree id.
pub fn apply_changes(repo: &Repo, root: ObjectId, changes: &[Change]) -> Result<ObjectId> {
    Ok(apply_dir(repo, Some(root), "", edits(changes))?.unwrap_or_else(empty_tree_id))
}

/// Returns `None` when the directory ends up empty (empty dirs don't exist).
fn apply_dir(
    repo: &Repo,
    id: Option<ObjectId>,
    prefix: &str,
    edits: Vec<Edit<'_>>,
) -> Result<Option<ObjectId>> {
    let mut tree: Tree = match id {
        Some(id) => repo.get_tree(&id)?,
        None => Tree::default(),
    };
    for (name, (leaf, deeper)) in group(edits) {
        let path = join(prefix, name);
        let (file_before, mut dir_after) = match tree.entries.remove(name) {
            Some(TreeEntry::Dir { tree }) => (None, Some(tree)),
            Some(f) => (Some(f), None),
            None => (None, None),
        };
        if !deeper.is_empty() {
            if file_before.is_some() && !matches!(leaf, Some(FileOp::Delete)) {
                return Err(invalid(format!("{path} is a file, not a directory")));
            }
            dir_after = apply_dir(repo, dir_after, &path, deeper)?;
        }
        let entry = match leaf {
            None => file_before.or(dir_after.map(|tree| TreeEntry::Dir { tree })),
            Some(FileOp::Delete) => {
                if file_before.is_none() {
                    return Err(invalid(format!("cannot delete {path}: no such file")));
                }
                dir_after.map(|tree| TreeEntry::Dir { tree })
            }
            Some(FileOp::Upsert { blob, mode }) => {
                if dir_after.is_some() {
                    return Err(invalid(format!("{path} is a directory")));
                }
                Some(TreeEntry::File {
                    blob: *blob,
                    mode: *mode,
                })
            }
        };
        if let Some(e) = entry {
            tree.entries.insert(name.to_string(), e);
        }
    }
    if tree.entries.is_empty() {
        Ok(None)
    } else {
        Ok(Some(repo.put_tree(&tree)?))
    }
}

/// Applies changes to a `HistTree`, stamping every touched path with `cs`.
/// Assumes the changes were already validated by `apply_changes`.
pub fn apply_hist(
    repo: &Repo,
    root: ObjectId,
    changes: &[Change],
    cs: ObjectId,
) -> Result<ObjectId> {
    Ok(hist_dir(repo, Some(root), edits(changes), cs)?.unwrap_or_else(empty_tree_id))
}

fn hist_dir(
    repo: &Repo,
    id: Option<ObjectId>,
    edits: Vec<Edit<'_>>,
    cs: ObjectId,
) -> Result<Option<ObjectId>> {
    let mut tree: HistTree = match id {
        Some(id) => repo.get_tree(&id)?,
        None => HistTree::default(),
    };
    for (name, (leaf, deeper)) in group(edits) {
        let mut sub = tree.entries.remove(name).and_then(|e| e.sub);
        if !deeper.is_empty() {
            sub = hist_dir(repo, sub, deeper, cs)?;
        }
        let entry = match leaf {
            Some(FileOp::Upsert { .. }) => Some(HistEntry {
                last: cs,
                sub: None,
            }),
            Some(FileOp::Delete) | None => sub.map(|s| HistEntry {
                last: cs,
                sub: Some(s),
            }),
        };
        if let Some(e) = entry {
            tree.entries.insert(name.to_string(), e);
        }
    }
    if tree.entries.is_empty() {
        Ok(None)
    } else {
        Ok(Some(repo.put_tree(&tree)?))
    }
}

pub fn lookup_entry(repo: &Repo, root: ObjectId, path: &str) -> Result<Option<TreeEntry>> {
    let mut cur = root;
    let comps: Vec<&str> = path.split('/').collect();
    for (i, c) in comps.iter().enumerate() {
        let tree: Tree = repo.get_tree(&cur)?;
        match tree.entries.get(*c) {
            None => return Ok(None),
            Some(e) if i == comps.len() - 1 => return Ok(Some(*e)),
            Some(TreeEntry::Dir { tree }) => cur = *tree,
            Some(TreeEntry::File { .. }) => return Ok(None),
        }
    }
    Ok(None)
}

pub fn lookup_file(
    repo: &Repo,
    root: ObjectId,
    path: &str,
) -> Result<Option<(ObjectId, FileMode)>> {
    Ok(match lookup_entry(repo, root, path)? {
        Some(TreeEntry::File { blob, mode }) => Some((blob, mode)),
        _ => None,
    })
}

/// Last changeset that touched `path` (file or directory) in a `HistTree`.
pub fn last_change(repo: &Repo, hist_root: ObjectId, path: &str) -> Result<Option<ObjectId>> {
    let mut cur = Some(hist_root);
    let comps: Vec<&str> = path.split('/').collect();
    for (i, c) in comps.iter().enumerate() {
        let Some(id) = cur else { return Ok(None) };
        let tree: HistTree = repo.get_tree(&id)?;
        match tree.entries.get(*c) {
            None => return Ok(None),
            Some(e) if i == comps.len() - 1 => return Ok(Some(e.last)),
            Some(e) => cur = e.sub,
        }
    }
    Ok(None)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileDiff {
    pub path: String,
    pub old: Option<(ObjectId, FileMode)>,
    pub new: Option<(ObjectId, FileMode)>,
}

/// File-level differences between two root trees, skipping identical
/// subtrees and anything outside `filter`.
pub fn diff_trees(
    repo: &Repo,
    a: ObjectId,
    b: ObjectId,
    filter: &PathFilter,
) -> Result<Vec<FileDiff>> {
    let mut out = Vec::new();
    diff_dir(repo, Some(a), Some(b), "", filter, &mut out)?;
    Ok(out)
}

fn split_entry(e: Option<&TreeEntry>) -> (Option<(ObjectId, FileMode)>, Option<ObjectId>) {
    match e {
        Some(TreeEntry::File { blob, mode }) => (Some((*blob, *mode)), None),
        Some(TreeEntry::Dir { tree }) => (None, Some(*tree)),
        None => (None, None),
    }
}

fn diff_dir(
    repo: &Repo,
    a: Option<ObjectId>,
    b: Option<ObjectId>,
    prefix: &str,
    filter: &PathFilter,
    out: &mut Vec<FileDiff>,
) -> Result<()> {
    if a == b {
        return Ok(());
    }
    let load = |id: Option<ObjectId>| -> Result<Tree> {
        match id {
            Some(id) => repo.get_tree(&id),
            None => Ok(Tree::default()),
        }
    };
    let (ta, tb) = (load(a)?, load(b)?);
    let names: BTreeSet<&String> = ta.entries.keys().chain(tb.entries.keys()).collect();
    for name in names {
        let path = join(prefix, name);
        let (fa, da) = split_entry(ta.entries.get(name));
        let (fb, db) = split_entry(tb.entries.get(name));
        if fa != fb && filter.includes(&path) {
            out.push(FileDiff {
                path: path.clone(),
                old: fa,
                new: fb,
            });
        }
        if da != db && filter.may_contain(&path) {
            diff_dir(repo, da, db, &path, filter, out)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::testutil::*;
    use crate::{tree::*, RootTree};

    #[test]
    fn apply_and_diff() {
        let (repo, _d) = repo();
        let a = commit(
            &repo,
            &[],
            "a",
            &[
                ("src/a.rs", Some("a")),
                ("src/sub/b.rs", Some("b")),
                ("README", Some("r")),
            ],
        );
        let b = commit(
            &repo,
            &[a],
            "b",
            &[
                ("src/sub/b.rs", None),
                ("src/c.rs", Some("c")),
                ("README", Some("r2")),
            ],
        );
        assert_eq!(read(&repo, b, "src/a.rs").as_deref(), Some("a"));
        assert_eq!(read(&repo, b, "src/sub/b.rs"), None);
        let (ra, rb) = (
            repo.derive::<RootTree>(&a).unwrap(),
            repo.derive::<RootTree>(&b).unwrap(),
        );
        let root: Tree = repo.get_tree(&rb).unwrap();
        let src = match root.entries["src"] {
            TreeEntry::Dir { tree } => repo.get_tree::<Tree>(&tree).unwrap(),
            _ => panic!(),
        };
        assert!(
            !src.entries.contains_key("sub"),
            "empty directories are pruned"
        );

        let paths = |f: &PathFilter| {
            diff_trees(&repo, ra, rb, f)
                .unwrap()
                .into_iter()
                .map(|d| d.path)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            paths(&PathFilter::all()),
            vec!["README", "src/c.rs", "src/sub/b.rs"]
        );
        assert_eq!(paths(&PathFilter::new(["src/sub"])), vec!["src/sub/b.rs"]);
    }

    #[test]
    fn file_replaced_by_directory() {
        let (repo, _d) = repo();
        let a = commit(&repo, &[], "a", &[("x", Some("file"))]);
        let b = commit(&repo, &[a], "b", &[("x", None), ("x/y", Some("nested"))]);
        assert_eq!(read(&repo, b, "x/y").as_deref(), Some("nested"));
        let c = commit(
            &repo,
            &[b],
            "c",
            &[("x/y", None), ("x", Some("file again"))],
        );
        assert_eq!(read(&repo, c, "x").as_deref(), Some("file again"));
    }
}
